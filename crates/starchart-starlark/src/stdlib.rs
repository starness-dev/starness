//! Built-in `@starchart/*` virtual Starlark modules shipped with the engine.

/// Built-in runtime service identifier for `@starchart/http`.
pub const BUILTIN_HTTP_SERVICE: &str = "__starchart_builtin_http__";

/// Built-in runtime service identifier for `@starchart/exec`.
pub const BUILTIN_EXEC_SERVICE: &str = "__starchart_builtin_exec__";

/// Built-in runtime service identifier for `@starchart/acp`.
pub const BUILTIN_ACP_SERVICE: &str = "__starchart_builtin_acp__";

/// Virtual Starlark module source for `@starchart/http`.
pub const HTTP_MODULE_ID: &str = "@starchart/http.star";
pub const HTTP_MODULE_SOURCE: &str = r#"
HttpMethod = enum("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD")

HttpRequest = record(
    method = str,
    url = str,
    headers = field(dict[str, str], {}),
    json = field(typing.Any, None),
    body = field(str | None, None),
    timeout_ms = field(int, 30000),
)

def http_request(
    *,
    url: str,
    method: HttpMethod | str = "GET",
    headers: dict[str, str] = {},
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
    request: HttpRequest | typing.Callable,
    id: str | None = None,
    on_done = None,
    on_error = None,
) -> Invoke:
    return invoke(
        src = "__starchart_builtin_http__",
        id = id,
        input = request,
        on_done = on_done,
        on_error = on_error,
    )
"#;

/// Virtual Starlark module source for `@starchart/exec`.
pub const EXEC_MODULE_ID: &str = "@starchart/exec.star";
pub const EXEC_MODULE_SOURCE: &str = r#"
ExecSpec = record(
    cmd = list[str],
    cwd = field(str | None, None),
    env = field(dict[str, str], {}),
    stdin = field(str | None, None),
    timeout_ms = field(int, 60000),
    check = field(bool, True),
)

def exec_spec(
    *,
    cmd: list[str],
    cwd: str | None = None,
    env: dict[str, str] = {},
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

def invoke_cmd(
    *,
    cmd: list[str] | ExecSpec | typing.Callable,
    id: str | None = None,
    cwd: str | None = None,
    env: dict[str, str] = {},
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
        src = "__starchart_builtin_exec__",
        id = id,
        input = spec_or_fn,
        on_done = on_done,
        on_error = on_error,
    )
"#;

/// Virtual Starlark module source for `@starchart/acp` (also aliased as `@starchart/agents`).
pub const ACP_MODULE_ID: &str = "@starchart/acp.star";
pub const ACP_MODULE_SOURCE: &str = r#"
AgentProtocol = enum("ACP", "CLAUDE_CLI", "CODEX_CLI")

AcpAgent = record(
    id = str,
    protocol = field(str, "ACP"),
    command = str,
    args = field(list[str], []),
    model = field(str, ""),
    cwd = field(str | None, None),
    env = field(dict[str, str], {}),
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
    env: dict[str, str] = {},
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
    env: dict[str, str] = {},
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
    env: dict[str, str] = {},
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
    env: dict[str, str] = {},
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
    prompt: str | typing.Callable,
    id: str | None = None,
    on_done = None,
    on_error = None,
) -> Invoke:
    return invoke(
        src = "__starchart_builtin_acp__",
        id = id if id != None else agent.id,
        input = {
            "agent": agent,
            "prompt": prompt,
        },
        on_done = on_done,
        on_error = on_error,
    )
"#;

/// Resolves a `@starchart/*` virtual module path to `(canonical_module_id, source_code)`.
pub fn resolve_builtin_module(load_path: &str) -> Option<(&'static str, &'static str)> {
    match load_path {
        "@starchart/http" | "@starchart/http.star" | "/@starchart/http.star" => {
            Some((HTTP_MODULE_ID, HTTP_MODULE_SOURCE))
        }
        "@starchart/exec" | "@starchart/exec.star" | "/@starchart/exec.star" => {
            Some((EXEC_MODULE_ID, EXEC_MODULE_SOURCE))
        }
        "@starchart/acp"
        | "@starchart/acp.star"
        | "/@starchart/acp.star"
        | "@starchart/agents"
        | "@starchart/agents.star"
        | "/@starchart/agents.star" => Some((ACP_MODULE_ID, ACP_MODULE_SOURCE)),
        _ => None,
    }
}

/// Returns `true` if `service_name` is one of the built-in `@starchart/*` runtime services.
pub fn is_builtin_service(service_name: &str) -> bool {
    matches!(
        service_name,
        BUILTIN_HTTP_SERVICE | BUILTIN_EXEC_SERVICE | BUILTIN_ACP_SERVICE
    )
}
