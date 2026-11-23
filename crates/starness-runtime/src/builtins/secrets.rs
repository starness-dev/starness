//! Late-bound secret resolution (`env`, `file`, OS `keychain`), trace redaction, and
//! `Zeroize`-backed error scrubbing for `@starness/secrets`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, bail};
use indexmap::IndexMap;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Map, Value};
use starness_ir::SECRET_MARKER_KEY;

use crate::trace::register_scope_redaction;

/// Default OS keychain service namespace used when `secret(..., source = "keychain")` omits `service`.
pub const DEFAULT_KEYCHAIN_SERVICE: &str = "starness";

/// OS keychain service namespace used for MCP OAuth 2.1 credentials (`starness auth login`).
pub const MCP_OAUTH_KEYCHAIN_SERVICE: &str = "starness.mcp.oauth";

/// Formats a Keychain account name, prefixing `<machine_id>/` when scoped to a specific machine.
pub fn scoped_keychain_account(machine_id: Option<&str>, key: &str) -> String {
    match machine_id.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!("{m}/{}", key.trim()),
        None => key.trim().to_owned(),
    }
}

/// Optional JSON file override (`{"<service>:<name>": "<secret>"}`) enabling headless CI and
/// integration tests to exercise `source = "keychain"` without triggering OS Keychain modal dialogs.
pub const KEYCHAIN_MOCK_FILE_ENV: &str = "STARNESS_KEYCHAIN_MOCK_FILE";

/// Deserialized payload inside `{"$starness_secret": { ... }}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SecretDescriptor {
    #[serde(default = "default_env_source")]
    pub source: String,
    pub name: String,
    #[serde(default = "default_keychain_service")]
    pub service: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub optional: bool,
}

fn default_env_source() -> String {
    "env".to_owned()
}

fn default_keychain_service() -> String {
    DEFAULT_KEYCHAIN_SERVICE.to_owned()
}

/// Parses a `serde_json::Value` as a [`SecretDescriptor`] if it is a `{"$starness_secret": ...}` marker.
pub fn parse_secret_descriptor(val: &Value) -> anyhow::Result<Option<SecretDescriptor>> {
    let Some(obj) = val.as_object() else {
        return Ok(None);
    };
    let Some(raw_inner) = obj.get(SECRET_MARKER_KEY) else {
        return Ok(None);
    };
    let desc: SecretDescriptor = serde_json::from_value(raw_inner.clone())
        .context("malformed `$starness_secret` descriptor")?;
    Ok(Some(desc))
}

/// Replaces every `{"$starness_secret": {"name": NAME, ...}}` object in `val` with `"<secret:NAME>"`.
pub fn redact_secret_descriptors(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            if let Some(inner) = map.get(SECRET_MARKER_KEY) {
                let name = inner
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("REDACTED");
                return Value::String(format!("<secret:{name}>"));
            }
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), redact_secret_descriptors(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(redact_secret_descriptors).collect())
        }
        other => other.clone(),
    }
}

/// Tracks resolved secret values in `ZeroizeOnDrop` [`SecretString`]s during a service invocation
/// so error strings and traces scrub raw secret material before surfacing it to callers or logs.
#[derive(Debug, Default, Clone)]
pub struct SecretScrubber {
    /// `(raw_secret_value, "<secret:NAME>")`, kept ordered longest-first.
    entries: Vec<(SecretString, String)>,
}

impl SecretScrubber {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a resolved raw secret value under `name` (`<secret:name>`) both locally and in
    /// any active [`ServiceScope`](crate::trace::ServiceScope) trace context.
    pub fn register(&mut self, name: &str, raw_secret: &SecretString) {
        if raw_secret.expose_secret().is_empty() {
            return;
        }
        let placeholder = format!("<secret:{name}>");
        register_scope_redaction(raw_secret.clone(), &placeholder);
        if self
            .entries
            .iter()
            .any(|(needle, _)| needle.expose_secret() == raw_secret.expose_secret())
        {
            return;
        }
        self.entries.push((raw_secret.clone(), placeholder));
        self.entries
            .sort_by_key(|a| std::cmp::Reverse(a.0.expose_secret().len()));
    }

    /// Replaces any registered raw secret value in `input` with `<secret:NAME>`.
    pub fn scrub_str(&self, input: &str) -> String {
        scrub_with_entries(input, &self.entries)
    }

    /// Scrubs any registered raw secret values from an `anyhow::Result<T>` error message.
    pub fn scrub_result<T>(&self, res: anyhow::Result<T>) -> anyhow::Result<T> {
        if self.entries.is_empty() {
            return res;
        }
        res.map_err(|err| {
            let formatted = format!("{err:#}");
            anyhow::anyhow!("{}", self.scrub_str(&formatted))
        })
    }
}

pub(crate) fn scrub_with_entries(input: &str, entries: &[(SecretString, String)]) -> String {
    if entries.is_empty() || input.is_empty() {
        return input.to_owned();
    }
    let mut current = input.to_owned();
    for (needle, replacement) in entries {
        let secret_str = needle.expose_secret();
        if !secret_str.is_empty() && current.contains(secret_str) {
            current = current.replace(secret_str, replacement);
        }
    }
    current
}

pub(crate) fn scrub_json_with_entries(val: &Value, entries: &[(SecretString, String)]) -> Value {
    let redacted = redact_secret_descriptors(val);
    if entries.is_empty() {
        return redacted;
    }
    scrub_json_inner(redacted, entries)
}

fn scrub_json_inner(val: Value, entries: &[(SecretString, String)]) -> Value {
    match val {
        Value::String(s) => Value::String(scrub_with_entries(&s, entries)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| scrub_json_inner(v, entries))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k, scrub_json_inner(v, entries));
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// Stores a secret in the OS Keychain (macOS Keychain, Windows Credential Manager, or Linux Secret Service).
pub fn keychain_set(service: &str, name: &str, value: &SecretString) -> anyhow::Result<()> {
    let svc = normalize_service(service)?;
    let key = normalize_name(name)?;
    if let Some(mock_path) = mock_keychain_path() {
        let mut map = read_mock_keychain(&mock_path)?;
        map.insert(format!("{svc}:{key}"), value.expose_secret().to_owned());
        return write_mock_keychain(&mock_path, &map);
    }
    let entry = keyring::Entry::new(svc, key)
        .with_context(|| format!("failed opening OS keychain entry `{svc}/{key}`"))?;
    entry
        .set_password(value.expose_secret())
        .with_context(|| format!("failed storing secret `{key}` in OS keychain (`{svc}`)"))
}

/// Deletes a secret from the OS Keychain, returning `Ok(true)` if it existed or `Ok(false)` if not found.
pub fn keychain_delete(service: &str, name: &str) -> anyhow::Result<bool> {
    let svc = normalize_service(service)?;
    let key = normalize_name(name)?;
    if let Some(mock_path) = mock_keychain_path() {
        let mut map = read_mock_keychain(&mock_path)?;
        let removed = map.remove(&format!("{svc}:{key}")).is_some();
        write_mock_keychain(&mock_path, &map)?;
        return Ok(removed);
    }
    let entry = keyring::Entry::new(svc, key)
        .with_context(|| format!("failed opening OS keychain entry `{svc}/{key}`"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(err) => Err(anyhow::anyhow!(
            "failed deleting secret `{key}` from OS keychain (`{svc}`): {err}"
        )),
    }
}

/// Checks whether `(service, name)` exists in the OS Keychain without exposing its plaintext value.
pub fn keychain_exists(service: &str, name: &str) -> anyhow::Result<bool> {
    Ok(keychain_get_optional(service, name)?.is_some())
}

/// Looks up a secret in the OS Keychain, checking `<machine_id>/<name>` first (when `machine_id`
/// is provided) and falling back to the global `<name>` entry.
pub fn keychain_get_scoped(
    service: &str,
    name: &str,
    machine_id: Option<&str>,
) -> anyhow::Result<Option<SecretString>> {
    if let Some(m) = machine_id.map(str::trim).filter(|m| !m.is_empty()) {
        let scoped = scoped_keychain_account(Some(m), name);
        if let Some(found) = keychain_get_optional(service, &scoped)? {
            return Ok(Some(found));
        }
    }
    keychain_get_optional(service, name)
}

/// Reads an optional secret from the OS Keychain (or `STARNESS_KEYCHAIN_MOCK_FILE` in tests).
pub fn keychain_get_optional(service: &str, name: &str) -> anyhow::Result<Option<SecretString>> {
    let svc = normalize_service(service)?;
    let key = normalize_name(name)?;
    if let Some(mock_path) = mock_keychain_path() {
        let map = read_mock_keychain(&mock_path)?;
        return Ok(map
            .get(&format!("{svc}:{key}"))
            .filter(|s| !s.is_empty())
            .cloned()
            .map(SecretString::from));
    }
    let entry = keyring::Entry::new(svc, key)
        .with_context(|| format!("failed opening OS keychain entry `{svc}/{key}`"))?;
    match entry.get_password() {
        Ok(pw) if !pw.is_empty() => Ok(Some(SecretString::from(pw))),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(anyhow::anyhow!(
            "failed reading secret `{key}` from OS keychain (`{svc}`): {err}"
        )),
    }
}

fn normalize_service(service: &str) -> anyhow::Result<&str> {
    let trimmed = service.trim();
    if trimmed.is_empty() {
        bail!("keychain `service` must not be empty");
    }
    Ok(trimmed)
}

fn normalize_name(name: &str) -> anyhow::Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("secret `name` must not be empty");
    }
    Ok(trimmed)
}

fn mock_keychain_path() -> Option<std::path::PathBuf> {
    std::env::var(KEYCHAIN_MOCK_FILE_ENV)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}

fn read_mock_keychain(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading mock keychain `{}`", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    serde_json::from_str(&raw)
        .with_context(|| format!("invalid JSON in mock keychain `{}`", path.display()))
}

fn write_mock_keychain(path: &Path, map: &HashMap<String, String>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(map)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Resolves a single [`SecretDescriptor`] from `"env"`, `"file"`, or `"keychain"` into a
/// `ZeroizeOnDrop` [`SecretString`], returning `Ok(None)` when missing/empty and `optional == true`.
pub fn resolve_secret_descriptor(
    desc: &SecretDescriptor,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<Option<SecretString>> {
    let name = desc.name.trim();
    if name.is_empty() {
        bail!("secret descriptor `name` must not be empty");
    }

    let raw_opt: Option<SecretString> = match desc.source.trim().to_ascii_lowercase().as_str() {
        "env" => match std::env::var(name) {
            Ok(v) if !v.is_empty() => Some(SecretString::from(v)),
            Ok(_) | Err(std::env::VarError::NotPresent) => {
                // Fall back to OS Keychain (e.g. stored via `starness secrets set <NAME>`).
                let service = if desc.service.trim().is_empty() {
                    DEFAULT_KEYCHAIN_SERVICE
                } else {
                    desc.service.trim()
                };
                let active_machine =
                    crate::trace::current_scope().map(|s| s.hub.machine_id().to_owned());
                if let Ok(Some(sec)) =
                    keychain_get_scoped(service, name, active_machine.as_deref())
                {
                    Some(sec)
                } else if desc.optional {
                    None
                } else {
                    bail!(
                        "missing required secret `{name}`: environment variable `{name}` is not set and no OS keychain entry was found (run `starness secrets set {name}`)"
                    );
                }
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("secret `{name}`: environment variable `{name}` is not valid UTF-8");
            }
        },
        "file" => match std::fs::read_to_string(name) {
            Ok(contents) => {
                let trimmed = contents.trim_end_matches(['\r', '\n']).to_owned();
                if trimmed.is_empty() {
                    if desc.optional {
                        None
                    } else {
                        bail!("missing required secret `{name}`: file `{name}` is empty");
                    }
                } else {
                    Some(SecretString::from(trimmed))
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && desc.optional => None,
            Err(err) => {
                bail!("failed reading secret file `{name}`: {err}");
            }
        },
        "keychain" | "keyring" => {
            let service = if desc.service.trim().is_empty() {
                DEFAULT_KEYCHAIN_SERVICE
            } else {
                desc.service.trim()
            };
            let active_machine = crate::trace::current_scope().map(|s| s.hub.machine_id().to_owned());
            match keychain_get_scoped(service, name, active_machine.as_deref())? {
                Some(sec) => Some(sec),
                None if desc.optional => None,
                None => {
                    bail!(
                        "missing required secret `{name}`: no entry found in OS keychain for service `{service}` (run `starness secrets set {name}`)"
                    );
                }
            }
        }
        other => {
            bail!(
                "unsupported secret source `{other}` for `{name}` (expected `env`, `file`, or `keychain`)"
            );
        }
    };

    let Some(raw) = raw_opt else {
        return Ok(None);
    };
    scrubber.register(name, &raw);
    if desc.prefix.is_empty() {
        Ok(Some(raw))
    } else {
        Ok(Some(SecretString::from(format!(
            "{}{}",
            desc.prefix,
            raw.expose_secret()
        ))))
    }
}

/// Resolves a `dict[str, str | Secret]` JSON map (`headers` or `env`) into `SecretString` values,
/// omitting entries whose `optional = True` secret is unset.
pub fn resolve_secret_string_map(
    map: Option<&Map<String, Value>>,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<IndexMap<String, SecretString>> {
    let Some(entries) = map else {
        return Ok(IndexMap::new());
    };
    let mut out = IndexMap::with_capacity(entries.len());
    for (k, v) in entries {
        if let Some(s) = v.as_str() {
            out.insert(k.clone(), SecretString::from(s.to_owned()));
        } else if let Some(desc) = parse_secret_descriptor(v)? {
            if let Some(resolved) = resolve_secret_descriptor(&desc, scrubber)? {
                out.insert(k.clone(), resolved);
            }
        } else {
            bail!("expected string or `secret(...)` value for key `{k}`, got `{v}`");
        }
    }
    Ok(out)
}

/// Recursively resolves any `$starness_secret` descriptors inside an arbitrary JSON value
/// (e.g. HTTP `json` payload or MCP `arguments`). Optional missing secrets resolve to `null`.
pub fn resolve_secrets_in_value(
    val: &Value,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<Value> {
    if let Some(desc) = parse_secret_descriptor(val)? {
        return match resolve_secret_descriptor(&desc, scrubber)? {
            Some(s) => Ok(Value::String(s.expose_secret().to_owned())),
            None => Ok(Value::Null),
        };
    }
    match val {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), resolve_secrets_in_value(v, scrubber)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(resolve_secrets_in_value(item, scrubber)?);
            }
            Ok(Value::Array(out))
        }
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn redacts_secret_descriptors_and_scrubs_resolved_values_from_errors() {
        let raw_input = json!({
            "url": "https://api.example.com",
            "headers": {
                "Authorization": {
                    "$starness_secret": {
                        "source": "env",
                        "name": "EXAMPLE_TOKEN",
                        "prefix": "Bearer ",
                        "optional": false
                    }
                }
            }
        });

        let redacted = redact_secret_descriptors(&raw_input);
        assert_eq!(
            redacted["headers"]["Authorization"],
            json!("<secret:EXAMPLE_TOKEN>")
        );

        let mut scrubber = SecretScrubber::new();
        scrubber.register("EXAMPLE_TOKEN", &SecretString::from("sk_live_99887766"));
        let err = scrubber
            .scrub_result::<()>(Err(anyhow::anyhow!(
                "401 Unauthorized for header Bearer sk_live_99887766"
            )))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(!msg.contains("sk_live_99887766"), "leaked secret in: {msg}");
        assert!(msg.contains("Bearer <secret:EXAMPLE_TOKEN>"), "got: {msg}");
    }

    #[test]
    fn resolves_file_secret_trimming_newlines_and_respects_optional() {
        let file_path = std::env::temp_dir().join(format!(
            "starness_secret_test_{}_{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&file_path, "super_secret_from_disk\r\n").unwrap();

        let map_val = json!({
            "X-File-Key": {
                "$starness_secret": {
                    "source": "file",
                    "name": file_path.to_string_lossy(),
                    "prefix": "Token ",
                    "optional": false
                }
            },
            "X-Optional-Missing": {
                "$starness_secret": {
                    "source": "file",
                    "name": "/nonexistent/starness/optional/secret/path",
                    "prefix": "",
                    "optional": true
                }
            }
        });

        let mut scrubber = SecretScrubber::new();
        let resolved =
            resolve_secret_string_map(map_val.as_object(), &mut scrubber).expect("resolve");
        let _ = std::fs::remove_file(&file_path);

        assert_eq!(
            resolved.get("X-File-Key").map(ExposeSecret::expose_secret),
            Some("Token super_secret_from_disk")
        );
        assert!(!resolved.contains_key("X-Optional-Missing"));
    }
}
