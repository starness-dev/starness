# Live Bluesky Feed Sampler (pure headless `.star` statechart — zero UI coupling):
# - Polls Bluesky's public API (`api.bsky.app`) every 10 seconds for a search query (`"ai harness"`),
#   deduplicating by URI and appending newly discovered posts to a running list.
# - Pair with the external `examples/bluesky_radar.ui.json` view-sheet:
#     starness check examples/bluesky_radar.star --ui examples/bluesky_radar.ui.json
#     starness run -i examples/bluesky_radar.star --ui examples/bluesky_radar.ui.json
#     starness serve examples/bluesky_radar.star --ui examples/bluesky_radar.ui.json --bind 127.0.0.1:8080

load("@starness/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")

RadarEvent = enum("REFRESH", "UPDATE_QUERY", "PAUSE", "RESUME", "STOP")

RadarCtx = record(
    api_base = field(str, "https://api.bsky.app"),
    query = field(str, "ai harness"),
    poll_ms = field(int, 10000),
    ticks = field(int, 0),
    seen_uris = field(list[str], []),
    posts_rows = field(list[list[str]], []),
    status_label = field(str, "Polling every 10s"),
    summary_kv = field(dict, {"query": "ai harness", "ticks": 0, "posts": 0}),
)

S = state_space("fetching", "waiting", "paused", "done")

def encode_query(raw: str) -> str:
    return raw.strip().replace("%", "%25").replace(" ", "%20").replace("#", "%23").replace("&", "%26")

def build_request(ctx: RadarCtx) -> HttpRequest:
    url = ctx.api_base + "/xrpc/app.bsky.feed.searchPosts?q=" + encode_query(ctx.query) + "&limit=5&sort=latest"
    return http_request(method = HttpMethod.GET, url = url, timeout_ms = 10000)

def extract_post_text(p: dict) -> str:
    rec = p.get("record", {})
    text = str(rec.get("text", "")).strip() if type(rec) == "dict" else ""
    extras = []

    # Pull full link URIs from Bluesky richtext facets (Bluesky shortens URLs in `record.text`)
    facets = rec.get("facets", []) if type(rec) == "dict" else []
    if type(facets) == "list":
        for f in facets:
            feats = f.get("features", []) if type(f) == "dict" else []
            if type(feats) == "list":
                for feat in feats:
                    uri = str(feat.get("uri", "")).strip() if type(feat) == "dict" else ""
                    if uri != "" and uri not in text and uri not in extras:
                        extras.append(uri)

    # Pull embed link cards, image alt text, or media labels
    embed = p.get("embed", {})
    if type(embed) == "dict":
        ext = embed.get("external")
        if type(ext) == "dict":
            title = str(ext.get("title", "")).strip()
            uri = str(ext.get("uri", "")).strip()
            if title != "" and uri != "":
                extras.append("[" + title + "](" + uri + ")")
            elif uri != "" and uri not in text:
                extras.append(uri)
        if text == "":
            images = embed.get("images", [])
            if type(images) == "list" and len(images) > 0:
                alts = [str(img.get("alt", "")).strip() for img in images if type(img) == "dict" and str(img.get("alt", "")).strip() != ""]
                if len(alts) > 0:
                    text = "[image] " + " | ".join(alts)
                else:
                    text = "[image post]"
            elif embed.get("playlist") != None or embed.get("thumbnail") != None:
                text = "[video post]"
            elif len(extras) == 0:
                text = "[quote / embed post]"

    if text == "" and len(extras) == 0:
        return "[media / embed post]"
    if len(extras) > 0:
        return (text + "\n" + "\n".join(extras)).strip()
    return text

def post_web_url(handle: str, at_uri: str) -> str:
    parts = at_uri.split("/")
    rkey = parts[-1] if len(parts) > 0 else ""
    if handle != "" and rkey != "":
        return "https://bsky.app/profile/" + handle + "/post/" + rkey
    return ""

def append_posts(ctx: RadarCtx, ev: dict) -> RadarCtx:
    body = ev.get("output", {}).get("body", {})
    raw_posts = body.get("posts", []) if type(body) == "dict" else []
    seen = list(ctx.seen_uris)
    seen_map = {u: True for u in seen}
    new_rows = []

    for p in raw_posts:
        if type(p) != "dict":
            continue
        uri = str(p.get("uri", ""))
        if uri == "" or seen_map.get(uri, False):
            continue
        seen_map[uri] = True
        seen.append(uri)
        author = p.get("author", {})
        handle = str(author.get("handle", "unknown")) if type(author) == "dict" else "unknown"
        url = post_web_url(handle, uri)
        author_cell = "[@" + handle + "](" + url + ")" if url != "" else "@" + handle
        text = extract_post_text(p)
        new_rows.append([author_cell, str(int(p.get("likeCount", 0) or 0)), text])

    # Newest posts at the top, capped at 10 rows
    rows = (new_rows + list(ctx.posts_rows))[:10]
    added = len(new_rows)
    next_ticks = ctx.ticks + 1
    return update(
        ctx,
        ticks = next_ticks,
        seen_uris = seen,
        posts_rows = rows,
        status_label = "Tick #" + str(next_ticks) + " (+" + str(added) + " new) · polling every 10s",
        summary_kv = {"query": ctx.query, "ticks": next_ticks, "posts": len(rows)},
    )

def apply_query(ctx: RadarCtx, ev: dict) -> RadarCtx:
    next_q = str(ev.get("query", ctx.query)).strip() or ctx.query
    return update(
        ctx,
        query = next_q,
        ticks = 0,
        seen_uris = [],
        posts_rows = [],
        status_label = "Fetching `" + next_q + "`…",
        summary_kv = {"query": next_q, "ticks": 0, "posts": 0},
    )

chart: StateMachine = machine(
    id = "bluesky_radar",
    description = "Polls Bluesky every 10s and appends new posts to a live list",
    initial = S.fetching,
    context = RadarCtx(),
    states = {
        S.fetching: state(
            invoke = invoke_http(
                request = build_request,
                on_done = outcome(target = S.waiting, assign = append_posts),
                on_error = outcome(
                    target = S.waiting,
                    assign = lambda ctx, _ev: update(ctx, status_label = "Fetch failed — retrying in 10s"),
                ),
            ),
            on = [transition(event = RadarEvent.STOP, target = S.done)],
        ),
        S.waiting: state(
            after = after(delay = lambda ctx, _ev: ctx.poll_ms, target = S.fetching),
            on = [
                transition(event = RadarEvent.REFRESH, target = S.fetching),
                transition(event = RadarEvent.UPDATE_QUERY, target = S.fetching, assign = apply_query),
                transition(
                    event = RadarEvent.PAUSE,
                    target = S.paused,
                    assign = lambda ctx, _ev: update(ctx, status_label = "Paused"),
                ),
                transition(event = RadarEvent.STOP, target = S.done),
            ],
        ),
        S.paused: state(
            on = [
                transition(event = RadarEvent.RESUME, target = S.fetching),
                transition(event = RadarEvent.UPDATE_QUERY, target = S.fetching, assign = apply_query),
                transition(event = RadarEvent.STOP, target = S.done),
            ],
        ),
        S.done: final_state(output = {"status": "stopped"}),
    },
)
