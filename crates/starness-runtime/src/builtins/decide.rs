//! Built-in `@starness/decide` runtime driver powered by `decision-rs`.
//!
//! Executes structured `noul`, `choice`, and `score` decision schemas across TypeSafe System One
//! (`jev`), Google Gemini Decisions, OpenAI Decisions, OpenRouter Jev, Cloudflare Clef, and
//! custom/self-hosted HTTP endpoints, with `@starness/secrets` resolution and trace redaction.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context, bail};
use base64::Engine as _;
use decision_rs::{
    Answer, Auth, Client, CloudflareAccountId, Distribution, Endpoint, HttpClient, Image,
    ImageDetail, Input, Model, ModelId, Provider, Questions, Settings,
};
use indexmap::IndexMap;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value, json};

use crate::builtins::secrets::{
    SecretScrubber, parse_secret_descriptor, resolve_secret_descriptor, resolve_secrets_in_value,
};
use crate::trace::{ProgressKind, report_progress};

fn shared_decision_http() -> anyhow::Result<&'static HttpClient> {
    static POOL: OnceLock<Result<HttpClient, String>> = OnceLock::new();
    match POOL.get_or_init(|| HttpClient::new().map_err(|e| e.to_string())) {
        Ok(pool) => Ok(pool),
        Err(msg) => bail!("failed initializing decision-rs HTTP pool: {msg}"),
    }
}

struct QuestionMeta {
    kind: String,
    threshold: f64,
    optional: bool,
    levels: Vec<String>,
}

/// Executes an `invoke_decision(...)` specification from `@starness/decide`.
pub async fn execute_decision(_ctx: Value, input: Value) -> anyhow::Result<Value> {
    let mut scrubber = SecretScrubber::new();
    let res = execute_decision_inner(&input, &mut scrubber).await;
    scrubber.scrub_result(res)
}

async fn execute_decision_inner(
    input: &Value,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<Value> {
    let obj = input
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("`@starness/decide` input must be an object"))?;

    let provider_obj = obj
        .get("provider")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("`invoke_decision` missing `provider` record"))?;

    let provider_str = provider_obj
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("typesafe");
    let model_str = provider_obj
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("DecisionProvider missing `model`"))?;

    let endpoint_opt = provider_obj
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let resolved_api_key =
        resolve_optional_secret_value(provider_obj.get("api_key"), "api_key", scrubber)?;
    let resolved_account_id =
        resolve_optional_secret_value(provider_obj.get("account_id"), "account_id", scrubber)?;

    let timeout_ms = obj
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(15_000);
    if !(1..=120_000).contains(&timeout_ms) {
        bail!("`invoke_decision` `timeout_ms` must be in 1..=120000, got {timeout_ms}");
    }

    let mut settings = Settings::default();
    settings.timeout_ms = timeout_ms;
    let pool = shared_decision_http()?.clone();

    let openai_key_clone = resolved_api_key.clone();
    let client = build_decision_client(
        provider_str,
        model_str,
        endpoint_opt,
        resolved_api_key,
        resolved_account_id,
        pool,
        settings,
    )?;

    let raw_evidence = obj
        .get("evidence")
        .ok_or_else(|| anyhow::anyhow!("`invoke_decision` missing `evidence`"))?;
    let mut decision_input = build_evidence_input(raw_evidence, scrubber)?;

    if let Some(images_val) = obj.get("images").filter(|v| !v.is_null()) {
        let images_arr = images_val
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("`invoke_decision` `images` must be a list"))?;
        let parsed_images = parse_decision_images(images_arr)?;
        if !parsed_images.is_empty() {
            decision_input = decision_input
                .with_images(parsed_images)
                .context("failed attaching images to decision evidence")?;
        }
    }

    let questions_obj = obj
        .get("questions")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("`invoke_decision` missing `questions` dict"))?;
    if questions_obj.is_empty() {
        bail!("`invoke_decision` `questions` must contain at least one question");
    }

    let (questions_schema, metas) = build_questions_schema(questions_obj)?;

    report_progress(
        ProgressKind::Log,
        format!(
            "decision `{provider_str}` (`{model_str}`) asking {} question(s)",
            questions_schema.len()
        ),
    );

    if provider_str.eq_ignore_ascii_case("openai") {
        // Note: `starlark` enables `serde_json/arbitrary_precision`, which triggers serde#505 on
        // `#[serde(flatten)]` inside `decision-rs`'s OpenAI `NamedAnswer` decoder. We validate
        // configuration, inputs, images, and schema with `decision-rs` above, then execute and
        // decode the OpenAI Decisions envelope without `#[serde(flatten)]`.
        return execute_openai_without_flatten(
            &client,
            model_str,
            endpoint_opt,
            openai_key_clone.as_ref(),
            timeout_ms,
            &decision_input,
            questions_obj,
            &metas,
        )
        .await
        .with_context(|| format!("decision call to `{provider_str}` (`{model_str}`) failed"));
    }

    let response = client
        .ask(decision_input, &questions_schema)
        .await
        .with_context(|| format!("decision call to `{provider_str}` (`{model_str}`) failed"))?;

    let mut answers_json = Map::with_capacity(metas.len());
    let mut summary_parts = Vec::with_capacity(metas.len());

    for (q_name, meta) in &metas {
        let ans = response.answers.get(q_name).ok_or_else(|| {
            anyhow::anyhow!("decision response missing expected question `{q_name}`")
        })?;
        match ans {
            Answer::Refusal(_) => {
                if !meta.optional {
                    bail!(
                        "decision question `{q_name}` was refused by `{}` (set `optional = True` on the question to handle refusals in state transitions)",
                        response.meta.wire_model
                    );
                }
                summary_parts.push(format!("{q_name}=refused"));
                answers_json.insert(
                    q_name.clone(),
                    json!({
                        "kind": meta.kind,
                        "refused": true,
                        "threshold": meta.threshold,
                        "value": Value::Null,
                        "selected": Value::Null,
                        "level": Value::Null,
                        "score": Value::Null,
                        "normalized_score": Value::Null,
                        "probability": Value::Null,
                        "probabilities": Value::Null,
                        "confidence": Value::Null,
                    }),
                );
            }
            Answer::Predicate(pred) => {
                let p = pred.probability().get();
                let is_yes = p >= meta.threshold;
                summary_parts.push(format!("{q_name}={is_yes}({p:.2})"));
                answers_json.insert(
                    q_name.clone(),
                    json!({
                        "kind": "noul",
                        "refused": false,
                        "value": is_yes,
                        "threshold": meta.threshold,
                        "probability": p,
                        "probabilities": {
                            "yes": p,
                            "no": (1.0 - p).clamp(0.0, 1.0),
                        },
                        "selected": Value::Null,
                        "level": Value::Null,
                        "score": Value::Null,
                        "normalized_score": Value::Null,
                        "confidence": Value::Null,
                    }),
                );
            }
            Answer::Choice(ch) => {
                let sel = ch.choice().to_owned();
                let sel_prob = ch.probabilities().get(&sel).map(|p| p.get());
                let conf = ch.confidence().map(|c| c.get());
                let probs_val = distribution_to_json(ch.probabilities(), None);
                match sel_prob {
                    Some(p) => summary_parts.push(format!("{q_name}={sel}({p:.2})")),
                    None => summary_parts.push(format!("{q_name}={sel}")),
                }
                answers_json.insert(
                    q_name.clone(),
                    json!({
                        "kind": "choice",
                        "refused": false,
                        "value": sel,
                        "selected": sel,
                        "probability": sel_prob,
                        "probabilities": probs_val,
                        "confidence": conf,
                        "level": Value::Null,
                        "score": Value::Null,
                        "normalized_score": Value::Null,
                    }),
                );
            }
            Answer::Score(sc) => {
                let raw = sc.score();
                let max_idx = meta.levels.len().saturating_sub(1);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let nearest_idx = (raw.round() as usize).min(max_idx);
                let nearest_level = meta.levels.get(nearest_idx).cloned();
                #[allow(clippy::cast_precision_loss)]
                let denom = max_idx.max(1) as f64;
                let normalized = (raw / denom).clamp(0.0, 1.0);
                let level_prob = sc
                    .probabilities()
                    .get(&nearest_idx.to_string())
                    .map(|p| p.get());
                let conf = sc.confidence().map(|c| c.get());
                let probs_val = distribution_to_json(sc.probabilities(), Some(&meta.levels));
                summary_parts.push(format!(
                    "{q_name}={}:{raw:.2}",
                    nearest_level.as_deref().unwrap_or("?")
                ));
                answers_json.insert(
                    q_name.clone(),
                    json!({
                        "kind": "score",
                        "refused": false,
                        "value": nearest_level,
                        "level": nearest_level,
                        "score": raw,
                        "normalized_score": normalized,
                        "probability": level_prob,
                        "probabilities": probs_val,
                        "confidence": conf,
                        "selected": Value::Null,
                    }),
                );
            }
            _ => bail!("unsupported decision-rs Answer variant for `{q_name}`"),
        }
    }

    report_progress(
        ProgressKind::Log,
        format!(
            "decision `{provider_str}` -> [{}] ({}ms)",
            summary_parts.join(", "),
            response.meta.latency.as_millis()
        ),
    );

    let u = &response.meta.usage;
    Ok(json!({
        "provider": provider_str,
        "model": model_str,
        "wire_model": response.meta.wire_model,
        "returned_model": response.meta.returned_model,
        "request_id": response.meta.request_id,
        "response_id": response.meta.response_id,
        "attempts": response.meta.attempts,
        "latency_ms": u64::try_from(response.meta.latency.as_millis()).unwrap_or(u64::MAX),
        "answers": Value::Object(answers_json),
        "usage": {
            "input_tokens": u.input_tokens,
            "output_tokens": u.output_tokens,
            "total_tokens": u.total_tokens,
            "cached_tokens": u.cached_tokens,
            "cache_write_tokens": u.cache_write_tokens,
            "reasoning_tokens": u.reasoning_tokens,
            "reported_cost_usd": u.reported_cost_usd,
        },
    }))
}

fn resolve_optional_secret_value(
    field_val: Option<&Value>,
    field_label: &str,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<Option<SecretString>> {
    let Some(val) = field_val.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        return Ok(Some(SecretString::from(trimmed.to_owned())));
    }
    if let Some(desc) = parse_secret_descriptor(val)? {
        return resolve_secret_descriptor(&desc, scrubber);
    }
    bail!("DecisionProvider `{field_label}` must be a string, `secret(...)`, or None")
}

fn build_decision_client(
    provider_str: &str,
    model_str: &str,
    endpoint_opt: Option<&str>,
    api_key_opt: Option<SecretString>,
    account_id_opt: Option<SecretString>,
    pool: HttpClient,
    settings: Settings,
) -> anyhow::Result<Client> {
    let norm = provider_str.trim().to_ascii_lowercase();
    if let Some(endpoint_url) = endpoint_opt {
        let endpoint = match norm.as_str() {
            "typesafe" | "jev" => Endpoint::system_one(endpoint_url),
            "openrouter" => Endpoint::jev_decisions(endpoint_url),
            "openai" => Endpoint::openai_decisions(endpoint_url),
            "gemini" => Endpoint::gemini_decisions(endpoint_url),
            "cloudflare" => Endpoint::clef_system_one(endpoint_url),
            other => bail!("unsupported decision provider `{other}`"),
        }
        .with_context(|| format!("invalid custom decision endpoint `{endpoint_url}`"))?;

        let auth = match api_key_opt {
            Some(secret) => Auth::ApiKey(secret),
            None => Auth::None,
        };

        // `Client::for_endpoint_with` requires a `Model::Custom(...)` whose identifier is not one
        // of the hosted built-in model aliases. Preserve explicit custom IDs verbatim and prefix
        // built-in defaults (`jev-1.14.0`, `gpt-6-luna`, ...) automatically as `custom/<model>`.
        let parsed = model_str
            .parse::<Model>()
            .with_context(|| format!("invalid decision model `{model_str}`"))?;
        let endpoint_model = match parsed {
            Model::Custom(_) => parsed,
            _ => {
                let adapted = format!("custom/{model_str}");
                let mid = ModelId::try_from(adapted.as_str())
                    .with_context(|| format!("invalid custom endpoint model `{adapted}`"))?;
                Model::Custom(mid)
            }
        };

        return Client::for_endpoint_with(endpoint, auth, endpoint_model, pool, settings)
            .context("failed constructing custom-endpoint decision client");
    }

    let provider = match norm.as_str() {
        "typesafe" | "jev" => Provider::TypeSafe,
        "gemini" => Provider::Gemini,
        "openai" => Provider::OpenAi,
        "openrouter" => Provider::OpenRouter,
        "cloudflare" => Provider::Cloudflare,
        other => bail!("unsupported decision provider `{other}`"),
    };

    let api_key = api_key_opt.ok_or_else(|| {
        anyhow::anyhow!("missing API key for hosted decision provider `{provider_str}`")
    })?;
    let parsed_model = model_str
        .parse::<Model>()
        .with_context(|| format!("invalid decision model `{model_str}` for `{provider_str}`"))?;

    let mut builder = Client::builder(provider)
        .api_key(api_key)
        .model(parsed_model)
        .http(pool)
        .settings(settings);

    if provider == Provider::Cloudflare {
        let acct_secret = account_id_opt.ok_or_else(|| {
            anyhow::anyhow!("Cloudflare decisions require `account_id` (`CLOUDFLARE_ACCOUNT_ID`)")
        })?;
        let acct_id: CloudflareAccountId = acct_secret
            .expose_secret()
            .parse()
            .context("invalid Cloudflare account ID")?;
        builder = builder.cloudflare_account(acct_id);
    }

    builder
        .build()
        .with_context(|| format!("failed building `{provider_str}` decision client"))
}

fn build_evidence_input(raw: &Value, scrubber: &mut SecretScrubber) -> anyhow::Result<Input> {
    if raw.is_null() {
        bail!("`invoke_decision` `evidence` must not be None");
    }
    let resolved = resolve_secrets_in_value(raw, scrubber)?;
    match &resolved {
        Value::String(s) => Input::try_from(s.as_str()).context("invalid string `evidence`"),
        Value::Object(_) | Value::Array(_) => {
            Input::json(&resolved).context("invalid structured JSON `evidence`")
        }
        scalar => Input::try_from(scalar.to_string()).context("invalid scalar `evidence`"),
    }
}

fn parse_decision_images(items: &[Value]) -> anyhow::Result<Vec<Image>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Some(path_str) = item.as_str() {
            let bytes = std::fs::read(path_str)
                .with_context(|| format!("failed reading decision image `{path_str}`"))?;
            let img = Image::try_from(bytes.as_slice())
                .with_context(|| format!("unsupported image format in `{path_str}`"))?;
            out.push(img);
            continue;
        }
        let obj = item.as_object().ok_or_else(|| {
            anyhow::anyhow!("decision image entry must be a file path string or `decision_image(...)` record")
        })?;
        let detail = match obj.get("detail").and_then(Value::as_str).unwrap_or("auto") {
            "auto" => ImageDetail::Auto,
            "low" => ImageDetail::Low,
            "high" => ImageDetail::High,
            other => bail!("unsupported image detail `{other}` (expected `auto`, `low`, or `high`)"),
        };
        let bytes = if let Some(path_str) = obj
            .get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            std::fs::read(path_str)
                .with_context(|| format!("failed reading decision image `{path_str}`"))?
        } else if let Some(b64_raw) = obj
            .get("base64")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let payload = match b64_raw.split_once(";base64,") {
                Some((_, rest)) => rest.trim(),
                None => b64_raw.trim(),
            };
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .context("invalid base64 payload in `decision_image`")?
        } else {
            bail!("`decision_image` requires either `path` or `base64`");
        };
        let img = Image::try_from(bytes.as_slice())
            .context("unsupported image format in `decision_image` (expected PNG, JPEG, or WebP)")?
            .with_detail(detail);
        out.push(img);
    }
    Ok(out)
}

fn build_questions_schema(
    questions_obj: &Map<String, Value>,
) -> anyhow::Result<(Questions, IndexMap<String, QuestionMeta>)> {
    let mut builder = Questions::builder();
    let mut metas = IndexMap::with_capacity(questions_obj.len());

    for (q_name, q_val) in questions_obj {
        let q = q_val.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "question `{q_name}` must be constructed with `noul(...)`, `choice(...)`, or `score(...)`"
            )
        })?;
        let kind = q
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let instructions = q
            .get("instructions")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("question `{q_name}` missing `instructions`"))?;
        let optional = q.get("optional").and_then(Value::as_bool).unwrap_or(false);
        let threshold = q.get("threshold").and_then(Value::as_f64).unwrap_or(0.5);

        match kind.as_str() {
            "noul" => {
                let yes_opt = q.get("yes").and_then(Value::as_str);
                let no_opt = q.get("no").and_then(Value::as_str);
                builder = match (yes_opt, no_opt) {
                    (Some(y), Some(n)) => builder.predicate_with(q_name, instructions, y, n),
                    (None, None) => builder.predicate(q_name, instructions),
                    _ => bail!("noul question `{q_name}` requires both `yes` and `no` when either is set"),
                };
                metas.insert(
                    q_name.clone(),
                    QuestionMeta {
                        kind,
                        threshold,
                        optional,
                        levels: Vec::new(),
                    },
                );
            }
            "choice" => {
                let pairs = unpack_pair_list(q.get("options"), q_name, "options")?;
                builder = builder.choice(q_name, instructions, pairs);
                metas.insert(
                    q_name.clone(),
                    QuestionMeta {
                        kind,
                        threshold,
                        optional,
                        levels: Vec::new(),
                    },
                );
            }
            "score" => {
                let pairs = unpack_pair_list(q.get("levels"), q_name, "levels")?;
                let level_names: Vec<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
                builder = builder.score(q_name, instructions, pairs);
                metas.insert(
                    q_name.clone(),
                    QuestionMeta {
                        kind,
                        threshold,
                        optional,
                        levels: level_names,
                    },
                );
            }
            other => {
                bail!(
                    "unknown decision question kind `{other}` for `{q_name}` (expected `noul`, `choice`, or `score`)"
                );
            }
        }
    }

    let built = builder.build().context("invalid decision question schema")?;
    Ok((built, metas))
}

fn unpack_pair_list(
    raw: Option<&Value>,
    q_name: &str,
    field: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let arr = raw
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("question `{q_name}` missing `{field}` pair list"))?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let pair = entry
            .as_array()
            .filter(|p| p.len() == 2)
            .ok_or_else(|| anyhow::anyhow!("question `{q_name}` `{field}` entry must be `[key, desc]`"))?;
        let k = pair[0]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("question `{q_name}` `{field}` key must be string"))?;
        let d = pair[1]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("question `{q_name}` `{field}` description must be string"))?;
        out.push((k.to_owned(), d.to_owned()));
    }
    Ok(out)
}

fn distribution_to_json(dist: &Distribution, level_names: Option<&[String]>) -> Value {
    let Distribution::Reported(items) = dist else {
        return Value::Null;
    };
    let mut map = Map::with_capacity(items.len());
    for (k, prob) in items {
        let mapped_key = if let Some(levels) = level_names
            && let Ok(idx) = k.parse::<usize>()
            && let Some(level_name) = levels.get(idx)
        {
            level_name.clone()
        } else {
            k.clone()
        };
        map.insert(mapped_key, json!(prob.get()));
    }
    Value::Object(map)
}

#[allow(clippy::too_many_arguments)]
async fn execute_openai_without_flatten(
    client: &Client,
    model_str: &str,
    endpoint_opt: Option<&str>,
    api_key_opt: Option<&SecretString>,
    timeout_ms: u64,
    input: &Input,
    questions_obj: &Map<String, Value>,
    metas: &IndexMap<String, QuestionMeta>,
) -> anyhow::Result<Value> {
    use std::time::{Duration, Instant};

    let url = endpoint_opt.unwrap_or("https://api.openai.com/v1/decisions");
    let wire_input = if input.images().is_empty() {
        Value::String(input.as_str().to_owned())
    } else {
        let mut content = vec![json!({
            "type": "input_text",
            "text": input.as_str(),
        })];
        for img in input.images() {
            let b64 = base64::engine::general_purpose::STANDARD.encode(img.as_bytes());
            let mut part = json!({
                "type": "input_image",
                "image_url": format!("data:{};base64,{b64}", img.media_type().as_str()),
            });
            match img.detail() {
                ImageDetail::Low => {
                    part["detail"] = json!("low");
                }
                ImageDetail::High => {
                    part["detail"] = json!("high");
                }
                _ => {}
            }
            content.push(part);
        }
        json!([{ "role": "user", "content": content }])
    };

    let mut wire_questions = Vec::with_capacity(questions_obj.len());
    for (name, q_val) in questions_obj {
        let q = q_val.as_object().unwrap();
        let kind = q.get("kind").and_then(Value::as_str).unwrap_or("");
        let instructions = q.get("instructions").and_then(Value::as_str).unwrap_or("");
        match kind {
            "noul" => {
                let yes_opt = q.get("yes").and_then(Value::as_str);
                let no_opt = q.get("no").and_then(Value::as_str);
                if let (Some(y), Some(n)) = (yes_opt, no_opt) {
                    wire_questions.push(json!({
                        "name": name,
                        "type": "choice",
                        "instructions": instructions,
                        "choices": [
                            { "value": true, "description": y },
                            { "value": false, "description": n },
                        ],
                    }));
                } else {
                    wire_questions.push(json!({
                        "name": name,
                        "type": "predicate",
                        "instructions": instructions,
                    }));
                }
            }
            "choice" => {
                let pairs = unpack_pair_list(q.get("options"), name, "options")?;
                let choices: Vec<Value> = pairs
                    .into_iter()
                    .map(|(k, d)| json!({ "value": k, "description": d }))
                    .collect();
                wire_questions.push(json!({
                    "name": name,
                    "type": "choice",
                    "instructions": instructions,
                    "choices": choices,
                }));
            }
            "score" => {
                let pairs = unpack_pair_list(q.get("levels"), name, "levels")?;
                let levels: Vec<Value> = pairs
                    .into_iter()
                    .map(|(k, d)| json!({ "label": k, "description": d }))
                    .collect();
                wire_questions.push(json!({
                    "name": name,
                    "type": "score",
                    "instructions": instructions,
                    "levels": levels,
                }));
            }
            _ => {}
        }
    }

    let req_body = serde_json::to_vec(&json!({
        "model": model_str,
        "input": wire_input,
        "questions": wire_questions,
    }))?;

    let started = Instant::now();
    let mut req = client
        .http()
        .as_reqwest()
        .post(url)
        .timeout(Duration::from_millis(timeout_ms))
        .header("Content-Type", "application/json")
        .body(req_body);

    if let Some(secret) = api_key_opt {
        req = req.header("Authorization", format!("Bearer {}", secret.expose_secret()));
    }

    let resp = req.send().await.context("HTTP transport error")?;
    let status = resp.status();
    let req_id = resp
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body_bytes = resp.bytes().await.context("failed reading response body")?;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    if !status.is_success() {
        bail!("OpenAI Decisions returned HTTP {status}");
    }

    let root: Value = serde_json::from_slice(&body_bytes)
        .context("invalid JSON in OpenAI Decisions response")?;
    let root_obj = root
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("OpenAI Decisions response must be a JSON object"))?;

    let returned_model = root_obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let response_id = root_obj
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let raw_answers = root_obj
        .get("answers")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("OpenAI Decisions response missing `answers` array"))?;

    if raw_answers.len() != metas.len() {
        bail!("OpenAI Decisions answer count differs from schema");
    }

    let mut by_name: HashMap<String, &Map<String, Value>> =
        HashMap::with_capacity(raw_answers.len());
    for item in raw_answers {
        let m = item
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("OpenAI answer entry must be an object"))?;
        let name = m
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("OpenAI answer entry missing `name`"))?;
        if !metas.contains_key(name) || by_name.insert(name.to_owned(), m).is_some() {
            bail!("unknown or duplicate answer `{name}` in OpenAI response");
        }
    }

    let mut answers_json = Map::with_capacity(metas.len());
    let mut summary_parts = Vec::with_capacity(metas.len());

    for (q_name, meta) in metas {
        let entry = by_name
            .get(q_name)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("missing answer `{q_name}`"))?;
        let ans_type = entry.get("type").and_then(Value::as_str).unwrap_or("");

        if ans_type == "refusal" {
            if !meta.optional {
                bail!(
                    "decision question `{q_name}` was refused by `{model_str}` (set `optional = True` on the question to handle refusals in state transitions)"
                );
            }
            summary_parts.push(format!("{q_name}=refused"));
            answers_json.insert(
                q_name.clone(),
                json!({
                    "kind": meta.kind,
                    "refused": true,
                    "threshold": meta.threshold,
                    "value": Value::Null,
                    "selected": Value::Null,
                    "level": Value::Null,
                    "score": Value::Null,
                    "normalized_score": Value::Null,
                    "probability": Value::Null,
                    "probabilities": Value::Null,
                    "confidence": Value::Null,
                }),
            );
            continue;
        }

        match (meta.kind.as_str(), ans_type) {
            ("noul", "predicate") => {
                let p = valid_unit_prob(entry.get("probability"), q_name)?;
                let is_yes = p >= meta.threshold;
                summary_parts.push(format!("{q_name}={is_yes}({p:.2})"));
                answers_json.insert(
                    q_name.clone(),
                    json!({
                        "kind": "noul",
                        "refused": false,
                        "value": is_yes,
                        "threshold": meta.threshold,
                        "probability": p,
                        "probabilities": { "yes": p, "no": (1.0 - p).clamp(0.0, 1.0) },
                        "selected": Value::Null,
                        "level": Value::Null,
                        "score": Value::Null,
                        "normalized_score": Value::Null,
                        "confidence": Value::Null,
                    }),
                );
            }
            ("noul", "choice") => {
                let probs_arr = entry
                    .get("probabilities")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("missing boolean `probabilities` for `{q_name}`"))?;
                let mut p_true = None;
                let mut sum = 0.0;
                for item in probs_arr {
                    let prob = valid_unit_prob(item.get("probability"), q_name)?;
                    sum += prob;
                    if item.get("value").and_then(Value::as_bool) == Some(true) {
                        p_true = Some(prob);
                    }
                }
                if (sum - 1.0).abs() > 0.002 {
                    bail!("unnormalized boolean distribution for `{q_name}`");
                }
                let p = p_true.ok_or_else(|| anyhow::anyhow!("missing `true` probability for `{q_name}`"))?;
                let is_yes = p >= meta.threshold;
                let conf = entry.get("confidence").map(|v| valid_unit_prob(Some(v), q_name)).transpose()?;
                summary_parts.push(format!("{q_name}={is_yes}({p:.2})"));
                answers_json.insert(
                    q_name.clone(),
                    json!({
                        "kind": "noul",
                        "refused": false,
                        "value": is_yes,
                        "threshold": meta.threshold,
                        "probability": p,
                        "probabilities": { "yes": p, "no": (1.0 - p).clamp(0.0, 1.0) },
                        "selected": Value::Null,
                        "level": Value::Null,
                        "score": Value::Null,
                        "normalized_score": Value::Null,
                        "confidence": conf,
                    }),
                );
            }
            ("choice", "choice") => {
                let sel = entry
                    .get("choice")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing string `choice` for `{q_name}`"))?
                    .to_owned();
                let conf = valid_unit_prob(entry.get("confidence"), q_name)?;
                let probs_arr = entry
                    .get("probabilities")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("missing `probabilities` for `{q_name}`"))?;
                let mut probs_map = Map::with_capacity(probs_arr.len());
                let mut sum = 0.0;
                for item in probs_arr {
                    let k = item
                        .get("value")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("choice probability `value` must be string"))?;
                    let p = valid_unit_prob(item.get("probability"), q_name)?;
                    sum += p;
                    probs_map.insert(k.to_owned(), json!(p));
                }
                if (sum - 1.0).abs() > 0.002 {
                    bail!("unnormalized choice distribution for `{q_name}`");
                }
                let sel_prob = probs_map.get(&sel).and_then(Value::as_f64);
                summary_parts.push(format!("{q_name}={sel}({:.2})", sel_prob.unwrap_or(0.0)));
                answers_json.insert(
                    q_name.clone(),
                    json!({
                        "kind": "choice",
                        "refused": false,
                        "value": sel,
                        "selected": sel,
                        "probability": sel_prob,
                        "probabilities": Value::Object(probs_map),
                        "confidence": conf,
                        "level": Value::Null,
                        "score": Value::Null,
                        "normalized_score": Value::Null,
                    }),
                );
            }
            ("score", "score") => {
                let raw = entry
                    .get("score")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| anyhow::anyhow!("missing numeric `score` for `{q_name}`"))?;
                let max_idx = meta.levels.len().saturating_sub(1);
                #[allow(clippy::cast_precision_loss)]
                let max_f = max_idx as f64;
                if !raw.is_finite() || !(0.0..=max_f).contains(&raw) {
                    bail!("score `{raw}` out of bounds for `{q_name}`");
                }
                let conf = valid_unit_prob(entry.get("confidence"), q_name)?;
                let probs_arr = entry
                    .get("probabilities")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("missing score `probabilities` for `{q_name}`"))?;
                let mut probs_map = Map::with_capacity(probs_arr.len());
                let mut sum = 0.0;
                let mut mean = 0.0;
                for item in probs_arr {
                    let idx_u64 = item
                        .get("value")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow::anyhow!("score probability `value` must be integer index"))?;
                    let idx = usize::try_from(idx_u64).unwrap_or(usize::MAX);
                    let label = item
                        .get("label")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("score probability missing `label`"))?;
                    if meta.levels.get(idx).map(String::as_str) != Some(label) {
                        bail!("score label/index mismatch for `{q_name}`");
                    }
                    let p = valid_unit_prob(item.get("probability"), q_name)?;
                    sum += p;
                    #[allow(clippy::cast_precision_loss)]
                    {
                        mean += (idx as f64) * p;
                    }
                    probs_map.insert(label.to_owned(), json!(p));
                }
                if probs_map.len() != meta.levels.len() || (sum - 1.0).abs() > 0.002 {
                    bail!("incomplete or unnormalized score distribution for `{q_name}`");
                }
                if ((mean / sum) - raw).abs() > 0.001 * max_f.max(1.0) {
                    bail!("score disagrees with distribution for `{q_name}`");
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let nearest_idx = (raw.round() as usize).min(max_idx);
                let nearest_level = meta.levels.get(nearest_idx).cloned();
                let level_prob = nearest_level
                    .as_ref()
                    .and_then(|l| probs_map.get(l))
                    .and_then(Value::as_f64);
                let normalized = (raw / max_f.max(1.0)).clamp(0.0, 1.0);
                summary_parts.push(format!(
                    "{q_name}={}:{raw:.2}",
                    nearest_level.as_deref().unwrap_or("?")
                ));
                answers_json.insert(
                    q_name.clone(),
                    json!({
                        "kind": "score",
                        "refused": false,
                        "value": nearest_level,
                        "level": nearest_level,
                        "score": raw,
                        "normalized_score": normalized,
                        "probability": level_prob,
                        "probabilities": Value::Object(probs_map),
                        "confidence": conf,
                        "selected": Value::Null,
                    }),
                );
            }
            _ => bail!("OpenAI answer type `{ans_type}` does not match schema kind `{}` for `{q_name}`", meta.kind),
        }
    }

    report_progress(
        ProgressKind::Log,
        format!(
            "decision `openai` -> [{}] ({latency_ms}ms)",
            summary_parts.join(", ")
        ),
    );

    let usage_obj = root_obj.get("usage").and_then(Value::as_object);
    let in_tok = usage_obj
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let out_tok = usage_obj
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tok = usage_obj
        .and_then(|u| u.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(in_tok + out_tok);

    Ok(json!({
        "provider": "openai",
        "model": model_str,
        "wire_model": model_str,
        "returned_model": returned_model,
        "request_id": req_id,
        "response_id": response_id,
        "attempts": 1,
        "latency_ms": latency_ms,
        "answers": Value::Object(answers_json),
        "usage": {
            "input_tokens": in_tok,
            "output_tokens": out_tok,
            "total_tokens": total_tok,
            "cached_tokens": Value::Null,
            "cache_write_tokens": Value::Null,
            "reasoning_tokens": Value::Null,
            "reported_cost_usd": usage_obj.and_then(|u| u.get("cost")).and_then(Value::as_f64),
        },
    }))
}

fn valid_unit_prob(val: Option<&Value>, q_name: &str) -> anyhow::Result<f64> {
    let p = val
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("missing numeric probability for `{q_name}`"))?;
    let checked = decision_rs::Probability::try_from(p)
        .map_err(|_| anyhow::anyhow!("invalid probability `{p}` for `{q_name}`"))?;
    Ok(checked.get())
}
