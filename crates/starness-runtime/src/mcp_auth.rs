//! MCP OAuth 2.1 browser login (`starness auth login`) and OS Keychain-backed
//! [`rmcp::transport::auth::CredentialStore`].
//!
//! Credentials are stored under Keychain service `starness.mcp.oauth` with account:
//! - `<machine_id>/<normalized_url>` when scoped to a specific machine (`--machine <id>`), or
//! - `<normalized_url>` for the global default.
//!
//! At runtime, `@starness/mcp` resolves machine-scoped credentials first and falls back to the
//! global entry for that server URL.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore, OAuthState,
    StoredCredentials,
};
use secrecy::{ExposeSecret, SecretString};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::builtins::secrets::{
    MCP_OAUTH_KEYCHAIN_SERVICE, keychain_delete, keychain_exists, keychain_get_optional,
    keychain_get_scoped, keychain_set, scoped_keychain_account,
};

/// Normalizes an MCP server URL for consistent Keychain account lookups.
pub fn normalize_mcp_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_owned()
}

/// OS Keychain-backed [`CredentialStore`] for `rmcp`'s OAuth 2.1 [`AuthorizationManager`].
///
/// Honors `STARNESS_KEYCHAIN_MOCK_FILE` for headless CI / integration tests.
#[derive(Debug, Clone)]
pub struct KeychainCredentialStore {
    url: String,
    read_machine_id: Option<String>,
    write_machine_id: Option<String>,
}

impl KeychainCredentialStore {
    /// Creates a store for runtime reads (checking `machine_id` first, then global fallback)
    /// and token-refresh writes (writing back to whichever scope held the credentials).
    pub fn for_runtime(url: &str, machine_id: Option<&str>) -> Self {
        let norm_url = normalize_mcp_url(url);
        let cleaned_machine = machine_id
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned);
        let write_machine = cleaned_machine.as_deref().and_then(|m| {
            let scoped = scoped_keychain_account(Some(m), &norm_url);
            if keychain_exists(MCP_OAUTH_KEYCHAIN_SERVICE, &scoped).unwrap_or(false) {
                Some(m.to_owned())
            } else {
                None
            }
        });
        Self {
            url: norm_url,
            read_machine_id: cleaned_machine,
            write_machine_id: write_machine,
        }
    }

    /// Creates a store targeting an explicit scope (either global or `--machine <id>`) for login/logout.
    pub fn for_explicit_scope(url: &str, machine_id: Option<&str>) -> Self {
        let norm_url = normalize_mcp_url(url);
        let cleaned = machine_id
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned);
        Self {
            url: norm_url,
            read_machine_id: cleaned.clone(),
            write_machine_id: cleaned,
        }
    }

    pub fn write_account(&self) -> String {
        scoped_keychain_account(self.write_machine_id.as_deref(), &self.url)
    }
}

#[async_trait]
impl CredentialStore for KeychainCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let raw_opt = keychain_get_scoped(
            MCP_OAUTH_KEYCHAIN_SERVICE,
            &self.url,
            self.read_machine_id.as_deref(),
        )
        .map_err(|e| AuthError::CredentialStoreError(format!("{e:#}")))?;

        let Some(raw) = raw_opt else {
            return Ok(None);
        };
        let creds: StoredCredentials = serde_json::from_str(raw.expose_secret()).map_err(|e| {
            AuthError::CredentialStoreError(format!("corrupt MCP OAuth credentials JSON: {e}"))
        })?;
        Ok(Some(creds))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let encoded = serde_json::to_string(&credentials).map_err(|e| {
            AuthError::CredentialStoreError(format!("failed serializing OAuth credentials: {e}"))
        })?;
        let account = self.write_account();
        keychain_set(
            MCP_OAUTH_KEYCHAIN_SERVICE,
            &account,
            &SecretString::from(encoded),
        )
        .map_err(|e| AuthError::CredentialStoreError(format!("{e:#}")))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let account = self.write_account();
        let _ = keychain_delete(MCP_OAUTH_KEYCHAIN_SERVICE, &account)
            .map_err(|e| AuthError::CredentialStoreError(format!("{e:#}")))?;
        Ok(())
    }
}

/// Result summary returned after a successful interactive `starness auth login`.
#[derive(Debug, Clone)]
pub struct McpOAuthLoginSummary {
    pub url: String,
    pub service: String,
    pub account: String,
}

/// Runs an interactive OAuth 2.1 PKCE browser login flow for a remote MCP server and persists
/// the resulting credentials in the OS Keychain (`starness.mcp.oauth`).
pub async fn mcp_oauth_login(
    url: &str,
    machine_id: Option<&str>,
    scopes: &[String],
    no_open: bool,
) -> anyhow::Result<McpOAuthLoginSummary> {
    let norm_url = normalize_mcp_url(url);
    if norm_url.is_empty() {
        bail!("MCP server URL must not be empty");
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed binding local OAuth callback listener on 127.0.0.1")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let store = KeychainCredentialStore::for_explicit_scope(&norm_url, machine_id);
    let account = store.write_account();

    let mut manager = AuthorizationManager::new(&norm_url)
        .await
        .with_context(|| format!("failed creating OAuth manager for `{norm_url}`"))?;
    manager.set_credential_store(store);

    let mut oauth = OAuthState::Unauthorized(manager);
    let mut auth_req = AuthorizationRequest::new(&redirect_uri)
        .with_client_name("starness")
        .with_initial_scope_selection();
    if !scopes.is_empty() {
        auth_req = auth_req.with_scopes(scopes.to_vec());
    }

    oauth
        .start_authorization(auth_req)
        .await
        .with_context(|| format!("OAuth discovery/registration failed for `{norm_url}`"))?;

    let auth_url = oauth
        .get_authorization_url()
        .await
        .context("failed generating OAuth authorization URL")?;

    eprintln!("Authorize Starness with `{norm_url}` in your browser:");
    eprintln!("  {auth_url}");
    if !no_open {
        open_browser(&auth_url);
    }
    eprintln!("Waiting for OAuth callback on {redirect_uri} (timeout: 5 minutes)...");

    let callback = tokio::time::timeout(
        Duration::from_secs(300),
        await_oauth_callback(&listener),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for OAuth browser callback"))??;

    if let Some(err) = callback.error {
        let desc = callback.error_description.unwrap_or_default();
        bail!("OAuth authorization denied: {err} {desc}");
    }

    let code = callback
        .code
        .ok_or_else(|| anyhow::anyhow!("OAuth callback missing `code` parameter"))?;
    let state = callback
        .state
        .ok_or_else(|| anyhow::anyhow!("OAuth callback missing `state` parameter"))?;

    oauth
        .handle_callback_with_issuer(&code, &state, callback.iss.as_deref())
        .await
        .context("failed exchanging OAuth authorization code for token")?;

    Ok(McpOAuthLoginSummary {
        url: norm_url,
        service: MCP_OAUTH_KEYCHAIN_SERVICE.to_owned(),
        account,
    })
}

/// Deletes stored MCP OAuth credentials for `(url, machine_id)` from the OS Keychain.
pub fn mcp_oauth_logout(url: &str, machine_id: Option<&str>) -> anyhow::Result<bool> {
    let norm_url = normalize_mcp_url(url);
    let account = scoped_keychain_account(machine_id, &norm_url);
    keychain_delete(MCP_OAUTH_KEYCHAIN_SERVICE, &account)
}

/// Checks whether stored MCP OAuth credentials exist for `(url, machine_id)` (exact scope or fallback).
pub fn mcp_oauth_status(url: &str, machine_id: Option<&str>) -> anyhow::Result<Option<String>> {
    let norm_url = normalize_mcp_url(url);
    if let Some(m) = machine_id.map(str::trim).filter(|m| !m.is_empty()) {
        let scoped = scoped_keychain_account(Some(m), &norm_url);
        if keychain_get_optional(MCP_OAUTH_KEYCHAIN_SERVICE, &scoped)?.is_some() {
            return Ok(Some(scoped));
        }
    }
    if keychain_get_optional(MCP_OAUTH_KEYCHAIN_SERVICE, &norm_url)?.is_some() {
        return Ok(Some(norm_url));
    }
    Ok(None)
}

/// Loads stored OAuth 2.1 credentials (machine-scoped first, then global fallback) and prepares
/// an [`AuthorizationManager`] ready to wrap a `reqwest::Client` in [`rmcp::transport::auth::AuthClient`].
///
/// Also returns the current access token (if any) as a [`SecretString`] so the caller can register
/// it with [`crate::builtins::secrets::SecretScrubber`].
pub async fn maybe_build_oauth_manager(
    url: &str,
    machine_id: Option<&str>,
) -> anyhow::Result<Option<(AuthorizationManager, Option<SecretString>)>> {
    let norm_url = normalize_mcp_url(url);
    let has_creds =
        keychain_get_scoped(MCP_OAUTH_KEYCHAIN_SERVICE, &norm_url, machine_id)?.is_some();
    if !has_creds {
        return Ok(None);
    }

    let store = KeychainCredentialStore::for_runtime(&norm_url, machine_id);
    let mut manager = AuthorizationManager::new(&norm_url)
        .await
        .with_context(|| format!("failed creating OAuth manager for `{norm_url}`"))?;
    manager.set_credential_store(store);

    let loaded = manager
        .initialize_from_store()
        .await
        .with_context(|| format!("failed loading stored OAuth credentials for `{norm_url}`"))?;
    if !loaded {
        return Ok(None);
    }

    let access_token = manager.get_access_token().await.ok().map(SecretString::from);
    Ok(Some((manager, access_token)))
}

struct OAuthCallbackParams {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn await_oauth_callback(listener: &TcpListener) -> anyhow::Result<OAuthCallbackParams> {
    loop {
        let (stream, _) = listener.accept().await?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut req_line = String::new();
        if reader.read_line(&mut req_line).await? == 0 {
            continue;
        }
        let target = req_line.split_whitespace().nth(1).unwrap_or("/");
        if !target.starts_with("/callback") {
            let _ = write_half
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
            continue;
        }

        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let ok = params.contains_key("code") && !params.contains_key("error");
        let body = if ok {
            "<!doctype html><html><body style=\"font-family:system-ui;padding:2rem\"><h2>Starness MCP login complete</h2><p>You can close this tab and return to your terminal.</p></body></html>"
        } else {
            "<!doctype html><html><body style=\"font-family:system-ui;padding:2rem\"><h2>Starness MCP login failed</h2><p>Check your terminal for details.</p></body></html>"
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = write_half.write_all(resp.as_bytes()).await;
        let _ = write_half.flush().await;

        return Ok(OAuthCallbackParams {
            code: params.get("code").cloned(),
            state: params.get("state").cloned(),
            iss: params.get("iss").cloned(),
            error: params.get("error").cloned(),
            error_description: params.get("error_description").cloned(),
        });
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(percent_decode(k), percent_decode(v));
        }
    }
    map
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_mcp_url_trims_trailing_slashes_and_whitespace() {
        assert_eq!(
            normalize_mcp_url("  https://mcp.linear.app/mcp/  "),
            "https://mcp.linear.app/mcp"
        );
    }

    #[test]
    fn scoped_keychain_account_prefixes_machine_id_when_present() {
        assert_eq!(
            scoped_keychain_account(None, "https://mcp.linear.app/mcp"),
            "https://mcp.linear.app/mcp"
        );
        assert_eq!(
            scoped_keychain_account(Some("issue_autopilot"), "https://mcp.linear.app/mcp"),
            "issue_autopilot/https://mcp.linear.app/mcp"
        );
        assert_eq!(
            scoped_keychain_account(Some("  "), "OPENAI_API_KEY"),
            "OPENAI_API_KEY"
        );
    }
}
