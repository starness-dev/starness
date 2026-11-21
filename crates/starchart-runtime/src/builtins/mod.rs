//! Built-in `@starchart/*` runtime drivers (`http`, `exec`, `acp`, `mcp`, `decide`, `secrets`).

pub mod acp;
pub mod decide;
pub mod exec;
pub mod http;
pub mod mcp;
pub mod secrets;

use starchart_starlark::{
    BUILTIN_ACP_SERVICE, BUILTIN_DECIDE_SERVICE, BUILTIN_EXEC_SERVICE, BUILTIN_HTTP_SERVICE,
    BUILTIN_MCP_SERVICE,
};

use crate::HostCallbacks;

/// Registers all built-in `@starchart/*` async drivers (`http`, `exec`, `acp`, `mcp`, `decide`) onto `callbacks`.
pub fn register_builtin_drivers(callbacks: HostCallbacks) -> HostCallbacks {
    callbacks
        .on_async_service(BUILTIN_HTTP_SERVICE, http::execute_http_request)
        .on_async_service(BUILTIN_EXEC_SERVICE, exec::execute_command)
        .on_async_service(BUILTIN_ACP_SERVICE, acp::execute_acp_turn)
        .on_async_service(BUILTIN_MCP_SERVICE, mcp::execute_mcp_client_call)
        .on_async_service(BUILTIN_DECIDE_SERVICE, decide::execute_decision)
}
