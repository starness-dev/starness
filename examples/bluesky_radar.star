# Live Bluesky Topic Radar (pure headless `.star` statechart — zero UI coupling):
# - Queries Bluesky's public unauthenticated AppView API (`api.bsky.app`) for posts matching
#   a topic (`"ai harness"` by default) across two parallel orthogonal regions (`latest` + `top`),
#   each with per-region exponential backoff on HTTP 429/5xx or transient network errors.
# - Deduplicates and summarizes posts into structured context (`summary_kv`, `posts_rows`, `digest_md`)
#   and parks in a hierarchical `watching` compound state (`paused` vs `auto_polling`).
# - Pair with the external `examples/bluesky_radar.ui.json` view-sheet for both Web Studio and TUI:
#     starness check examples/bluesky_radar.star --ui examples/bluesky_radar.ui.json
#     starness run -i examples/bluesky_radar.star --ui examples/bluesky_radar.ui.json
#     starness serve examples/bluesky_radar.star --ui examples/bluesky_radar.ui.json --bind 127.0.0.1:8080

load("@starness/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")

RadarEvent = enum(
    "SAMPLE_NOW",
    "UPDATE_QUERY",
    "START_POLLING",
    "PAUSE_POLLING",
    "CURATE_DIGEST",
    "EXPORT_DIGEST",
    "BACK_TO_WATCH",
    "STOP",
)

RadarCtx = record(
    api_base = field(str, "https://api.bsky.app"),
    query = field(str, "ai harness"),
    limit = field(int, 10),
    poll_interval_ms = field(int, 30000),
    max_retries = field(int, 3),
    base_backoff_ms = field(int, 500),
    latest_retries = field(int, 0),
    top_retries = field(int, 0),
    latest_posts = field(list[dict], []),
    top_posts = field(list[dict], []),
    latest_error = field(str, ""),
    top_error = field(str, ""),
    seen_uris = field(list[str], []),
    sample_count = field(int, 0),
    new_this_tick = field(int, 0),
    status_label = field(str, "Sampling Bluesky…"),
    alert_title = field(str, "Bluesky Radar Ready"),
    alert_message = field(str, "Waiting for first sample."),
    summary_kv = field(dict, {}),
    posts_rows = field(list[list[str]], []),
    digest_md = field(str, "# Bluesky Radar\n\nNo posts sampled yet."),
    curated_notes = field(list[dict], []),
)

S = state_space(
    "sampling",
    "latest_region",
    "fetching_latest",
    "latest_retry_backoff",
    "latest_done",
    "top_region",
    "fetching_top",
    "top_retry_backoff",
    "top_done",
    "summarizing",
    "watching",
    "paused",
    "auto_polling",
    "curating",
    "done",
)

def url_encode(raw: str) -> str:
    s = raw.replace("%", "%25")
    s = s.replace(" ", "%20")
    s = s.replace("\"", "%22")
    s = s.replace("#", "%23")
    s = s.replace("&", "%26")
    s = s.replace("+", "%2B")
    s = s.replace("/", "%2F")
    s = s.replace(":", "%3A")
    s = s.replace("=", "%3D")
    s = s.replace("?", "%3F")
    s = s.replace("@", "%40")
    return s

def build_search_request(ctx: RadarCtx, sort_mode: str) -> HttpRequest:
    q = url_encode(ctx.query.strip())
    url = (
        ctx.api_base
        + "/xrpc/app.bsky.feed.searchPosts?q="
        + q
        + "&limit="
        + str(ctx.limit)
        + "&sort="
        + sort_mode
    )
    return http_request(
        method = HttpMethod.GET,
        url = url,
        headers = {"Accept": "application/json", "User-Agent": "starness-bluesky-radar/0.1"},
        timeout_ms = 10000,
    )

def build_latest_request(ctx: RadarCtx) -> HttpRequest:
    return build_search_request(ctx, "latest")

def build_top_request(ctx: RadarCtx) -> HttpRequest:
    return build_search_request(ctx, "top")

def is_http_ok(_ctx: RadarCtx, ev: dict) -> bool:
    res = ev.get("output", {})
    body = res.get("body", {})
    return res.get("status") == 200 and type(body) == "dict"

def can_retry_latest(ctx: RadarCtx, ev: dict) -> bool:
    status = ev.get("output", {}).get("status", 503)
    return (status == 429 or status >= 500) and ctx.latest_retries < ctx.max_retries

def can_retry_top(ctx: RadarCtx, ev: dict) -> bool:
    status = ev.get("output", {}).get("status", 503)
    return (status == 429 or status >= 500) and ctx.top_retries < ctx.max_retries

def can_retry_latest_err(ctx: RadarCtx, _ev: dict) -> bool:
    return ctx.latest_retries < ctx.max_retries

def can_retry_top_err(ctx: RadarCtx, _ev: dict) -> bool:
    return ctx.top_retries < ctx.max_retries

def latest_backoff_ms(ctx: RadarCtx, _ev: dict) -> int:
    shift = ctx.latest_retries - 1 if ctx.latest_retries > 0 else 0
    return ctx.base_backoff_ms * (1 << shift)

def top_backoff_ms(ctx: RadarCtx, _ev: dict) -> int:
    shift = ctx.top_retries - 1 if ctx.top_retries > 0 else 0
    return ctx.base_backoff_ms * (1 << shift)

def poll_delay_ms(ctx: RadarCtx, _ev: dict) -> int:
    return ctx.poll_interval_ms

def extract_posts(ev: dict) -> list[dict]:
    body = ev.get("output", {}).get("body", {})
    if type(body) != "dict":
        return []
    raw_posts = body.get("posts", [])
    if type(raw_posts) != "list":
        return []
    cleaned = []
    for p in raw_posts:
        if type(p) != "dict":
            continue
        author = p.get("author", {})
        handle = str(author.get("handle", "unknown")) if type(author) == "dict" else "unknown"
        rec = p.get("record", {})
        text = str(rec.get("text", "")) if type(rec) == "dict" else ""
        created = str(rec.get("createdAt", "")) if type(rec) == "dict" else ""
        cleaned.append({
            "uri": str(p.get("uri", "")),
            "handle": handle,
            "text": text,
            "created_at": created,
            "likes": int(p.get("likeCount", 0) or 0),
            "reposts": int(p.get("repostCount", 0) or 0),
            "replies": int(p.get("replyCount", 0) or 0),
        })
    return cleaned

def save_latest_posts(ctx: RadarCtx, ev: dict) -> RadarCtx:
    return update(ctx, latest_posts = extract_posts(ev), latest_error = "")

def save_top_posts(ctx: RadarCtx, ev: dict) -> RadarCtx:
    return update(ctx, top_posts = extract_posts(ev), top_error = "")

def save_latest_error(ctx: RadarCtx, ev: dict) -> RadarCtx:
    status = ev.get("output", {}).get("status", None)
    msg = "HTTP " + str(status) if status != None else str(ev.get("error", "request failed"))
    return update(ctx, latest_posts = [], latest_error = msg)

def save_top_error(ctx: RadarCtx, ev: dict) -> RadarCtx:
    status = ev.get("output", {}).get("status", None)
    msg = "HTTP " + str(status) if status != None else str(ev.get("error", "request failed"))
    return update(ctx, top_posts = [], top_error = msg)

def compact_excerpt(raw: str, max_len: int) -> str:
    flat = " ".join(raw.replace("\r", " ").replace("\n", " ").split())
    if len(flat) <= max_len:
        return flat
    return flat[:max_len - 1] + "…"

def summarize_sample(ctx: RadarCtx, _ev: dict) -> RadarCtx:
    next_samples = ctx.sample_count + 1
    seen = list(ctx.seen_uris)
    seen_lookup = {}
    for u in seen:
        seen_lookup[u] = True

    combined = []
    batch_uris = {}
    new_count = 0

    for p in ctx.top_posts + ctx.latest_posts:
        uri = str(p.get("uri", ""))
        if uri == "" or batch_uris.get(uri, False):
            continue
        batch_uris[uri] = True
        combined.append(p)
        if not seen_lookup.get(uri, False):
            seen_lookup[uri] = True
            seen.append(uri)
            new_count += 1

    rows = []
    for p in combined:
        rows.append([
            "@" + str(p.get("handle", "unknown")),
            str(p.get("likes", 0)),
            str(p.get("reposts", 0)),
            compact_excerpt(str(p.get("text", "")), 92),
        ])

    top_lines = []
    for p in ctx.top_posts[:6]:
        top_lines.append(
            "- **@" + str(p.get("handle", "unknown")) + "** (" + str(p.get("likes", 0)) + " likes, " + str(p.get("reposts", 0)) + " reposts): " + compact_excerpt(str(p.get("text", "")), 140)
        )
    if len(top_lines) == 0:
        top_lines.append("- No top posts returned for this sample.")

    latest_lines = []
    for p in ctx.latest_posts[:6]:
        latest_lines.append(
            "- **@" + str(p.get("handle", "unknown")) + "** (" + str(p.get("created_at", ""))[:16] + "): " + compact_excerpt(str(p.get("text", "")), 140)
        )
    if len(latest_lines) == 0:
        latest_lines.append("- No latest posts returned for this sample.")

    digest = (
        "# Bluesky Radar: " + ctx.query + "\n"
        + "Sample #" + str(next_samples) + " · " + str(len(combined)) + " unique posts (" + str(new_count) + " newly discovered).\n\n"
        + "## Top Posts\n"
        + "\n".join(top_lines)
        + "\n\n## Latest Posts\n"
        + "\n".join(latest_lines)
    )

    err_parts = []
    if ctx.latest_error != "":
        err_parts.append("latest: " + ctx.latest_error)
    if ctx.top_error != "":
        err_parts.append("top: " + ctx.top_error)

    if len(err_parts) > 0:
        a_title = "Partial Sample Warning"
        a_msg = "Fetch error (" + "; ".join(err_parts) + "). Showing available results."
        s_label = "Sample #" + str(next_samples) + " (partial)"
    elif new_count > 0:
        a_title = str(new_count) + " New Post(s) Discovered"
        a_msg = "Tracking `" + ctx.query + "` across latest + top feeds (" + str(len(seen)) + " total seen)."
        s_label = "Sample #" + str(next_samples) + " · " + str(len(combined)) + " posts"
    else:
        a_title = "Feed Up to Date"
        a_msg = "No new posts since last sample for `" + ctx.query + "`."
        s_label = "Sample #" + str(next_samples) + " · steady"

    return update(
        ctx,
        sample_count = next_samples,
        new_this_tick = new_count,
        seen_uris = seen,
        status_label = s_label,
        alert_title = a_title,
        alert_message = a_msg,
        summary_kv = {
            "query": ctx.query,
            "samples": next_samples,
            "unique_posts": len(combined),
            "new_this_tick": new_count,
        },
        posts_rows = rows,
        digest_md = digest,
        latest_retries = 0,
        top_retries = 0,
    )

def apply_query_update(ctx: RadarCtx, ev: dict) -> RadarCtx:
    raw_q = ev.get("query", ctx.query)
    next_q = str(raw_q).strip() if raw_q != None else ctx.query
    if next_q == "":
        next_q = ctx.query
    raw_limit = ev.get("limit", ctx.limit)
    next_limit = int(raw_limit) if type(raw_limit) == "int" and raw_limit > 0 else ctx.limit
    changed = next_q != ctx.query
    return update(
        ctx,
        query = next_q,
        limit = next_limit,
        seen_uris = [] if changed else ctx.seen_uris,
        sample_count = 0 if changed else ctx.sample_count,
        latest_retries = 0,
        top_retries = 0,
    )

def capture_curated_notes(ctx: RadarCtx, ev: dict) -> RadarCtx:
    incoming = ev.get("comments", [])
    if type(incoming) != "list":
        incoming = []
    return update(ctx, curated_notes = incoming)

chart: StateMachine = machine(
    id = "bluesky_radar",
    description = "Live Bluesky topic radar sampling latest + top feeds in parallel with backoff, auto-poll toggle, and digest curation",
    initial = S.sampling,
    context = RadarCtx(),
    states = {
        S.sampling: parallel_state(
            states = {
                S.latest_region: compound_state(
                    initial = S.fetching_latest,
                    states = {
                        S.fetching_latest: state(
                            invoke = invoke_http(
                                id = "bsky_latest",
                                request = build_latest_request,
                                on_done = [
                                    outcome(
                                        target = S.latest_done,
                                        guard = is_http_ok,
                                        assign = save_latest_posts,
                                    ),
                                    outcome(
                                        target = S.latest_retry_backoff,
                                        guard = can_retry_latest,
                                        assign = lambda ctx, _ev: update(
                                            ctx,
                                            latest_retries = ctx.latest_retries + 1,
                                        ),
                                    ),
                                    outcome(target = S.latest_done, assign = save_latest_error),
                                ],
                                on_error = [
                                    outcome(
                                        target = S.latest_retry_backoff,
                                        guard = can_retry_latest_err,
                                        assign = lambda ctx, _ev: update(
                                            ctx,
                                            latest_retries = ctx.latest_retries + 1,
                                        ),
                                    ),
                                    outcome(target = S.latest_done, assign = save_latest_error),
                                ],
                            ),
                        ),
                        S.latest_retry_backoff: state(
                            after = after(delay = latest_backoff_ms, target = S.fetching_latest),
                        ),
                        S.latest_done: final_state(),
                    },
                ),
                S.top_region: compound_state(
                    initial = S.fetching_top,
                    states = {
                        S.fetching_top: state(
                            invoke = invoke_http(
                                id = "bsky_top",
                                request = build_top_request,
                                on_done = [
                                    outcome(
                                        target = S.top_done,
                                        guard = is_http_ok,
                                        assign = save_top_posts,
                                    ),
                                    outcome(
                                        target = S.top_retry_backoff,
                                        guard = can_retry_top,
                                        assign = lambda ctx, _ev: update(
                                            ctx,
                                            top_retries = ctx.top_retries + 1,
                                        ),
                                    ),
                                    outcome(target = S.top_done, assign = save_top_error),
                                ],
                                on_error = [
                                    outcome(
                                        target = S.top_retry_backoff,
                                        guard = can_retry_top_err,
                                        assign = lambda ctx, _ev: update(
                                            ctx,
                                            top_retries = ctx.top_retries + 1,
                                        ),
                                    ),
                                    outcome(target = S.top_done, assign = save_top_error),
                                ],
                            ),
                        ),
                        S.top_retry_backoff: state(
                            after = after(delay = top_backoff_ms, target = S.fetching_top),
                        ),
                        S.top_done: final_state(),
                    },
                ),
            },
            on_done = outcome(target = S.summarizing),
            on = [
                transition(event = RadarEvent.STOP, target = S.done),
            ],
        ),
        S.summarizing: state(
            always = outcome(target = S.watching, assign = summarize_sample),
        ),
        S.watching: compound_state(
            initial = S.paused,
            states = {
                S.paused: state(
                    on = [
                        transition(event = RadarEvent.START_POLLING, target = S.auto_polling),
                    ],
                ),
                S.auto_polling: state(
                    after = after(delay = poll_delay_ms, target = S.sampling),
                    on = [
                        transition(event = RadarEvent.PAUSE_POLLING, target = S.paused),
                    ],
                ),
            },
            on = [
                transition(
                    event = RadarEvent.SAMPLE_NOW,
                    target = S.sampling,
                    assign = lambda ctx, _ev: update(ctx, latest_retries = 0, top_retries = 0),
                ),
                transition(
                    event = RadarEvent.UPDATE_QUERY,
                    target = S.sampling,
                    assign = apply_query_update,
                ),
                transition(event = RadarEvent.CURATE_DIGEST, target = S.curating),
                transition(event = RadarEvent.STOP, target = S.done),
            ],
        ),
        S.curating: state(
            on = [
                transition(
                    event = RadarEvent.EXPORT_DIGEST,
                    target = S.done,
                    assign = capture_curated_notes,
                ),
                transition(event = RadarEvent.BACK_TO_WATCH, target = S.watching),
                transition(event = RadarEvent.STOP, target = S.done),
            ],
        ),
        S.done: final_state(output = {"status": "exported"}),
    },
)
