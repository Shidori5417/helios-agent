//! Interactive MCP login: `helios mcp login|logout <server>`.
//!
//! The access token is kept in the system keyring under `mcp-oauth:<server>`,
//! never in a config file, an environment variable, or model context. There is
//! no refresh token: when the token expires the user logs in again.

use super::*;

const TOKEN_FORMAT_VERSION: u64 = 1;

pub(crate) fn login_account(server: &str) -> String {
    format!("mcp-oauth:{server}")
}

pub(crate) fn encode_token(token: &agent_mcp::LoginToken) -> String {
    serde_json::json!({
        "v": TOKEN_FORMAT_VERSION,
        "access_token": token.value,
        "expires_at_unix": token.expires_at_unix,
    })
    .to_string()
}

pub(crate) fn decode_token(text: &str) -> Option<agent_mcp::LoginToken> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("v")?.as_u64()? != TOKEN_FORMAT_VERSION {
        return None;
    }
    Some(agent_mcp::LoginToken {
        value: value.get("access_token")?.as_str()?.to_owned(),
        expires_at_unix: value.get("expires_at_unix")?.as_u64()?,
    })
}

/// A missing, unreadable or malformed entry is the same as "not logged in".
pub(crate) fn load_login_token(
    store: &dyn SecretStore,
    server: &str,
) -> Option<agent_mcp::LoginToken> {
    store
        .get(&login_account(server))
        .ok()
        .flatten()
        .and_then(|text| decode_token(&text))
}

/// Gives a freshly built client the stored login token when its server uses login.
pub(crate) fn attach_login<P: agent_mcp::McpPolicy>(
    client: McpClient<P>,
    server: &str,
    login_required: bool,
) -> McpClient<P> {
    if login_required {
        client.with_login(true, load_login_token(&KeyringSecretStore, server))
    } else {
        client
    }
}

pub(crate) fn mcp_login_command(
    id: &str,
    workspace: &Path,
    config_path: &Path,
) -> Result<String, String> {
    mcp_login_with_store(id, workspace, config_path, &KeyringSecretStore)
}

pub(crate) fn mcp_login_with_store(
    id: &str,
    workspace: &Path,
    config_path: &Path,
    store: &dyn SecretStore,
) -> Result<String, String> {
    let server = load_mcp_config(config_path, workspace)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|server| server.config.name == id)
        .ok_or("MCP server not found")?;
    let login = server
        .oauth_login
        .clone()
        .ok_or("this server has no oauth_authorization_url, so there is nothing to log in to")?;
    // Exchanging the code is a network request made by Helios, so it follows the
    // same host allowlist as the server's own traffic.
    let host = reqwest::Url::parse(&login.token_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .ok_or("invalid oauth_token_url")?;
    let host = agent_mcp::normalize_host(&host).map_err(|error| error.to_string())?;
    if !server.allowed_hosts.contains(&host) {
        return Err(format!(
            "token host {host} is not in this server's allowed_hosts"
        ));
    }
    let client_id = env::var(&login.client_id_env)
        .map_err(|_| format!("environment variable {} is not set", login.client_id_env))?;
    // Fail before the browser step if the token could not be saved afterwards.
    store.get(&login_account(id))?;
    let request = agent_mcp::oauth_pkce::LoginRequest {
        authorize_url: &login.authorization_url,
        token_url: &login.token_url,
        client_id: &client_id,
        scope: login.scope.as_deref(),
    };
    let token = agent_mcp::oauth_pkce::login(&request, |url| {
        println!("Open this URL in a browser to sign in:\n{url}");
        println!("Waiting up to 3 minutes for the redirect on 127.0.0.1 ...");
    })
    .map_err(|error| error.to_string())?;
    store.set(&login_account(id), &encode_token(&token))?;
    Ok(format!(
        "logged in to {id}; the token is stored in the system keyring"
    ))
}

pub(crate) fn mcp_logout_command(id: &str) -> Result<String, String> {
    mcp_logout_with_store(id, &KeyringSecretStore)
}

pub(crate) fn mcp_logout_with_store(id: &str, store: &dyn SecretStore) -> Result<String, String> {
    store.delete(&login_account(id))?;
    Ok(format!("logged out of {id}; the stored token was removed"))
}
