//! Small shared helpers: redaction, error envelopes, validators, paths and atomic writes.

use super::*;

pub(crate) fn output_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

pub(crate) fn error_envelope(error: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "sequence": 0,
        "timestamp": output_timestamp(),
        "session_id": null,
        "turn_id": null,
        "kind": "error",
        "payload": {"status": "failed", "error": redact_output(error)},
    })
    .to_string()
}

pub(crate) fn redact_output(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if [
        "api_key",
        "apikey",
        "authorization",
        "password",
        "secret",
        "token",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return "[REDACTED]".into();
    }
    const MAX_OUTPUT: usize = 8 * 1024;
    if value.len() > MAX_OUTPUT {
        let prefix = value.chars().take(MAX_OUTPUT).collect::<String>();
        format!("{prefix}…[truncated]")
    } else {
        value.into()
    }
}

pub(crate) fn secret_arg(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "token=",
        "secret=",
        "password=",
        "api_key=",
        "--token",
        "--secret",
        "--password",
        "sk-",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

pub(crate) fn argv_digest(argv: &[String]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    argv.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) fn expand_user_path(path: PathBuf) -> Result<PathBuf, String> {
    if path == Path::new("~") || path.starts_with("~/") {
        let home = env::var_os("HOME").ok_or("HOME is not set")?;
        return Ok(PathBuf::from(home).join(path.strip_prefix("~").unwrap_or(Path::new(""))));
    }
    Ok(path)
}

pub(crate) fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub(crate) fn valid_key_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_ .".contains(&byte))
}

pub(crate) fn read_public_key_file(path: &Path) -> Result<String, String> {
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value = raw.trim();
    if value.contains("BEGIN")
        || value.contains("PRIVATE")
        || value.len() != 64
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("public key file must contain exactly 32-byte Ed25519 public key hex; private/PEM material is rejected".into());
    }
    Ok(value.to_ascii_lowercase())
}

pub(crate) fn parse_role(role: &str) -> Result<Role, String> {
    match role {
        "system" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        other => Err(format!("unknown stored role: {other}")),
    }
}

pub(crate) fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

pub(crate) fn valid_config_key(key: &str) -> bool {
    matches!(key, "provider" | "model" | "base_url")
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("invalid target path")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    fs::write(&temp, bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&temp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&temp, path).map_err(|e| {
        let _ = fs::remove_file(&temp);
        e.to_string()
    })
}

pub(crate) fn provider_env(provider: &str) -> &'static str {
    match provider {
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        _ => "GENERIC_API_KEY",
    }
}

pub(crate) fn valid_provider(provider: &str) -> bool {
    matches!(provider, "openai" | "anthropic" | "gemini" | "generic")
}

pub(crate) fn command_available(name: &str) -> bool {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

pub(crate) fn safe_control_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn safe_env_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

pub(crate) fn secret_env_name(value: &str) -> bool {
    let upper = value.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "API_KEY", "PRIVATE_KEY"]
        .iter()
        .any(|marker| upper.contains(marker))
}
