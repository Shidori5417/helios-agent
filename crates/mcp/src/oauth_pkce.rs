//! Authorization-code login with PKCE (RFC 7636) for HTTP MCP servers.
//!
//! One loopback listener receives the provider's redirect, so the browser never
//! reaches the agent over the network. The code is exchanged together with the
//! PKCE verifier, which lets a public client work without a stored secret. The
//! access token is returned to the caller only; it is not written to disk and
//! never reaches model context.

use crate::{LoginToken, McpError};
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

const CALLBACK_PATH: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Endpoints and client identity for one login. No secret: this is a public client.
#[derive(Debug, Clone, Copy)]
pub struct LoginRequest<'a> {
    pub authorize_url: &'a str,
    pub token_url: &'a str,
    pub client_id: &'a str,
    pub scope: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceCodes {
    pub verifier: String,
    pub challenge: String,
}

/// Creates a fresh S256 verifier and its challenge from the OS random source.
pub fn pkce_codes() -> io::Result<PkceCodes> {
    let verifier = random_urlsafe(32)?;
    let challenge = challenge_for(&verifier);
    Ok(PkceCodes {
        verifier,
        challenge,
    })
}

/// The S256 method: BASE64URL(SHA256(verifier)).
pub fn challenge_for(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

fn random_urlsafe(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0u8; bytes];
    File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(base64url(&buffer))
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 4);
    for chunk in bytes.chunks(3) {
        let a = u32::from(chunk[0]);
        let b = u32::from(*chunk.get(1).unwrap_or(&0));
        let c = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (a << 16) | (b << 8) | c;
        // A partial final group yields one more character than its bytes, without padding.
        for shift in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * shift)) & 63) as usize] as char);
        }
    }
    out
}

/// Builds the URL the user opens in a browser.
pub fn authorize_url(
    request: &LoginRequest<'_>,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<String, McpError> {
    let mut url = Url::parse(request.authorize_url)
        .map_err(|_| McpError::Protocol("invalid authorize endpoint".into()))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", request.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        if let Some(scope) = request.scope {
            query.append_pair("scope", scope);
        }
    }
    Ok(url.into())
}

/// Reads the authorization code from a request line such as
/// `GET /callback?code=...&state=... HTTP/1.1`. The state must match exactly.
pub fn parse_callback(request_line: &str, expected_state: &str) -> Result<String, McpError> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| McpError::Protocol("malformed login callback".into()))?;
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| McpError::Protocol("malformed login callback".into()))?;
    let (mut code, mut state, mut error) = (None, None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        // Only the short provider error code is echoed; it is never a secret.
        let shown: String = error.chars().take(64).collect();
        return Err(McpError::Protocol(format!(
            "authorization was refused: {shown}"
        )));
    }
    if state.as_deref() != Some(expected_state) {
        return Err(McpError::Protocol("login state did not match".into()));
    }
    code.filter(|code| !code.is_empty())
        .ok_or_else(|| McpError::Protocol("login callback carried no code".into()))
}

/// Runs the whole login. `announce` receives the URL to open; it is called before
/// the listener waits, so a caller can print it or launch a browser.
pub fn login(
    request: &LoginRequest<'_>,
    announce: impl FnOnce(&str),
) -> Result<LoginToken, McpError> {
    require_https(request.authorize_url)?;
    require_https(request.token_url)?;
    run_login(request, announce)
}

fn require_https(endpoint: &str) -> Result<(), McpError> {
    match Url::parse(endpoint) {
        Ok(url) if url.scheme() == "https" && url.host_str().is_some() => Ok(()),
        _ => Err(McpError::Protocol("OAuth endpoints must use https".into())),
    }
}

fn run_login(
    request: &LoginRequest<'_>,
    announce: impl FnOnce(&str),
) -> Result<LoginToken, McpError> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let codes = pkce_codes()?;
    let state = random_urlsafe(16)?;
    announce(&authorize_url(
        request,
        &redirect_uri,
        &state,
        &codes.challenge,
    )?);
    let code = wait_for_code(&listener, &state)?;
    exchange(request, &code, &redirect_uri, &codes.verifier)
}

/// Accepts browser requests until the callback arrives. Other paths, such as
/// `/favicon.ico`, get a 404 and do not end the wait.
fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String, McpError> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(READ_TIMEOUT))?;
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line)?;
                let target = line.split_whitespace().nth(1).unwrap_or("");
                if target != CALLBACK_PATH && !target.starts_with("/callback?") {
                    respond(&mut stream, "404 Not Found", "not found");
                    continue;
                }
                let result = parse_callback(&line, state);
                match &result {
                    Ok(_) => respond(
                        &mut stream,
                        "200 OK",
                        "Login complete. You can close this tab and return to Helios.",
                    ),
                    Err(_) => respond(
                        &mut stream,
                        "400 Bad Request",
                        "Login failed. Return to Helios.",
                    ),
                }
                return result;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(McpError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    // The browser page is informational only; a failed write must not change the outcome.
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn exchange(
    request: &LoginRequest<'_>,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<LoginToken, McpError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(EXCHANGE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| McpError::Protocol(error.to_string()))?;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", request.client_id),
        ("code_verifier", verifier),
    ];
    let response = client
        .post(request.token_url)
        .form(&form)
        .send()
        .map_err(|_| McpError::Protocol("oauth token request failed".into()))?;
    if !response.status().is_success() {
        return Err(McpError::Protocol(format!(
            "oauth token endpoint answered HTTP {}",
            response.status()
        )));
    }
    let body: serde_json::Value = response
        .json()
        .map_err(|_| McpError::Protocol("oauth token response was not JSON".into()))?;
    let value = body
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| McpError::Protocol("oauth response has no access_token".into()))?;
    // Same bounds as the client-credentials path: default 5 minutes, at most a day.
    let lifetime = body
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(300)
        .min(86_400);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    Ok(LoginToken {
        value,
        expires_at_unix: now.saturating_add(lifetime),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread};

    fn request<'a>() -> LoginRequest<'a> {
        LoginRequest {
            authorize_url: "https://auth.example.test/authorize",
            token_url: "https://auth.example.test/token",
            client_id: "helios-public",
            scope: Some("tools.read"),
        }
    }

    #[test]
    fn s256_matches_the_rfc7636_appendix_b_vector() {
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn base64url_matches_rfc4648_vectors_without_padding() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url(input.as_bytes()), expected);
        }
    }

    #[test]
    fn generated_verifiers_are_fresh_and_within_the_spec_length() {
        let first = pkce_codes().expect("codes");
        let second = pkce_codes().expect("codes");
        assert_ne!(first.verifier, second.verifier);
        assert!((43..=128).contains(&first.verifier.len()));
        assert_eq!(first.challenge, challenge_for(&first.verifier));
    }

    #[test]
    fn authorize_url_carries_pkce_state_and_scope() {
        let url =
            authorize_url(&request(), "http://127.0.0.1:9/callback", "st", "ch").expect("url");
        let parsed = Url::parse(&url).expect("parse");
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["code_challenge"], "ch");
        assert_eq!(pairs["state"], "st");
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:9/callback");
        assert_eq!(pairs["scope"], "tools.read");
        assert!(!pairs.contains_key("client_secret"));
    }

    #[test]
    fn callback_requires_matching_state_and_a_code() {
        assert_eq!(
            parse_callback("GET /callback?code=abc&state=s1 HTTP/1.1", "s1").expect("code"),
            "abc"
        );
        assert!(parse_callback("GET /callback?code=abc&state=other HTTP/1.1", "s1").is_err());
        assert!(parse_callback("GET /callback?state=s1 HTTP/1.1", "s1").is_err());
        let refused = parse_callback("GET /callback?error=access_denied&state=s1 HTTP/1.1", "s1")
            .expect_err("refused");
        assert!(refused.to_string().contains("access_denied"));
    }

    #[test]
    fn non_https_endpoints_are_rejected_before_any_network_use() {
        let mut insecure = request();
        insecure.token_url = "http://auth.example.test/token";
        let error = login(&insecure, |_| panic!("must not announce")).expect_err("rejected");
        assert!(error.to_string().contains("https"));
    }

    fn read_body(stream: &mut TcpStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("header");
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().expect("length");
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).expect("body");
        String::from_utf8(body).expect("utf8")
    }

    /// Runs `run_login` against a fake provider on loopback: the browser step hits
    /// the callback, and the token endpoint records the form it received.
    #[test]
    fn login_exchanges_the_code_with_the_verifier_for_the_announced_challenge() {
        let token_listener = TcpListener::bind("127.0.0.1:0").expect("token listener");
        let token_port = token_listener.local_addr().expect("addr").port();
        let (form_tx, form_rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = token_listener.accept().expect("accept token");
            let form = read_body(&mut stream);
            let body = r#"{"access_token":"tok-123","expires_in":3600}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("respond");
            form_tx.send(form).expect("send form");
        });
        let token_url = format!("http://127.0.0.1:{token_port}/token");
        let mut login_request = request();
        login_request.token_url = &token_url;
        // run_login skips the https check so the loopback fake can stand in for the provider.
        let (challenge_tx, challenge_rx) = mpsc::channel();
        let token = run_login(&login_request, move |url| {
            let parsed = Url::parse(url).expect("authorize url");
            let pairs: std::collections::HashMap<_, _> =
                parsed.query_pairs().into_owned().collect();
            challenge_tx
                .send(pairs["code_challenge"].clone())
                .expect("send challenge");
            let redirect = Url::parse(&pairs["redirect_uri"]).expect("redirect");
            let callback = format!(
                "GET {}?code=abc&state={} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                redirect.path(),
                pairs["state"]
            );
            let address = format!("127.0.0.1:{}", redirect.port().expect("port"));
            thread::spawn(move || {
                // Browsers ask for /favicon.ico first; that must not end the login.
                let mut icon = TcpStream::connect(&address).expect("connect icon");
                icon.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                    .expect("send icon");
                let mut not_found = String::new();
                let _ = icon.read_to_string(&mut not_found);
                assert!(not_found.starts_with("HTTP/1.1 404"));
                let mut browser = TcpStream::connect(address).expect("connect callback");
                browser
                    .write_all(callback.as_bytes())
                    .expect("send callback");
                let mut sink = String::new();
                let _ = browser.read_to_string(&mut sink);
            });
        })
        .expect("login");
        assert_eq!(token.value, "tok-123");
        assert!(
            token.valid_for(3_500),
            "expires_in 3600 should leave about an hour"
        );
        assert!(!token.valid_for(3_700));
        let form = form_rx.recv().expect("form");
        let fields: std::collections::HashMap<_, _> = Url::parse(&format!("http://x/?{form}"))
            .expect("form url")
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(fields["grant_type"], "authorization_code");
        assert_eq!(fields["code"], "abc");
        assert_eq!(fields["client_id"], "helios-public");
        assert!(!fields.contains_key("client_secret"));
        let challenge = challenge_rx.recv().expect("challenge");
        assert_eq!(challenge_for(&fields["code_verifier"]), challenge);
    }
}
