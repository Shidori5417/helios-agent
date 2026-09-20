use agent_terminal::CancellationToken;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::JoinHandle,
    time::Duration,
};

pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LspServerConfig {
    pub name: String,
    pub language: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct LspConfigFile {
    #[serde(default)]
    lsp_server: Vec<LspServerConfig>,
}

fn default_timeout() -> u64 {
    10_000
}

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("LSP I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("LSP protocol failed: {0}")]
    Protocol(String),
    #[error("LSP response exceeded {0} bytes")]
    Oversized(usize),
    #[error("LSP request timed out")]
    Timeout,
    #[error("LSP request cancelled")]
    Cancelled,
    #[error("LSP path escapes workspace")]
    OutsideWorkspace,
}

pub fn load_config(
    path: impl AsRef<Path>,
    workspace: impl AsRef<Path>,
) -> Result<Vec<LspServerConfig>, LspError> {
    let workspace = workspace.as_ref().canonicalize()?;
    let path = path.as_ref().canonicalize()?;
    if !path.starts_with(&workspace) {
        return Err(LspError::OutsideWorkspace);
    }
    let file: LspConfigFile = toml::from_str(&fs::read_to_string(path)?)
        .map_err(|e| LspError::Protocol(e.to_string()))?;
    file.lsp_server
        .into_iter()
        .map(|mut server| {
            validate_config(&mut server, &workspace)?;
            Ok(server)
        })
        .collect()
}

fn validate_config(server: &mut LspServerConfig, workspace: &Path) -> Result<(), LspError> {
    if server.name.is_empty()
        || server.command.is_empty()
        || server.language.is_empty()
        || server.timeout_ms == 0
        || server.timeout_ms > 60_000
    {
        return Err(LspError::Protocol("invalid LSP server config".into()));
    }
    let cwd = server
        .cwd
        .clone()
        .unwrap_or_else(|| workspace.to_path_buf());
    let cwd = workspace.join(cwd).canonicalize()?;
    if !cwd.starts_with(workspace) {
        return Err(LspError::OutsideWorkspace);
    }
    server.cwd = Some(cwd);
    if server.env_allowlist.iter().any(|name| {
        name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    }) {
        return Err(LspError::Protocol(
            "invalid LSP environment allowlist".into(),
        ));
    }
    Ok(())
}

pub trait LspTransport {
    fn write_frame(&mut self, value: &Value) -> Result<(), LspError>;
    fn read_frame(&mut self, max_bytes: usize, timeout: Duration) -> Result<Value, LspError>;
    fn read_frame_cancelable(
        &mut self,
        max_bytes: usize,
        timeout: Duration,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, LspError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(LspError::Cancelled);
        }
        self.read_frame(max_bytes, timeout)
    }
}

pub struct StdioTransport {
    child: Child,
    stdin: std::process::ChildStdin,
    frames: Receiver<Result<Value, LspError>>,
    reader: Option<JoinHandle<()>>,
}

impl StdioTransport {
    pub fn spawn(config: &LspServerConfig, workspace: &Path) -> Result<Self, LspError> {
        let cwd = config.cwd.as_deref().unwrap_or(workspace);
        if !cwd.starts_with(workspace) {
            return Err(LspError::OutsideWorkspace);
        }
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(cwd)
            .env_clear()
            .stdout(Stdio::piped())
            .stdin(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // The child is an untrusted language server; isolate its descendants so
            // cancellation and Drop can terminate the complete process group.
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        for key in &config.env_allowlist {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Protocol("LSP stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Protocol("LSP stdout unavailable".into()))?;
        let (sender, frames) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut reader = io::BufReader::new(stdout);
            loop {
                match read_frame(&mut reader, MAX_RESPONSE_BYTES) {
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
        Ok(Self {
            child,
            stdin,
            frames,
            reader: Some(reader),
        })
    }
}

impl LspTransport for StdioTransport {
    fn write_frame(&mut self, value: &Value) -> Result<(), LspError> {
        write_frame(&mut self.stdin, value)
    }
    fn read_frame(&mut self, max_bytes: usize, timeout: Duration) -> Result<Value, LspError> {
        self.read_frame_cancelable(max_bytes, timeout, None)
    }
    fn read_frame_cancelable(
        &mut self,
        max_bytes: usize,
        timeout: Duration,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, LspError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(LspError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(LspError::Timeout);
            }
            let slice = remaining.min(Duration::from_millis(25));
            match self.frames.recv_timeout(slice) {
                Ok(Ok(value)) if value.to_string().len() <= max_bytes => return Ok(value),
                Ok(Ok(_)) => return Err(LspError::Oversized(max_bytes)),
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(LspError::Protocol("LSP stream closed".into()))
                }
            }
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // Negative pid targets the process group created above.
            let _ = unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub struct LspClient<T> {
    transport: T,
    next_id: u64,
    timeout: Duration,
    max_response_bytes: usize,
    initialized: bool,
}

impl<T: LspTransport> LspClient<T> {
    pub fn new(transport: T, timeout: Duration) -> Self {
        Self {
            transport,
            next_id: 1,
            timeout,
            max_response_bytes: MAX_RESPONSE_BYTES,
            initialized: false,
        }
    }
    pub fn with_response_limit(mut self, limit: usize) -> Self {
        self.max_response_bytes = limit;
        self
    }
    pub fn initialize(&mut self, root: &Path) -> Result<(), LspError> {
        self.initialize_with_cancellation(root, None)
    }
    pub fn initialize_with_cancellation(
        &mut self,
        root: &Path,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(), LspError> {
        let root_uri = format!("file://{}", root.display());
        self.request_with_cancellation(
            "initialize",
            json!({"processId": null, "rootUri": root_uri, "capabilities": {}}),
            cancellation,
        )?;
        self.notify("initialized", json!({}))?;
        self.initialized = true;
        Ok(())
    }
    pub fn did_open(&mut self, uri: &str, language: &str, text: &str) -> Result<(), LspError> {
        self.did_open_with_cancellation(uri, language, text, None)
    }
    pub fn did_open_with_cancellation(
        &mut self,
        uri: &str,
        language: &str,
        text: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(), LspError> {
        self.require_initialized()?;
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(LspError::Cancelled);
        }
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":language,"version":1,"text":text}}),
        )
    }
    pub fn symbols(&mut self, uri: &str) -> Result<Value, LspError> {
        self.symbols_with_cancellation(uri, None)
    }
    pub fn symbols_with_cancellation(
        &mut self,
        uri: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, LspError> {
        self.request_with_cancellation(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
            cancellation,
        )
    }
    pub fn definition(&mut self, uri: &str, line: u32, character: u32) -> Result<Value, LspError> {
        self.definition_with_cancellation(uri, line, character, None)
    }
    pub fn definition_with_cancellation(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, LspError> {
        self.request_with_cancellation(
            "textDocument/definition",
            json!({"textDocument":{"uri":uri},"position":{"line":line,"character":character}}),
            cancellation,
        )
    }
    pub fn references(&mut self, uri: &str, line: u32, character: u32) -> Result<Value, LspError> {
        self.references_with_cancellation(uri, line, character, None)
    }
    pub fn references_with_cancellation(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, LspError> {
        self.request_with_cancellation("textDocument/references", json!({"textDocument":{"uri":uri},"position":{"line":line,"character":character},"context":{"includeDeclaration":true}}), cancellation)
    }
    pub fn diagnostics(&mut self, uri: &str) -> Result<Value, LspError> {
        self.diagnostics_with_cancellation(uri, None)
    }
    pub fn diagnostics_with_cancellation(
        &mut self,
        uri: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, LspError> {
        self.request_with_cancellation(
            "textDocument/diagnostic",
            json!({"textDocument":{"uri":uri}}),
            cancellation,
        )
    }
    pub fn shutdown(&mut self) -> Result<(), LspError> {
        if self.initialized {
            let _ = self.request("shutdown", Value::Null)?;
            self.notify("exit", Value::Null)?;
            self.initialized = false;
        }
        Ok(())
    }
    fn require_initialized(&self) -> Result<(), LspError> {
        if self.initialized {
            Ok(())
        } else {
            Err(LspError::Protocol("LSP client is not initialized".into()))
        }
    }
    fn notify(&mut self, method: &str, params: Value) -> Result<(), LspError> {
        self.transport
            .write_frame(&json!({"jsonrpc":"2.0","method":method,"params":params}))
    }
    fn request(&mut self, method: &str, params: Value) -> Result<Value, LspError> {
        self.request_with_cancellation(method, params, None)
    }
    fn request_with_cancellation(
        &mut self,
        method: &str,
        params: Value,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, LspError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(LspError::Cancelled);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.transport
            .write_frame(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let value = self.transport.read_frame_cancelable(
            self.max_response_bytes,
            self.timeout,
            cancellation,
        )?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(LspError::Protocol("LSP response id mismatch".into()));
        }
        if let Some(error) = value.get("error") {
            return Err(LspError::Protocol(error.to_string()));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
}

fn write_frame(writer: &mut impl Write, value: &Value) -> Result<(), LspError> {
    let body = serde_json::to_vec(value).map_err(|e| LspError::Protocol(e.to_string()))?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl BufRead, max: usize) -> Result<Value, LspError> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(LspError::Protocol("LSP stream closed".into()));
        }
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| LspError::Protocol("invalid content length".into()))?,
            );
        }
    }
    let length = length.ok_or_else(|| LspError::Protocol("missing content length".into()))?;
    if length > max {
        return Err(LspError::Oversized(max));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| LspError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Mock {
        sent: Vec<Value>,
        responses: VecDeque<Value>,
    }
    impl LspTransport for Mock {
        fn write_frame(&mut self, value: &Value) -> Result<(), LspError> {
            self.sent.push(value.clone());
            Ok(())
        }
        fn read_frame(&mut self, max: usize, _timeout: Duration) -> Result<Value, LspError> {
            let value = self.responses.pop_front().ok_or(LspError::Timeout)?;
            if value.to_string().len() > max {
                return Err(LspError::Oversized(max));
            }
            Ok(value)
        }
    }

    fn client() -> LspClient<Mock> {
        LspClient::new(
            Mock {
                sent: Vec::new(),
                responses: VecDeque::from([
                    json!({"jsonrpc":"2.0","id":1,"result":{}}),
                    json!({"jsonrpc":"2.0","id":2,"result":[{"name":"main"}]}),
                ]),
            },
            Duration::from_millis(20),
        )
    }

    #[test]
    fn handshake_and_symbol_mapping_use_typed_jsonrpc() {
        let mut client = client();
        client.initialize(Path::new("/workspace")).expect("init");
        let symbols = client
            .symbols("file:///workspace/main.rs")
            .expect("symbols");
        assert_eq!(symbols[0]["name"], "main");
        assert_eq!(client.transport.sent[0]["method"], "initialize");
    }
    #[test]
    fn framed_reader_rejects_oversized_and_accepts_partial_body() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let frame = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = frame.into_bytes();
        bytes.extend_from_slice(body);
        let value = read_frame(&mut io::Cursor::new(bytes), 1024).expect("frame");
        assert_eq!(value["id"], 1);
        let mut oversized = io::Cursor::new(b"Content-Length: 9\r\n\r\n123456789".to_vec());
        assert!(matches!(
            read_frame(&mut oversized, 4),
            Err(LspError::Oversized(4))
        ));
    }
    #[test]
    fn cancellation_is_checked_before_a_request_is_written() {
        let mut client = client();
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let error = client
            .symbols_with_cancellation("file:///workspace/main.rs", Some(&cancellation))
            .expect_err("cancelled request");
        assert!(matches!(error, LspError::Cancelled));
        assert!(client.transport.sent.is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn slow_stdio_server_is_reaped_after_cancellation() {
        let root = tempfile_root();
        fs::create_dir_all(&root).expect("root");
        let config = LspServerConfig {
            name: "slow".into(),
            language: "text".into(),
            extensions: Vec::new(),
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            cwd: Some(root.clone()),
            env_allowlist: Vec::new(),
            enabled: true,
            timeout_ms: 5_000,
        };
        let transport = StdioTransport::spawn(&config, &root).expect("spawn");
        let pid = transport.child.id();
        let mut client = LspClient::new(transport, Duration::from_secs(5));
        let cancellation = CancellationToken::default();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker_cancel = cancellation.clone();
        let worker = std::thread::spawn(move || {
            ready_tx.send(()).expect("ready");
            client.initialize_with_cancellation(&root, Some(&worker_cancel))
        });
        ready_rx.recv().expect("worker started");
        cancellation.cancel();
        assert!(matches!(
            worker.join().expect("worker join"),
            Err(LspError::Cancelled) | Err(LspError::Timeout)
        ));
        for _ in 0..100 {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return;
            }
            std::thread::yield_now();
        }
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
    #[test]
    fn config_rejects_workspace_escape_and_keeps_server_lazy() {
        let root = tempfile_root();
        fs::create_dir_all(root.join(".agent")).expect("dir");
        fs::write(root.join(".agent/config.toml"), "[[lsp_server]]\nname='rust'\nlanguage='rust'\ncommand='rust-analyzer'\ncwd='../'\nenabled=true\n").expect("config");
        assert!(matches!(
            load_config(root.join(".agent/config.toml"), &root),
            Err(LspError::OutsideWorkspace)
        ));
    }
    fn tempfile_root() -> PathBuf {
        std::env::temp_dir().join(format!("agent-lsp-{}", std::process::id()))
    }
}
