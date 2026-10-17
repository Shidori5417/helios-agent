use agent_sandbox::{PermissionClass, PermissionEngine, PermissionError};
use agent_terminal::CancellationToken;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransport {
    Stdio,
    StreamableHttp {
        url: String,
        auth_env: Option<String>,
        session_id: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env_allowlist: Vec<String>,
    pub enabled: bool,
    pub tool_include: Vec<String>,
    pub tool_exclude: Vec<String>,
    pub startup_timeout: Duration,
    pub tool_timeout: Duration,
    pub permission_class: PermissionClass,
    pub transport: McpTransport,
}

#[derive(Clone, Debug)]
pub struct LoadedMcpServer {
    pub config: McpServerConfig,
    pub allowed_hosts: Vec<String>,
    /// Whether a stdio server may open network connections. Off by default.
    pub network_allowed: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct McpConfigFile {
    #[serde(default, rename = "mcp_server")]
    pub servers: Vec<McpServerFile>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct McpServerFile {
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
    #[serde(default)]
    pub tool_include: Vec<String>,
    #[serde(default)]
    pub tool_exclude: Vec<String>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    pub transport: Option<String>,
    pub url: Option<String>,
    pub auth_env: Option<String>,
    /// Let this stdio server open TCP connections (sandbox exception).
    #[serde(default)]
    pub allow_network: bool,
}

fn default_true() -> bool {
    true
}

pub fn load_config(
    path: impl AsRef<std::path::Path>,
    workspace: &std::path::Path,
) -> Result<Vec<LoadedMcpServer>, McpError> {
    let path = path.as_ref().canonicalize().map_err(McpError::Start)?;
    let workspace = workspace.canonicalize().map_err(McpError::Start)?;
    if !path.starts_with(&workspace) {
        return Err(McpError::Protocol("MCP config escapes workspace".into()));
    }
    let document: McpConfigFile =
        toml::from_str(&std::fs::read_to_string(path).map_err(McpError::Start)?)
            .map_err(|error| McpError::Protocol(format!("invalid MCP config: {error}")))?;
    document
        .servers
        .into_iter()
        .map(|server| {
            if server.name.is_empty()
                || server
                    .env_allowlist
                    .iter()
                    .any(|name| !valid_env_name(name) || secret_env_name(name))
                || server
                    .auth_env
                    .as_deref()
                    .is_some_and(|name| !valid_env_name(name))
            {
                return Err(McpError::Protocol(
                    "invalid MCP config secret reference".into(),
                ));
            }
            let cwd = server
                .cwd
                .map(|cwd| workspace.join(cwd))
                .map(|cwd| cwd.canonicalize().map_err(McpError::Start))
                .transpose()?
                .map(|cwd| {
                    if cwd.starts_with(&workspace) {
                        Ok(cwd)
                    } else {
                        Err(McpError::Protocol("MCP cwd escapes workspace".into()))
                    }
                })
                .transpose()?;
            let transport = match server.transport.as_deref().unwrap_or("stdio") {
                "stdio" => McpTransport::Stdio,
                "http" | "streamable_http" => McpTransport::StreamableHttp {
                    url: server
                        .url
                        .clone()
                        .ok_or_else(|| McpError::Protocol("HTTP MCP server requires url".into()))?,
                    auth_env: server.auth_env.clone(),
                    session_id: None,
                },
                other => {
                    return Err(McpError::Protocol(format!(
                        "unsupported MCP transport: {other}"
                    )))
                }
            };
            let allowed_hosts = server
                .allowed_hosts
                .iter()
                .map(|host| normalize_host(host))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LoadedMcpServer {
                config: McpServerConfig {
                    name: server.name,
                    command: server.command,
                    args: server.args,
                    cwd,
                    env_allowlist: server.env_allowlist,
                    enabled: server.enabled,
                    tool_include: server.tool_include,
                    tool_exclude: server.tool_exclude,
                    startup_timeout: Duration::from_secs(10),
                    tool_timeout: Duration::from_secs(30),
                    permission_class: PermissionClass::ReadOnly,
                    transport,
                },
                allowed_hosts,
                network_allowed: server.allow_network,
            })
        })
        .collect()
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "API_KEY", "PRIVATE_KEY"]
        .iter()
        .any(|marker| upper.contains(marker))
}

pub fn normalize_host(host: &str) -> Result<String, McpError> {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() || host.contains('/') || host.contains(':') || host.contains('*') {
        return Err(McpError::Protocol(
            "allowed host must be an exact DNS name or IP".into(),
        ));
    }
    if host.parse::<std::net::IpAddr>().is_ok()
        || (host.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !part.starts_with('-')
                && !part.ends_with('-')
        }))
    {
        Ok(host)
    } else {
        Err(McpError::Protocol("invalid allowed host".into()))
    }
}

pub struct HostAllowlist<P> {
    pub inner: P,
    pub allowed_hosts: Vec<String>,
}
impl<P: McpPolicy> McpPolicy for HostAllowlist<P> {
    fn authorize(&self, class: PermissionClass, tool: &McpTool) -> Result<(), McpError> {
        self.inner.authorize(class, tool)
    }
    fn authorize_network(&self, host: &str) -> Result<(), McpError> {
        let host = normalize_host(host)?;
        if self.allowed_hosts.iter().any(|allowed| allowed == &host) {
            Ok(())
        } else {
            Err(McpError::NetworkDenied(host))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP server is disabled")]
    Disabled,
    #[error("MCP permission denied: {0}")]
    Permission(#[from] PermissionError),
    #[error("MCP process failed to start: {0}")]
    Start(#[from] std::io::Error),
    #[error("MCP response timed out")]
    Timeout,
    #[error("MCP response was cancelled")]
    Cancelled,
    #[error("MCP response exceeded {limit} bytes")]
    Oversized { limit: usize },
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    #[error("MCP network access denied for host: {0}")]
    NetworkDenied(String),
}

pub trait McpPolicy: Send + Sync {
    fn authorize(&self, class: PermissionClass, tool: &McpTool) -> Result<(), McpError>;
    fn authorize_network(&self, _host: &str) -> Result<(), McpError> {
        Err(McpError::NetworkDenied("network policy denied".into()))
    }
}

pub struct PermissionPolicy {
    engine: PermissionEngine,
}
impl PermissionPolicy {
    pub fn new(engine: PermissionEngine) -> Self {
        Self { engine }
    }
}
impl McpPolicy for PermissionPolicy {
    fn authorize(&self, class: PermissionClass, _tool: &McpTool) -> Result<(), McpError> {
        self.engine.authorize(class).map_err(McpError::Permission)
    }
}

pub struct McpClient<P> {
    config: McpServerConfig,
    policy: P,
    child: Option<Child>,
    stdin: Option<Box<dyn Write + Send>>,
    responses: Option<Receiver<Result<Value, McpError>>>,
    tools: HashMap<String, McpTool>,
    next_id: u64,
    max_response_bytes: usize,
    http_session_id: Option<String>,
    child_hook: Option<agent_terminal::ChildHook>,
}

impl<P: McpPolicy> McpClient<P> {
    pub fn new(config: McpServerConfig, policy: P, max_response_bytes: usize) -> Self {
        let http_session_id = match &config.transport {
            McpTransport::StreamableHttp { session_id, .. } => session_id.clone(),
            McpTransport::Stdio => None,
        };
        Self {
            config,
            policy,
            child: None,
            stdin: None,
            responses: None,
            tools: HashMap::new(),
            next_id: 1,
            max_response_bytes,
            http_session_id,
            child_hook: None,
        }
    }

    /// Applies `hook` in the child before exec (for example the workspace
    /// sandbox). Stdio servers only; HTTP servers run in this process.
    pub fn with_child_hook(mut self, hook: Option<agent_terminal::ChildHook>) -> Self {
        self.child_hook = hook;
        self
    }

    pub fn ensure_started(&mut self) -> Result<(), McpError> {
        if !self.config.enabled {
            return Err(McpError::Disabled);
        }
        if let McpTransport::StreamableHttp { url, .. } = &self.config.transport {
            let parsed = reqwest::Url::parse(url)
                .map_err(|error| McpError::Protocol(format!("invalid MCP URL: {error}")))?;
            match parsed.scheme() {
                "http" | "https" => {}
                scheme => {
                    return Err(McpError::Protocol(format!(
                        "unsupported MCP URL scheme: {scheme}"
                    )))
                }
            }
            let host = parsed
                .host_str()
                .ok_or_else(|| McpError::Protocol("MCP URL has no host".into()))?;
            self.policy.authorize_network(host)?;
            return Ok(());
        }
        if self.child.is_some() {
            return Ok(());
        }
        let mut command = Command::new(&self.config.command);
        command
            .args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(cwd) = &self.config.cwd {
            command.current_dir(cwd);
        }
        command.env_clear();
        for name in &self.config.env_allowlist {
            if let Ok(value) = std::env::var(name) {
                command.env(name, value);
            }
        }
        #[cfg(unix)]
        if let Some(hook) = self.child_hook.clone() {
            use std::os::unix::process::CommandExt;
            // SAFETY: the hook only applies a prebuilt sandbox (syscalls, no
            // allocation), which is the contract of `ChildHook`.
            unsafe {
                command.pre_exec(move || hook());
            }
        }
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("missing stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("missing stdout".into()))?;
        let (sender, receiver) = mpsc::channel();
        let max = self.max_response_bytes;
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_frame(&mut reader, max) {
                    Ok(value) => {
                        if sender.send(Ok(value)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        self.stdin = Some(Box::new(stdin));
        self.responses = Some(receiver);
        self.child = Some(child);
        Ok(())
    }

    pub fn initialize(&mut self, cancellation: &CancellationToken) -> Result<Value, McpError> {
        if self.responses.is_none() {
            self.ensure_started()?;
        }
        let response = self.request("initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agent-cli","version":"0.1.0"}}), cancellation)?;
        let _ = self.notify("notifications/initialized", json!({}));
        Ok(response)
    }

    pub fn discover_tools(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<McpTool>, McpError> {
        let response = self.request("tools/list", json!({}), cancellation)?;
        let mut tools = Vec::new();
        for value in response
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::Protocol("tools/list missing tools".into()))?
        {
            let tool: McpTool = serde_json::from_value(value.clone())
                .map_err(|error| McpError::Protocol(error.to_string()))?;
            if !self.config.tool_include.is_empty()
                && !self.config.tool_include.contains(&tool.name)
            {
                continue;
            }
            if self.config.tool_exclude.contains(&tool.name) {
                continue;
            }
            self.tools.insert(tool.name.clone(), tool.clone());
            tools.push(tool);
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tools)
    }

    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, McpError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| McpError::Protocol(format!("tool not discovered: {name}")))?
            .clone();
        self.policy.authorize(self.config.permission_class, &tool)?;
        self.request(
            "tools/call",
            json!({"name":name,"arguments":arguments}),
            cancellation,
        )
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, McpError> {
        if let McpTransport::StreamableHttp { url, auth_env, .. } = self.config.transport.clone() {
            return self.http_request(&url, auth_env.as_deref(), method, params, cancellation);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::Protocol("server not started".into()))?;
        write_frame(stdin, &request)?;
        let receiver = self
            .responses
            .as_ref()
            .ok_or_else(|| McpError::Protocol("response reader unavailable".into()))?;
        let deadline = std::time::Instant::now() + self.config.tool_timeout;
        loop {
            if cancellation.is_cancelled() {
                return Err(McpError::Cancelled);
            }
            let wait = deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(Duration::from_millis(20));
            if wait.is_zero() {
                return Err(McpError::Timeout);
            }
            match receiver.recv_timeout(wait) {
                Ok(Ok(value)) => {
                    if value.get("error").is_some() {
                        return Err(McpError::Protocol(value.to_string()));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(value));
                }
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(McpError::Protocol("response reader disconnected".into()))
                }
            }
        }
    }

    fn http_request(
        &mut self,
        url: &str,
        auth_env: Option<&str>,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, McpError> {
        if cancellation.is_cancelled() {
            return Err(McpError::Cancelled);
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(self.config.tool_timeout)
            .build()
            .map_err(|error| McpError::Protocol(error.to_string()))?;
        let mut request = client
            .post(url)
            .json(&json!({"jsonrpc":"2.0","id":self.next_id,"method":method,"params":params}));
        self.next_id = self.next_id.saturating_add(1);
        if let Some(session) = self.http_session_id.as_deref() {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(name) = auth_env {
            if let Ok(value) = std::env::var(name) {
                request = request.bearer_auth(value);
            }
        }
        let response = request.send().map_err(|error| {
            if error.is_timeout() {
                McpError::Timeout
            } else {
                McpError::Protocol(error.to_string())
            }
        })?;
        if let Some(session) = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            self.http_session_id = Some(session.to_owned());
        }
        let body = response
            .bytes()
            .map_err(|error| McpError::Protocol(error.to_string()))?;
        if body.len() > self.max_response_bytes {
            return Err(McpError::Oversized {
                limit: self.max_response_bytes,
            });
        }
        let value: Value =
            serde_json::from_slice(&body).map_err(|error| McpError::Protocol(error.to_string()))?;
        if value.get("error").is_some() {
            return Err(McpError::Protocol(value.to_string()));
        }
        Ok(value.get("result").cloned().unwrap_or(value))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::Protocol("server not started".into()))?;
        write_frame(
            stdin,
            &json!({"jsonrpc":"2.0","method":method,"params":params}),
        )
    }
}

impl<P> Drop for McpClient<P> {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn write_frame(writer: &mut impl Write, value: &Value) -> Result<(), McpError> {
    let body = serde_json::to_vec(value).map_err(|error| McpError::Protocol(error.to_string()))?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl BufRead, max: usize) -> Result<Value, McpError> {
    let mut length = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.is_empty() {
            return Err(McpError::Protocol("MCP stream closed".into()));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| McpError::Protocol("invalid content length".into()))?,
            );
        }
    }
    let length = length.ok_or_else(|| McpError::Protocol("missing content length".into()))?;
    if length > max {
        return Err(McpError::Oversized { limit: max });
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|error| McpError::Protocol(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_sandbox::{PermissionEngine, PermissionMode};
    use std::io::Cursor;

    #[test]
    fn stdio_frame_roundtrip_and_bounds() {
        let value = json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &value).expect("write");
        assert_eq!(
            read_frame(&mut Cursor::new(bytes), 4096).expect("read"),
            value
        );
        let mut oversized = Vec::new();
        write_frame(&mut oversized, &json!({"long":"123456"})).expect("write");
        assert!(matches!(
            read_frame(&mut Cursor::new(oversized), 4),
            Err(McpError::Oversized { .. })
        ));
    }

    #[test]
    fn permission_policy_denies_mcp_write() {
        let root = std::env::temp_dir();
        let engine = PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("engine");
        let policy = PermissionPolicy::new(engine);
        let tool = McpTool {
            name: "write".into(),
            description: "write".into(),
            input_schema: json!({}),
        };
        assert!(matches!(
            policy.authorize(PermissionClass::WorkspaceWrite, &tool),
            Err(McpError::Permission(_))
        ));
    }

    #[test]
    fn mock_handshake_discovery_and_call_use_jsonrpc_boundary() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(json!({"id":1,"result":{"serverInfo":{"name":"mock"}}})))
            .expect("handshake");
        sender.send(Ok(json!({"id":2,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}}))).expect("tools");
        sender
            .send(Ok(
                json!({"id":3,"result":{"content":[{"type":"text","text":"ok"}]}}),
            ))
            .expect("call");
        let root = std::env::temp_dir();
        let engine = PermissionEngine::new(&root, PermissionMode::WorkspaceWrite).expect("engine");
        let policy = PermissionPolicy::new(engine);
        let config = McpServerConfig {
            name: "mock".into(),
            command: "unused".into(),
            args: vec![],
            cwd: None,
            env_allowlist: vec![],
            enabled: true,
            tool_include: vec![],
            tool_exclude: vec![],
            startup_timeout: Duration::from_secs(1),
            tool_timeout: Duration::from_secs(1),
            permission_class: PermissionClass::WorkspaceWrite,
            transport: McpTransport::Stdio,
        };
        let mut client = McpClient {
            config,
            policy,
            child: None,
            stdin: Some(Box::new(Vec::<u8>::new())),
            responses: Some(receiver),
            tools: HashMap::new(),
            next_id: 1,
            max_response_bytes: 4096,
            http_session_id: None,
            child_hook: None,
        };
        client
            .initialize(&CancellationToken::default())
            .expect("initialize");
        let tools = client
            .discover_tools(&CancellationToken::default())
            .expect("discover");
        assert_eq!(tools[0].name, "echo");
        assert_eq!(
            client
                .call_tool("echo", json!({"value":"x"}), &CancellationToken::default())
                .expect("call")["content"][0]["text"],
            "ok"
        );
    }

    #[test]
    fn response_timeout_is_bounded() {
        let (_sender, receiver) = mpsc::channel();
        let root = std::env::temp_dir();
        let engine = PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("engine");
        let config = McpServerConfig {
            name: "mock".into(),
            command: "unused".into(),
            args: vec![],
            cwd: None,
            env_allowlist: vec![],
            enabled: true,
            tool_include: vec![],
            tool_exclude: vec![],
            startup_timeout: Duration::from_millis(10),
            tool_timeout: Duration::from_millis(10),
            permission_class: PermissionClass::ReadOnly,
            transport: McpTransport::Stdio,
        };
        let mut client = McpClient {
            config,
            policy: PermissionPolicy::new(engine),
            child: None,
            stdin: Some(Box::new(Vec::<u8>::new())),
            responses: Some(receiver),
            tools: HashMap::new(),
            next_id: 1,
            max_response_bytes: 4096,
            http_session_id: None,
            child_hook: None,
        };
        assert!(matches!(
            client.initialize(&CancellationToken::default()),
            Err(McpError::Timeout)
        ));
    }

    struct LocalNetworkPolicy;
    impl McpPolicy for LocalNetworkPolicy {
        fn authorize(&self, _class: PermissionClass, _tool: &McpTool) -> Result<(), McpError> {
            Ok(())
        }
        fn authorize_network(&self, host: &str) -> Result<(), McpError> {
            if host == "127.0.0.1" {
                Ok(())
            } else {
                Err(McpError::NetworkDenied(host.to_string()))
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn streamable_http_maps_handshake_discovery_call_and_auth() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let server = thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).expect("header");
                    request.push(byte[0]);
                }
                let header = String::from_utf8_lossy(&request);
                assert!(header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer local-secret"));
                let content_length = header
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("Content-Length:")
                            .or_else(|| line.strip_prefix("content-length:"))
                    })
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .expect("request content length");
                let mut request_body = vec![0_u8; content_length];
                stream.read_exact(&mut request_body).expect("request body");
                let body = match index {
                    0 => json!({"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"mock"}}}),
                    1 => {
                        json!({"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}})
                    }
                    _ => {
                        json!({"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"ok"}]}})
                    }
                };
                let bytes = serde_json::to_vec(&body).expect("json");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nMcp-Session-Id: test-session\r\n\r\n",
                    bytes.len()
                )
                .expect("response");
                stream.write_all(&bytes).expect("body");
                stream.flush().expect("flush");
            }
        });
        std::env::set_var("AGENT_MCP_TEST_AUTH", "local-secret");
        let config = McpServerConfig {
            name: "http".into(),
            command: String::new(),
            args: vec![],
            cwd: None,
            env_allowlist: vec![],
            enabled: true,
            tool_include: vec![],
            tool_exclude: vec![],
            startup_timeout: Duration::from_secs(1),
            tool_timeout: Duration::from_secs(1),
            permission_class: PermissionClass::ReadOnly,
            transport: McpTransport::StreamableHttp {
                url: format!("http://{address}"),
                auth_env: Some("AGENT_MCP_TEST_AUTH".into()),
                session_id: None,
            },
        };
        let mut client = McpClient::new(config, LocalNetworkPolicy, 4096);
        client
            .initialize(&CancellationToken::default())
            .expect("initialize");
        assert_eq!(
            client
                .discover_tools(&CancellationToken::default())
                .expect("discover")[0]
                .name,
            "echo"
        );
        assert_eq!(
            client
                .call_tool("echo", json!({}), &CancellationToken::default())
                .expect("call")["content"][0]["text"],
            "ok"
        );
        server.join().expect("server");
        std::env::remove_var("AGENT_MCP_TEST_AUTH");
    }

    #[test]
    fn network_policy_denies_http_by_default() {
        let config = McpServerConfig {
            name: "http".into(),
            command: String::new(),
            args: vec![],
            cwd: None,
            env_allowlist: vec![],
            enabled: true,
            tool_include: vec![],
            tool_exclude: vec![],
            startup_timeout: Duration::from_secs(1),
            tool_timeout: Duration::from_secs(1),
            permission_class: PermissionClass::ReadOnly,
            transport: McpTransport::StreamableHttp {
                url: "http://127.0.0.1:1".into(),
                auth_env: None,
                session_id: None,
            },
        };
        let root = std::env::temp_dir();
        let policy = PermissionPolicy::new(
            PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("engine"),
        );
        let mut client = McpClient::new(config, policy, 4096);
        assert!(matches!(
            client.initialize(&CancellationToken::default()),
            Err(McpError::NetworkDenied(_))
        ));
    }

    #[test]
    fn config_loads_typed_servers_and_rejects_workspace_escape() {
        let root = std::env::temp_dir().join(format!("agent-mcp-config-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".agent")).expect("root");
        let config = root.join(".agent/config.toml");
        std::fs::write(
            &config,
            "[[mcp_server]]\nname='local'\ncommand='mock'\nenabled=true\ntransport='stdio'\nenv_allowlist=['MCP_MODE']\ntool_include=['echo']\n",
        )
        .expect("config");
        let loaded = load_config(&config, &root).expect("load");
        assert_eq!(loaded[0].config.name, "local");
        assert_eq!(loaded[0].config.tool_include, vec!["echo"]);
        assert!(load_config(&config, &root).is_ok());
        std::fs::write(
            &config,
            "[[mcp_server]]\nname='bad'\ncommand='mock'\nenv_allowlist=['API_TOKEN']\n",
        )
        .expect("secret config");
        assert!(load_config(&config, &root).is_err());
        assert!(normalize_host("Example.COM").is_ok());
        assert!(normalize_host("https://example.com").is_err());
        assert!(normalize_host("*.example.com").is_err());
        let outside = std::env::temp_dir().join("agent-mcp-outside.toml");
        std::fs::write(&outside, "").expect("outside");
        assert!(load_config(&outside, &root).is_err());
    }

    /// A stdio "server" that tries to write outside the workspace and open a
    /// TCP connection. Under the sandbox neither side effect may happen.
    #[cfg(target_os = "linux")]
    #[test]
    fn stdio_server_is_confined_to_the_workspace() {
        use agent_sandbox::landlock::{status, workspace_sandbox, Status};
        if status() == Status::Unavailable {
            eprintln!("skipped: kernel does not provide Landlock");
            return;
        }
        let root = std::env::temp_dir().join(format!("agent-mcp-sandbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let root = root.canonicalize().expect("canonical");
        let outside = std::path::PathBuf::from("/var/tmp")
            .join(format!("agent-mcp-escape-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let script = format!(
            "echo leak > {} ; python3 -c \"import socket; socket.create_connection(('127.0.0.1',{port}),timeout=2)\" 2>/dev/null; echo done",
            outside.display()
        );
        let config = McpServerConfig {
            name: "probe".into(),
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            cwd: Some(root.clone()),
            env_allowlist: vec!["PATH".into()],
            enabled: true,
            tool_include: vec![],
            tool_exclude: vec![],
            startup_timeout: Duration::from_secs(5),
            tool_timeout: Duration::from_secs(5),
            permission_class: PermissionClass::ReadOnly,
            transport: McpTransport::Stdio,
        };
        let sandbox = workspace_sandbox(&root, &[], false)
            .expect("ruleset")
            .expect("supported kernel");
        let hook: agent_terminal::ChildHook =
            std::sync::Arc::new(move || sandbox.restrict_current());
        let engine = PermissionEngine::new(&root, PermissionMode::WorkspaceWrite).expect("engine");
        let mut client = McpClient::new(config, PermissionPolicy::new(engine), 64 * 1024)
            .with_child_hook(Some(hook));
        client.ensure_started().expect("spawn");
        std::thread::sleep(Duration::from_millis(800));
        assert!(!outside.exists(), "MCP server escaped the workspace");
        assert!(
            listener.accept().is_err(),
            "MCP server opened a TCP connection"
        );
        // Control: the same server without the hook does write outside, so the
        // absence above is caused by the sandbox and not by the probe.
        let control_engine =
            PermissionEngine::new(&root, PermissionMode::WorkspaceWrite).expect("engine");
        let mut control = McpClient::new(
            McpServerConfig {
                name: "control".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), format!("echo leak > {}", outside.display())],
                cwd: Some(root.clone()),
                env_allowlist: vec!["PATH".into()],
                enabled: true,
                tool_include: vec![],
                tool_exclude: vec![],
                startup_timeout: Duration::from_secs(5),
                tool_timeout: Duration::from_secs(5),
                permission_class: PermissionClass::ReadOnly,
                transport: McpTransport::Stdio,
            },
            PermissionPolicy::new(control_engine),
            64 * 1024,
        );
        control.ensure_started().expect("spawn control");
        std::thread::sleep(Duration::from_millis(500));
        assert!(outside.exists(), "control server did not write outside");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }
}
