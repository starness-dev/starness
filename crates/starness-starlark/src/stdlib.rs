//! Built-in `@starness/*` virtual Starlark modules shipped with the engine.

/// Built-in runtime service identifier for `@starness/http`.
pub const BUILTIN_HTTP_SERVICE: &str = "__starness_builtin_http__";

/// Built-in runtime service identifier for `@starness/exec`.
pub const BUILTIN_EXEC_SERVICE: &str = "__starness_builtin_exec__";

/// Built-in runtime service identifier for `@starness/acp`.
pub const BUILTIN_ACP_SERVICE: &str = "__starness_builtin_acp__";

/// Built-in runtime service identifier for `@starness/mcp` (client).
pub const BUILTIN_MCP_SERVICE: &str = "__starness_builtin_mcp__";

/// Built-in runtime service identifier for `@starness/decide` (`decision-rs`).
pub const BUILTIN_DECIDE_SERVICE: &str = "__starness_builtin_decide__";

/// Built-in runtime service identifier for `@starness/llm` (`genai`).
pub const BUILTIN_LLM_SERVICE: &str = "__starness_builtin_llm__";

/// Built-in runtime service identifier for the prelude `invoke_all` fan-out driver.
pub const BUILTIN_BATCH_SERVICE: &str = "__starness_builtin_batch__";

/// Virtual Starlark module source for `@starness/secrets`.
pub const SECRETS_MODULE_ID: &str = "@starness/secrets.star";
pub const SECRETS_MODULE_SOURCE: &str = r#"
SecretSource = enum("env", "file", "keychain")

secret = __starness_secret__
"#;

/// Virtual Starlark module source for `@starness/http`.
pub const HTTP_MODULE_ID: &str = "@starness/http.star";
pub const HTTP_MODULE_SOURCE: &str = r#"
HttpMethod = enum("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD")

HttpRequest = record(
    method = str,
    url = str,
    headers = field(dict[str, str | Secret], {}),
    json = field(typing.Any, None),
    body = field(str | None, None),
    timeout_ms = field(int, 30000),
)

def http_request(
    *,
    url: str,
    method: HttpMethod | str = "GET",
    headers: dict[str, str | Secret] = {},
    json = None,
    body: str | None = None,
    timeout_ms: int = 30000,
) -> HttpRequest:
    method_str = method.value if type(method) == "enum" else str(method)
    if timeout_ms <= 0:
        fail("http_request `timeout_ms` must be > 0")
    return HttpRequest(
        method = method_str,
        url = url,
        headers = headers,
        json = json,
        body = body,
        timeout_ms = timeout_ms,
    )

def invoke_http(
    *,
    request: HttpRequest | typing.Callable[[typing.Any], HttpRequest],
    id: str | None = None,
    on_done = None,
    on_error = None,
) -> Invoke:
    return invoke(
        src = "__starness_builtin_http__",
        id = id,
        input = request,
        on_done = on_done,
        on_error = on_error,
    )
"#;

/// Virtual Starlark module source for `@starness/exec`.
pub const EXEC_MODULE_ID: &str = "@starness/exec.star";
pub const EXEC_MODULE_SOURCE: &str = r#"
ExecSpec = record(
    cmd = list[str],
    cwd = field(str | None, None),
    env = field(dict[str, str | Secret], {}),
    stdin = field(str | None, None),
    timeout_ms = field(int, 60000),
    check = field(bool, True),
)

def exec_spec(
    *,
    cmd: list[str],
    cwd: str | None = None,
    env: dict[str, str | Secret] = {},
    stdin: str | None = None,
    timeout_ms: int = 60000,
    check: bool = True,
) -> ExecSpec:
    if len(cmd) == 0:
        fail("exec_spec `cmd` must contain at least one program argument")
    if timeout_ms <= 0:
        fail("exec_spec `timeout_ms` must be > 0")
    return ExecSpec(
        cmd = cmd,
        cwd = cwd,
        env = env,
        stdin = stdin,
        timeout_ms = timeout_ms,
        check = check,
    )

ExecResult = record(
    code = int,
    stdout = str,
    stderr = str,
    ok = bool,
)

def exec_result(event: dict) -> ExecResult:
    out = event.get("output") if "output" in event else event
    if type(out) != "dict":
        fail("exec_result: event missing `output` dict")
    code = int(out.get("code", 0))
    return ExecResult(
        code = code,
        stdout = str(out.get("stdout", "")),
        stderr = str(out.get("stderr", "")),
        ok = bool(out.get("ok", code == 0)),
    )

def invoke_cmd(
    *,
    cmd: list[str] | ExecSpec | typing.Callable[[typing.Any], list[str] | ExecSpec],
    id: str | None = None,
    cwd: str | None = None,
    env: dict[str, str | Secret] = {},
    stdin: str | None = None,
    timeout_ms: int = 60000,
    check: bool = True,
    on_done = None,
    on_error = None,
) -> Invoke:
    spec_or_fn = (
        exec_spec(
            cmd = cmd,
            cwd = cwd,
            env = env,
            stdin = stdin,
            timeout_ms = timeout_ms,
            check = check,
        )
        if type(cmd) == "list"
        else cmd
    )
    return invoke(
        src = "__starness_builtin_exec__",
        id = id,
        input = spec_or_fn,
        on_done = on_done,
        on_error = on_error,
    )
"#;

/// Virtual Starlark module source for `@starness/acp` (also aliased as `@starness/agents`).
pub const ACP_MODULE_ID: &str = "@starness/acp.star";
pub const ACP_MODULE_SOURCE: &str = r#"
AgentProtocol = enum("ACP", "CLAUDE_CLI", "CODEX_CLI")
ArtifactMode = enum("replace", "append", "once")

artifact_tool = __starness_artifact_tool__
read_tool = __starness_read_tool__
artifact = __starness_artifact__
artifact_list = __starness_artifact_list__

AcpAgent = record(
    id = str,
    protocol = field(str, "ACP"),
    command = str,
    args = field(list[str], []),
    model = field(str, ""),
    cwd = field(str | None, None),
    env = field(dict[str, str | Secret], {}),
    config = field(dict[str, str], {}),
    timeout_ms = field(int, 300000),
)

def acp_agent(
    *,
    id: str,
    command: str,
    args: list[str] = [],
    model: str = "",
    protocol: AgentProtocol | str = "ACP",
    cwd: str | None = None,
    env: dict[str, str | Secret] = {},
    config: dict[str, str] = {},
    timeout_ms: int = 300000,
) -> AcpAgent:
    if id == "":
        fail("acp_agent `id` must not be empty")
    if command == "":
        fail("acp_agent `command` must not be empty")
    if timeout_ms <= 0:
        fail("acp_agent `timeout_ms` must be > 0")
    proto_str = protocol.value if type(protocol) == "enum" else str(protocol)
    return AcpAgent(
        id = id,
        protocol = proto_str,
        command = command,
        args = args,
        model = model,
        cwd = cwd,
        env = env,
        config = config,
        timeout_ms = timeout_ms,
    )

def agy_agent(
    *,
    id: str = "agy",
    command: str = "agy",
    args: list[str] = ["acp"],
    model: str = "",
    cwd: str | None = None,
    env: dict[str, str | Secret] = {},
    config: dict[str, str] = {},
    timeout_ms: int = 300000,
) -> AcpAgent:
    return acp_agent(
        id = id,
        protocol = AgentProtocol.ACP,
        command = command,
        args = args,
        model = model,
        cwd = cwd,
        env = env,
        config = config,
        timeout_ms = timeout_ms,
    )

def claude_agent(
    *,
    id: str = "claude",
    command: str = "claude",
    args: list[str] = [],
    model: str = "",
    use_acp_adapter: bool = False,
    permission_mode: str = "",
    cwd: str | None = None,
    env: dict[str, str | Secret] = {},
    config: dict[str, str] = {},
    timeout_ms: int = 300000,
) -> AcpAgent:
    merged_cfg = dict(config)
    if permission_mode != "":
        merged_cfg["permission_mode"] = permission_mode
    if use_acp_adapter:
        return acp_agent(
            id = id,
            protocol = AgentProtocol.ACP,
            command = "npx",
            args = ["-y", "@agentclientprotocol/claude-agent-acp"] + args,
            model = model,
            cwd = cwd,
            env = env,
            config = merged_cfg,
            timeout_ms = timeout_ms,
        )
    return acp_agent(
        id = id,
        protocol = AgentProtocol.CLAUDE_CLI,
        command = command,
        args = args,
        model = model,
        cwd = cwd,
        env = env,
        config = merged_cfg,
        timeout_ms = timeout_ms,
    )

def codex_agent(
    *,
    id: str = "codex",
    command: str = "codex",
    args: list[str] = [],
    model: str = "",
    use_acp_adapter: bool = False,
    sandbox: str = "",
    cwd: str | None = None,
    env: dict[str, str | Secret] = {},
    config: dict[str, str] = {},
    timeout_ms: int = 300000,
) -> AcpAgent:
    merged_cfg = dict(config)
    if sandbox != "":
        merged_cfg["sandbox"] = sandbox
    if use_acp_adapter:
        return acp_agent(
            id = id,
            protocol = AgentProtocol.ACP,
            command = "npx",
            args = ["-y", "@agentclientprotocol/codex-acp"] + args,
            model = model,
            cwd = cwd,
            env = env,
            config = merged_cfg,
            timeout_ms = timeout_ms,
        )
    return acp_agent(
        id = id,
        protocol = AgentProtocol.CODEX_CLI,
        command = command,
        args = args,
        model = model,
        cwd = cwd,
        env = env,
        config = merged_cfg,
        timeout_ms = timeout_ms,
    )

def invoke_agent(
    *,
    agent: AcpAgent,
    prompt: str | typing.Callable[[typing.Any], str],
    context: dict | typing.Callable[[typing.Any], dict] | None = None,
    tools: list[AgentTool] = [],
    id: str | None = None,
    on_done = None,
    on_error = None,
) -> Invoke:
    seen_tools = {}
    for t in tools:
        if t.name in seen_tools:
            fail("duplicate MCP tool name `" + t.name + "` in invoke_agent(...)")
        seen_tools[t.name] = True
    return invoke(
        src = "__starness_builtin_acp__",
        id = id if id != None else agent.id,
        input = {
            "agent": agent,
            "prompt": prompt,
            "context": context,
            "tools": tools,
        },
        on_done = on_done,
        on_error = on_error,
    )
"#;

/// Virtual Starlark module source for `@starness/mcp` (deterministic MCP client invocations).
pub const MCP_MODULE_ID: &str = "@starness/mcp.star";
pub const MCP_MODULE_SOURCE: &str = r#"
McpTransport = enum("STDIO", "HTTP")

McpServer = record(
    id = str,
    transport = field(str, "STDIO"),
    command = field(str, ""),
    args = field(list[str], []),
    cwd = field(str | None, None),
    env = field(dict[str, str | Secret], {}),
    url = field(str, ""),
    headers = field(dict[str, str | Secret], {}),
    timeout_ms = field(int, 30000),
)

def mcp_stdio_server(
    *,
    id: str,
    command: str,
    args: list[str] = [],
    cwd: str | None = None,
    env: dict[str, str | Secret] = {},
    timeout_ms: int = 30000,
) -> McpServer:
    if id == "":
        fail("mcp_stdio_server `id` must not be empty")
    if command == "":
        fail("mcp_stdio_server `command` must not be empty")
    if timeout_ms <= 0:
        fail("mcp_stdio_server `timeout_ms` must be > 0")
    return McpServer(
        id = id,
        transport = "STDIO",
        command = command,
        args = args,
        cwd = cwd,
        env = env,
        timeout_ms = timeout_ms,
    )

def mcp_http_server(
    *,
    id: str,
    url: str,
    headers: dict[str, str | Secret] = {},
    timeout_ms: int = 30000,
) -> McpServer:
    if id == "":
        fail("mcp_http_server `id` must not be empty")
    if url == "":
        fail("mcp_http_server `url` must not be empty")
    if timeout_ms <= 0:
        fail("mcp_http_server `timeout_ms` must be > 0")
    return McpServer(
        id = id,
        transport = "HTTP",
        url = url,
        headers = headers,
        timeout_ms = timeout_ms,
    )

def mcp_server(
    *,
    id: str,
    command: str = "",
    args: list[str] = [],
    cwd: str | None = None,
    env: dict[str, str | Secret] = {},
    url: str = "",
    headers: dict[str, str | Secret] = {},
    timeout_ms: int = 30000,
) -> McpServer:
    if command != "" and url != "":
        fail("mcp_server accepts either `command` (stdio) or `url` (http), not both")
    if url != "":
        return mcp_http_server(id = id, url = url, headers = headers, timeout_ms = timeout_ms)
    if command != "":
        return mcp_stdio_server(
            id = id,
            command = command,
            args = args,
            cwd = cwd,
            env = env,
            timeout_ms = timeout_ms,
        )
    fail("mcp_server requires either `command` (stdio) or `url` (http)")

McpResult = record(
    tool = str,
    is_error = bool,
    text = str,
    data = typing.Any,
)

def mcp_result(event: dict) -> McpResult:
    out = event.get("output") if "output" in event else event
    if type(out) != "dict":
        fail("mcp_result: event missing `output` dict")
    return McpResult(
        tool = str(out.get("tool", "")),
        is_error = bool(out.get("is_error", False)),
        text = str(out.get("text", "")),
        data = out.get("data", None),
    )

def invoke_mcp(
    *,
    server: McpServer | typing.Callable[[typing.Any], McpServer],
    tool: str | typing.Callable[[typing.Any], str],
    arguments: dict | typing.Callable[[typing.Any], dict] = {},
    check_tool_error: bool = True,
    id: str | None = None,
    on_done = None,
    on_error = None,
) -> Invoke:
    if type(tool) == "string" and tool == "":
        fail("invoke_mcp `tool` must not be empty")
    resolved_id = id
    if resolved_id == None and type(server) == "record" and type(tool) == "string":
        resolved_id = server.id + "_" + tool
    return invoke(
        src = "__starness_builtin_mcp__",
        id = resolved_id,
        input = {
            "server": server,
            "tool": tool,
            "arguments": arguments,
            "check_tool_error": check_tool_error,
        },
        on_done = on_done,
        on_error = on_error,
    )
"#;

/// Virtual Starlark module source for `@starness/decide` (typed LLM / System One decisions via `decision-rs`).
pub const DECIDE_MODULE_ID: &str = "@starness/decide.star";
pub const DECIDE_MODULE_SOURCE: &str = r#"
DecisionProviderKind = enum("typesafe", "gemini", "openai", "openrouter", "cloudflare")
ImageDetail = enum("auto", "low", "high")

DecisionProvider = record(
    provider = str,
    model = str,
    api_key = field(str | Secret | None, None),
    account_id = field(str | Secret | None, None),
    endpoint = field(str | None, None),
)

DecisionQuestion = record(
    kind = str,
    instructions = str,
    yes = field(str | None, None),
    no = field(str | None, None),
    threshold = field(float, 0.5),
    options = field(list[list[str]], []),
    levels = field(list[list[str]], []),
    optional = field(bool, False),
)

DecisionImage = record(
    path = field(str | None, None),
    base64 = field(str | None, None),
    detail = field(str, "auto"),
)

def _unwrap_key(k) -> str:
    s = str(k.value) if type(k) == "enum" else str(k)
    if s == "":
        fail("decision option/level key must not be empty")
    return s

def _normalize_pairs(items, label: str, allow_bare_items: bool) -> list[list[str]]:
    out = []
    seen = {}
    if type(items) == "dict":
        for k, v in items.items():
            key_str = _unwrap_key(k)
            desc_str = _unwrap_key(v) if type(v) == "enum" else str(v)
            if desc_str == "":
                fail(label + " description for `" + key_str + "` must not be empty")
            if key_str in seen:
                fail("duplicate " + label + " key `" + key_str + "`")
            seen[key_str] = True
            out.append([key_str, desc_str])
    elif type(items) == "list" or type(items) == "tuple":
        for elem in items:
            if type(elem) == "tuple" or type(elem) == "list":
                if len(elem) != 2:
                    fail(label + " pair must have length 2 `(key, description)`")
                key_str = _unwrap_key(elem[0])
                desc_str = _unwrap_key(elem[1]) if type(elem[1]) == "enum" else str(elem[1])
            elif allow_bare_items and (type(elem) == "string" or type(elem) == "enum"):
                key_str = _unwrap_key(elem)
                desc_str = key_str
            else:
                fail(label + " list item must be a `(key, description)` pair or string/enum")
            if desc_str == "":
                fail(label + " description for `" + key_str + "` must not be empty")
            if key_str in seen:
                fail("duplicate " + label + " key `" + key_str + "`")
            seen[key_str] = True
            out.append([key_str, desc_str])
    else:
        fail(label + " must be a dict or list, got `" + type(items) + "`")
    if len(out) < 2:
        fail(label + " requires at least 2 entries")
    return out

def noul(
    instructions: str,
    *,
    yes: str | None = None,
    no: str | None = None,
    threshold: float | int = 0.5,
    optional: bool = False,
) -> DecisionQuestion:
    if instructions == "":
        fail("noul `instructions` must not be empty")
    if (yes == None) != (no == None):
        fail("noul requires both `yes` and `no` criteria when either is specified")
    if yes != None and yes == "":
        fail("noul `yes` criteria must not be empty")
    if no != None and no == "":
        fail("noul `no` criteria must not be empty")
    thresh_f = float(threshold)
    if thresh_f < 0.0 or thresh_f > 1.0:
        fail("noul `threshold` must be between 0.0 and 1.0")
    return DecisionQuestion(
        kind = "noul",
        instructions = instructions,
        yes = yes,
        no = no,
        threshold = thresh_f,
        optional = optional,
    )

def choice(
    instructions: str,
    *,
    options,
    optional: bool = False,
) -> DecisionQuestion:
    if instructions == "":
        fail("choice `instructions` must not be empty")
    pairs = _normalize_pairs(options, "choice `options`", True)
    return DecisionQuestion(
        kind = "choice",
        instructions = instructions,
        options = pairs,
        optional = optional,
    )

def score(
    instructions: str,
    *,
    levels,
    optional: bool = False,
) -> DecisionQuestion:
    if instructions == "":
        fail("score `instructions` must not be empty")
    pairs = _normalize_pairs(levels, "score `levels`", True)
    return DecisionQuestion(
        kind = "score",
        instructions = instructions,
        levels = pairs,
        optional = optional,
    )

def decision_image(
    *,
    path: str | None = None,
    base64: str | None = None,
    detail: ImageDetail | str = "auto",
) -> DecisionImage:
    if (path == None) == (base64 == None):
        fail("decision_image requires exactly one of `path` or `base64`")
    detail_str = str(detail.value) if type(detail) == "enum" else str(detail)
    if detail_str not in ["auto", "low", "high"]:
        fail("decision_image `detail` must be `auto`, `low`, or `high`")
    return DecisionImage(
        path = path,
        base64 = base64,
        detail = detail_str,
    )

def jev(
    model: str = "jev-1.14.0",
    *,
    api_key: str | Secret | None = None,
    endpoint: str | None = None,
) -> DecisionProvider:
    if model == "":
        fail("jev `model` must not be empty")
    resolved_key = api_key
    if resolved_key == None and endpoint == None:
        resolved_key = __starness_secret__("TYPESAFE_API_KEY")
    return DecisionProvider(
        provider = "typesafe",
        model = model,
        api_key = resolved_key,
        endpoint = endpoint,
    )

def gemini_decision(
    model: str = "gemini-decision-1",
    *,
    api_key: str | Secret | None = None,
    endpoint: str | None = None,
) -> DecisionProvider:
    if model == "":
        fail("gemini_decision `model` must not be empty")
    resolved_key = api_key
    if resolved_key == None and endpoint == None:
        resolved_key = __starness_secret__("GEMINI_API_KEY")
    return DecisionProvider(
        provider = "gemini",
        model = model,
        api_key = resolved_key,
        endpoint = endpoint,
    )

def openai_decision(
    model: str = "gpt-6-luna",
    *,
    api_key: str | Secret | None = None,
    endpoint: str | None = None,
) -> DecisionProvider:
    if model == "":
        fail("openai_decision `model` must not be empty")
    resolved_key = api_key
    if resolved_key == None and endpoint == None:
        resolved_key = __starness_secret__("OPENAI_API_KEY")
    return DecisionProvider(
        provider = "openai",
        model = model,
        api_key = resolved_key,
        endpoint = endpoint,
    )

def openrouter_decision(
    model: str = "typesafe/jev-1.14",
    *,
    api_key: str | Secret | None = None,
    endpoint: str | None = None,
) -> DecisionProvider:
    if model == "":
        fail("openrouter_decision `model` must not be empty")
    resolved_key = api_key
    if resolved_key == None and endpoint == None:
        resolved_key = __starness_secret__("OPENROUTER_API_KEY")
    return DecisionProvider(
        provider = "openrouter",
        model = model,
        api_key = resolved_key,
        endpoint = endpoint,
    )

def cloudflare_decision(
    model: str = "clef",
    *,
    account_id: str | Secret | None = None,
    api_key: str | Secret | None = None,
    endpoint: str | None = None,
) -> DecisionProvider:
    if model == "":
        fail("cloudflare_decision `model` must not be empty")
    resolved_acct = account_id
    resolved_key = api_key
    if endpoint == None:
        if resolved_acct == None:
            resolved_acct = __starness_secret__("CLOUDFLARE_ACCOUNT_ID")
        if resolved_key == None:
            resolved_key = __starness_secret__("CLOUDFLARE_API_TOKEN")
    return DecisionProvider(
        provider = "cloudflare",
        model = model,
        api_key = resolved_key,
        account_id = resolved_acct,
        endpoint = endpoint,
    )

def invoke_decision(
    *,
    provider: DecisionProvider | typing.Callable[[typing.Any], DecisionProvider],
    evidence: dict | str | typing.Callable[[typing.Any], typing.Any],
    questions: dict[str, DecisionQuestion],
    images = None,
    timeout_ms: int = 15000,
    id: str | None = None,
    on_done = None,
    on_error = None,
) -> Invoke:
    if len(questions) == 0:
        fail("invoke_decision `questions` must contain at least one question")
    if timeout_ms <= 0 or timeout_ms > 120000:
        fail("invoke_decision `timeout_ms` must be in 1..=120000")
    resolved_id = id
    if resolved_id == None and type(provider) == "record":
        resolved_id = provider.provider + "_decision"
    return invoke(
        src = "__starness_builtin_decide__",
        id = resolved_id,
        input = {
            "provider": provider,
            "evidence": evidence,
            "questions": questions,
            "images": images,
            "timeout_ms": timeout_ms,
        },
        on_done = on_done,
        on_error = on_error,
    )

def answer(event: dict, name: str) -> dict:
    out = event.get("output") if "output" in event else event
    if type(out) != "dict":
        fail("decision `answer`: event missing `output` dict")
    answers = out.get("answers") if "answers" in out else out
    if type(answers) != "dict" or name not in answers:
        fail("decision answer `" + name + "` not found in event")
    ans = answers[name]
    if type(ans) != "dict":
        fail("decision answer `" + name + "` is not a dict")
    return ans

def refused(event: dict, name: str) -> bool:
    return bool(answer(event, name).get("refused", False))

def prob(event: dict, name: str) -> float:
    ans = answer(event, name)
    if bool(ans.get("refused", False)):
        return 0.0
    p = ans.get("probability")
    return float(p) if p != None else 0.0

def is_yes(event: dict, name: str, threshold: float | int | None = None) -> bool:
    ans = answer(event, name)
    if bool(ans.get("refused", False)):
        return False
    p = ans.get("probability")
    if p == None:
        return False
    gate = float(threshold) if threshold != None else float(ans.get("threshold", 0.5))
    return float(p) >= gate

def selected(event: dict, name: str) -> str | None:
    ans = answer(event, name)
    if bool(ans.get("refused", False)):
        return None
    val = ans.get("selected")
    return str(val) if val != None else None

def level(event: dict, name: str) -> str | None:
    ans = answer(event, name)
    if bool(ans.get("refused", False)):
        return None
    val = ans.get("level")
    return str(val) if val != None else None

def score_value(event: dict, name: str) -> float | None:
    ans = answer(event, name)
    if bool(ans.get("refused", False)):
        return None
    val = ans.get("score")
    return float(val) if val != None else None

def confidence(event: dict, name: str) -> float | None:
    ans = answer(event, name)
    if bool(ans.get("refused", False)):
        return None
    val = ans.get("confidence")
    return float(val) if val != None else None
"#;

/// Virtual Starlark module source for `@starness/llm` (chat, tool calling, structured `Record` extraction, attachments, and caching via `genai`).
pub const LLM_MODULE_ID: &str = "@starness/llm.star";
pub const LLM_MODULE_SOURCE: &str = r#"
ReasoningEffort = enum("zero", "minimal", "low", "medium", "high", "xhigh", "max")
CachePolicy = enum("ephemeral", "memory", "5m", "1h", "24h")
ToolChoice = enum("auto", "none", "required")

llm_structured = __starness_llm_structured__

LlmModel = record(
    model = str,
    api_key = field(str | Secret | None, None),
    endpoint = field(str | None, None),
    temperature = field(float | None, None),
    max_tokens = field(int | None, None),
    top_p = field(float | None, None),
    reasoning_effort = field(str | None, None),
)

def _normalize_cache(cache: CachePolicy | str | None) -> str | None:
    if cache == None:
        return None
    raw = str(cache.value) if type(cache) == "enum" else str(cache).lower()
    if raw not in ["ephemeral", "memory", "5m", "1h", "24h"]:
        fail("`cache` must be one of `ephemeral`, `memory`, `5m`, `1h`, or `24h`, got `" + raw + "`")
    return raw

def llm_model(
    model: str,
    *,
    api_key: str | Secret | None = None,
    endpoint: str | None = None,
    temperature: float | int | None = None,
    max_tokens: int | None = None,
    top_p: float | int | None = None,
    reasoning_effort: ReasoningEffort | str | None = None,
) -> LlmModel:
    if model == "":
        fail("llm_model `model` must not be empty")
    temp_f = float(temperature) if temperature != None else None
    if temp_f != None and temp_f < 0.0:
        fail("llm_model `temperature` must be >= 0.0")
    if max_tokens != None and max_tokens <= 0:
        fail("llm_model `max_tokens` must be > 0")
    top_p_f = float(top_p) if top_p != None else None
    if top_p_f != None and (top_p_f <= 0.0 or top_p_f > 1.0):
        fail("llm_model `top_p` must be in (0.0, 1.0]")
    effort_str = None
    if reasoning_effort != None:
        raw_effort = str(reasoning_effort.value) if type(reasoning_effort) == "enum" else str(reasoning_effort).lower()
        if raw_effort == "none":
            raw_effort = "zero"
        if raw_effort not in ["zero", "minimal", "low", "medium", "high", "xhigh", "max"]:
            fail("llm_model `reasoning_effort` must be one of `zero`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`")
        effort_str = raw_effort
    return LlmModel(
        model = model,
        api_key = api_key,
        endpoint = endpoint,
        temperature = temp_f,
        max_tokens = max_tokens,
        top_p = top_p_f,
        reasoning_effort = effort_str,
    )

def attachment(
    *,
    path: str | None = None,
    base64: str | None = None,
    url: str | None = None,
    mime: str | None = None,
    name: str | None = None,
) -> dict:
    sources = (1 if path != None else 0) + (1 if base64 != None else 0) + (1 if url != None else 0)
    if sources != 1:
        fail("attachment requires exactly one of `path`, `base64`, or `url`")
    if path != None and path == "":
        fail("attachment `path` must not be empty")
    if base64 != None:
        if base64 == "":
            fail("attachment `base64` must not be empty")
        if mime == None or mime == "":
            fail("attachment with `base64` requires `mime` (e.g. `image/png`)")
    if url != None:
        if url == "":
            fail("attachment `url` must not be empty")
        if mime == None or mime == "":
            fail("attachment with `url` requires `mime` (e.g. `image/png`)")
    return {
        "path": path,
        "base64": base64,
        "url": url,
        "mime": mime,
        "name": name,
    }

def system_msg(
    text: str,
    *,
    cache: CachePolicy | str | None = None,
) -> dict:
    if text == "":
        fail("system_msg `text` must not be empty")
    return {
        "role": "system",
        "text": text,
        "cache": _normalize_cache(cache),
    }

def user_msg(
    text: str = "",
    *,
    attachments: list = [],
    cache: CachePolicy | str | None = None,
) -> dict:
    if text == "" and len(attachments) == 0:
        fail("user_msg requires non-empty `text` or at least one `attachment(...)`")
    return {
        "role": "user",
        "text": text,
        "attachments": attachments,
        "cache": _normalize_cache(cache),
    }

def assistant_msg(
    event_or_text,
    *,
    cache: CachePolicy | str | None = None,
) -> dict:
    cache_str = _normalize_cache(cache)
    if type(event_or_text) == "string":
        return {
            "role": "assistant",
            "text": event_or_text,
            "cache": cache_str,
        }
    if type(event_or_text) == "dict":
        out = event_or_text.get("output") if "output" in event_or_text else event_or_text
        if type(out) == "dict":
            if "$starness_llm_msg" in out:
                msg = dict(out)
                if cache_str != None:
                    msg["cache"] = cache_str
                return msg
            if "message" in out and type(out["message"]) == "dict":
                msg = dict(out["message"])
                if cache_str != None:
                    msg["cache"] = cache_str
                return msg
            if "text" in out:
                return {
                    "role": "assistant",
                    "text": str(out["text"]),
                    "cache": cache_str,
                }
    fail("assistant_msg expects a string or an @starness/llm `on_done` event dict")

ToolCall = record(
    id = str,
    name = str,
    args = dict,
)

def tool_result(
    call_or_id,
    content,
    *,
    fn_name: str | None = None,
    cache: CachePolicy | str | None = None,
) -> dict:
    call_id = ""
    resolved_fn = fn_name
    if type(call_or_id) == "record":
        call_id = str(call_or_id.id)
        if resolved_fn == None:
            resolved_fn = str(call_or_id.name)
    elif type(call_or_id) == "dict":
        raw_id = call_or_id.get("id")
        call_id = str(raw_id) if raw_id != None else ""
        if resolved_fn == None:
            raw_name = call_or_id.get("name")
            resolved_fn = str(raw_name) if raw_name != None else None
    else:
        call_id = str(call_or_id) if call_or_id != None else ""
    if call_id == "":
        fail("tool_result requires a non-empty tool call `id` or tool-call dict")
    return {
        "role": "tool",
        "call_id": call_id,
        "fn_name": resolved_fn,
        "content": content,
        "cache": _normalize_cache(cache),
    }

def tool_results_msg(
    results: list,
    *,
    cache: CachePolicy | str | None = None,
) -> dict:
    if len(results) == 0:
        fail("tool_results_msg requires at least one tool_result(...)")
    for item in results:
        if type(item) != "dict" or item.get("role") != "tool" or not item.get("call_id"):
            fail("tool_results_msg items must be created with tool_result(...)")
    return {
        "role": "tool",
        "results": results,
        "cache": _normalize_cache(cache),
    }

def tool_def(
    name: str,
    *,
    description: str = "",
    schema = None,
    strict: bool | None = None,
    cache: CachePolicy | str | None = None,
) -> dict:
    if name == "":
        fail("tool_def `name` must not be empty")
    compiled_schema = __starness_derive_llm_schema__(schema)
    return {
        "name": name,
        "description": description,
        "schema": compiled_schema,
        "strict": strict,
        "cache": _normalize_cache(cache),
    }

def invoke_llm(
    *,
    model: LlmModel | str | typing.Callable[[typing.Any], LlmModel | str],
    prompt: str | typing.Callable[[typing.Any], str] | None = None,
    messages: list | typing.Callable[[typing.Any], list] | None = None,
    system: str | typing.Callable[[typing.Any], str] | None = None,
    system_cache: CachePolicy | str | None = None,
    context = None,
    tools: list = [],
    tool_choice: ToolChoice | str | None = None,
    schema = None,
    stream: bool = True,
    timeout_ms: int = 60000,
    id: str | None = None,
    on_done = None,
    on_error = None,
) -> Invoke:
    if prompt == None and messages == None:
        fail("invoke_llm requires `prompt` or `messages`")
    if type(prompt) == "string" and prompt == "" and messages == None:
        fail("invoke_llm `prompt` must not be empty")
    if timeout_ms <= 0:
        fail("invoke_llm `timeout_ms` must be > 0")
    model_or_fn = llm_model(model) if type(model) == "string" else model
    compiled_schema = __starness_derive_llm_schema__(schema)
    choice_str = None
    if tool_choice != None:
        choice_str = str(tool_choice.value) if type(tool_choice) == "enum" else str(tool_choice)
        if choice_str == "":
            fail("invoke_llm `tool_choice` must not be empty")
    return invoke(
        src = "__starness_builtin_llm__",
        id = id,
        input = {
            "model": model_or_fn,
            "prompt": prompt,
            "messages": messages,
            "system": system,
            "system_cache": _normalize_cache(system_cache),
            "context": context,
            "tools": tools,
            "tool_choice": choice_str,
            "schema": compiled_schema,
            "stream": stream,
            "timeout_ms": timeout_ms,
        },
        on_done = on_done,
        on_error = on_error,
    )

def llm_text(event: dict) -> str:
    out = event.get("output") if "output" in event else event
    if type(out) != "dict":
        fail("llm_text: event missing `output` dict")
    txt = out.get("text")
    return str(txt) if txt != None else ""

def llm_tool_calls(event: dict) -> list[ToolCall]:
    out = event.get("output") if "output" in event else event
    if type(out) != "dict":
        fail("llm_tool_calls: event missing `output` dict")
    raw_calls = out.get("tool_calls")
    if raw_calls == None:
        return []
    typed = []
    for c in raw_calls:
        if type(c) == "record":
            typed.append(c)
        elif type(c) == "dict":
            raw_args = c.get("args", {})
            typed.append(ToolCall(
                id = str(c.get("id", "")),
                name = str(c.get("name", "")),
                args = raw_args if type(raw_args) == "dict" else {},
            ))
    return typed

def has_tool_calls(event: dict) -> bool:
    return len(llm_tool_calls(event)) > 0

def llm_tool_call(event: dict) -> ToolCall | None:
    calls = llm_tool_calls(event)
    return calls[0] if len(calls) > 0 else None
"#;

/// Resolves a `@starness/*` virtual module path to `(canonical_module_id, source_code)`.
pub fn resolve_builtin_module(load_path: &str) -> Option<(&'static str, &'static str)> {
    match load_path {
        "@starness/http" | "@starness/http.star" | "/@starness/http.star" => {
            Some((HTTP_MODULE_ID, HTTP_MODULE_SOURCE))
        }
        "@starness/exec" | "@starness/exec.star" | "/@starness/exec.star" => {
            Some((EXEC_MODULE_ID, EXEC_MODULE_SOURCE))
        }
        "@starness/acp"
        | "@starness/acp.star"
        | "/@starness/acp.star"
        | "@starness/agents"
        | "@starness/agents.star"
        | "/@starness/agents.star" => Some((ACP_MODULE_ID, ACP_MODULE_SOURCE)),
        "@starness/mcp" | "@starness/mcp.star" | "/@starness/mcp.star" => {
            Some((MCP_MODULE_ID, MCP_MODULE_SOURCE))
        }
        "@starness/secrets" | "@starness/secrets.star" | "/@starness/secrets.star" => {
            Some((SECRETS_MODULE_ID, SECRETS_MODULE_SOURCE))
        }
        "@starness/decide" | "@starness/decide.star" | "/@starness/decide.star" => {
            Some((DECIDE_MODULE_ID, DECIDE_MODULE_SOURCE))
        }
        "@starness/llm" | "@starness/llm.star" | "/@starness/llm.star" => {
            Some((LLM_MODULE_ID, LLM_MODULE_SOURCE))
        }
        _ => None,
    }
}

/// Returns `true` if `service_name` is one of the built-in `@starness/*` runtime services.
pub fn is_builtin_service(service_name: &str) -> bool {
    matches!(
        service_name,
        BUILTIN_HTTP_SERVICE
            | BUILTIN_EXEC_SERVICE
            | BUILTIN_ACP_SERVICE
            | BUILTIN_MCP_SERVICE
            | BUILTIN_DECIDE_SERVICE
            | BUILTIN_LLM_SERVICE
            | BUILTIN_BATCH_SERVICE
    )
}
