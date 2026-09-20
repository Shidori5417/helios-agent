use agent_sandbox::{PermissionClass, PermissionEngine};
use agent_terminal::{run, CancellationToken, CommandSpec, CommandStatus};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegistryIndex {
    pub schema_version: u32,
    pub entries: Vec<RegistryEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegistryEnvelope {
    pub schema_version: u32,
    pub registry_id: String,
    pub sequence: u64,
    pub generated_at: u64,
    pub expires_at: u64,
    pub entries: Vec<RegistryEntry>,
    pub key_id: String,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRegistry {
    pub payload: RegistryIndex,
    pub registry_id: String,
    pub sequence: u64,
    pub generated_at: u64,
    pub expires_at: u64,
    pub key_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct RegistryState {
    #[serde(default)]
    registries: BTreeMap<String, RegistryStateEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct RegistryStateEntry {
    sequence: u64,
    digest: String,
}

const REGISTRY_CLOCK_SKEW_SECS: u64 = 300;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegistryEntry {
    pub id: String,
    pub version: String,
    pub manifest: String,
    pub sha256: String,
    pub signing_key_id: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TrustStore {
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
    #[serde(default)]
    pub revoked: BTreeSet<String>,
    #[serde(default)]
    pub disabled: BTreeSet<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry schema failed: {0}")]
    Schema(String),
    #[error("registry entry is invalid: {0}")]
    Invalid(String),
    #[error("registry trust verification failed: {0}")]
    Untrusted(String),
    #[error("registry response exceeded {0} bytes")]
    Oversized(usize),
    #[error("registry network access requires HTTPS")]
    InsecureNetwork,
    #[error("registry HTTP request failed: {0}")]
    Network(String),
    #[error("registry rollback detected")]
    Rollback,
    #[error("registry sequence equivocation detected")]
    Equivocation,
    #[error("registry envelope expired or generated in the future")]
    Freshness,
}

pub fn canonical_registry_bytes(entry: &RegistryEntry, manifest: &[u8]) -> Vec<u8> {
    let payload = format!(
        "{}\n{}\n{}\n{}\n{}\n",
        entry.id, entry.version, entry.manifest, entry.sha256, entry.signing_key_id
    );
    [payload.as_bytes(), manifest].concat()
}

pub fn canonical_registry_index_bytes(index: &RegistryIndex) -> Result<Vec<u8>, RegistryError> {
    let mut entries = index.entries.clone();
    entries.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.version.cmp(&right.version))
    });
    serde_json::to_vec(&RegistryIndex {
        schema_version: index.schema_version,
        entries,
    })
    .map_err(|error| RegistryError::Schema(error.to_string()))
}

fn canonical_envelope_payload(envelope: &RegistryEnvelope) -> Result<Vec<u8>, RegistryError> {
    canonical_registry_index_bytes(&RegistryIndex {
        schema_version: envelope.schema_version,
        entries: envelope.entries.clone(),
    })
    .map(|entries| {
        let mut value = serde_json::json!({
            "schema_version": envelope.schema_version,
            "registry_id": envelope.registry_id,
            "sequence": envelope.sequence,
            "generated_at": envelope.generated_at,
            "expires_at": envelope.expires_at,
            "entries": serde_json::from_slice::<serde_json::Value>(&entries).unwrap_or_default(),
        });
        serde_json::to_vec(&value).unwrap_or_else(|_| {
            value["entries"] = serde_json::json!([]);
            serde_json::to_vec(&value).unwrap_or_default()
        })
    })
}

pub fn verify_registry_envelope(
    envelope: &RegistryEnvelope,
    trust: &TrustStore,
    now: u64,
) -> Result<VerifiedRegistry, RegistryError> {
    if envelope.schema_version != 1
        || envelope.registry_id.is_empty()
        || envelope.key_id.is_empty()
        || envelope.expires_at < now
        || envelope.generated_at > now.saturating_add(REGISTRY_CLOCK_SKEW_SECS)
        || envelope.expires_at < envelope.generated_at
    {
        return Err(RegistryError::Freshness);
    }
    if trust.revoked.contains(&envelope.key_id) || trust.disabled.contains(&envelope.key_id) {
        return Err(RegistryError::Untrusted(
            "signing key is revoked or disabled".into(),
        ));
    }
    let key = trust
        .keys
        .get(&envelope.key_id)
        .ok_or_else(|| RegistryError::Untrusted("signing key is not trusted".into()))?;
    let key = decode_hex(key).map_err(RegistryError::Invalid)?;
    let verifying_key = VerifyingKey::from_bytes(
        key.as_slice()
            .try_into()
            .map_err(|_| RegistryError::Invalid("invalid public key length".into()))?,
    )
    .map_err(|_| RegistryError::Untrusted("invalid public key".into()))?;
    let signature =
        Signature::from_slice(&decode_hex(&envelope.signature).map_err(RegistryError::Invalid)?)
            .map_err(|_| RegistryError::Untrusted("invalid signature".into()))?;
    let payload = canonical_envelope_payload(envelope)?;
    verifying_key
        .verify(&payload, &signature)
        .map_err(|_| RegistryError::Untrusted("registry envelope signature failed".into()))?;
    Ok(VerifiedRegistry {
        payload: RegistryIndex {
            schema_version: envelope.schema_version,
            entries: envelope.entries.clone(),
        },
        registry_id: envelope.registry_id.clone(),
        sequence: envelope.sequence,
        generated_at: envelope.generated_at,
        expires_at: envelope.expires_at,
        key_id: envelope.key_id.clone(),
    })
}

pub fn load_signed_registry(
    source: impl AsRef<str>,
    trust: &TrustStore,
    state_path: impl AsRef<Path>,
) -> Result<VerifiedRegistry, RegistryError> {
    let bytes = read_registry_source(source.as_ref())?;
    let envelope: RegistryEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| RegistryError::Schema(error.to_string()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RegistryError::Schema(error.to_string()))?
        .as_secs();
    let verified = verify_registry_envelope(&envelope, trust, now)?;
    accept_registry_state(&verified, state_path)?;
    Ok(verified)
}

/// Verifies a signed envelope without accepting it into rollback state.
pub fn verify_signed_registry(
    source: impl AsRef<str>,
    trust: &TrustStore,
) -> Result<VerifiedRegistry, RegistryError> {
    let bytes = read_registry_source(source.as_ref())?;
    let envelope: RegistryEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| RegistryError::Schema(error.to_string()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RegistryError::Schema(error.to_string()))?
        .as_secs();
    verify_registry_envelope(&envelope, trust, now)
}

fn accept_registry_state(
    registry: &VerifiedRegistry,
    state_path: impl AsRef<Path>,
) -> Result<(), RegistryError> {
    let path = state_path.as_ref();
    let state = if path.exists() {
        serde_json::from_slice::<RegistryState>(&fs::read(path)?)
            .map_err(|error| RegistryError::Schema(error.to_string()))?
    } else {
        RegistryState::default()
    };
    let digest = sha256_hex(&canonical_registry_index_bytes(&registry.payload)?);
    if let Some(previous) = state.registries.get(&registry.registry_id) {
        if registry.sequence < previous.sequence {
            return Err(RegistryError::Rollback);
        }
        if registry.sequence == previous.sequence && digest != previous.digest {
            return Err(RegistryError::Equivocation);
        }
        if registry.sequence == previous.sequence {
            return Ok(());
        }
    }
    let mut next = state;
    next.registries.insert(
        registry.registry_id.clone(),
        RegistryStateEntry {
            sequence: registry.sequence,
            digest,
        },
    );
    let parent = path
        .parent()
        .ok_or_else(|| RegistryError::Invalid("registry state has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".registry-state-{}", unique_suffix()));
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&next).map_err(|e| RegistryError::Schema(e.to_string()))?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn load_trust_store(path: impl AsRef<Path>) -> Result<TrustStore, RegistryError> {
    let store: TrustStore = serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    for (id, key) in &store.keys {
        if id.is_empty() || decode_hex(key).map_or(true, |bytes| bytes.len() != 32) {
            return Err(RegistryError::Invalid(
                "trust store contains invalid public key".into(),
            ));
        }
    }
    Ok(store)
}

pub fn load_registry(source: impl AsRef<str>) -> Result<RegistryIndex, RegistryError> {
    let bytes = read_registry_source(source.as_ref())?;
    let index: RegistryIndex =
        serde_json::from_slice(&bytes).map_err(|error| RegistryError::Schema(error.to_string()))?;
    if index.schema_version != 1 {
        return Err(RegistryError::Invalid("unsupported registry schema".into()));
    }
    Ok(index)
}

fn read_registry_source(source: &str) -> Result<Vec<u8>, RegistryError> {
    if let Some(path) = source.strip_prefix("file://") {
        read_bounded(Path::new(path), MAX_REGISTRY_BYTES)
    } else if source.starts_with("https://") {
        fetch_https(source, MAX_REGISTRY_BYTES)
    } else if source.starts_with("http://") {
        Err(RegistryError::InsecureNetwork)
    } else {
        read_bounded(Path::new(source), MAX_REGISTRY_BYTES)
    }
}

pub fn verify_registry_entry(
    entry: &RegistryEntry,
    manifest: &[u8],
    trust: &TrustStore,
) -> Result<PluginManifest, RegistryError> {
    if !safe_id(&entry.id) || entry.signing_key_id.is_empty() {
        return Err(RegistryError::Invalid(
            "invalid plugin id or signing key id".into(),
        ));
    }
    if sha256_hex(manifest) != entry.sha256.to_ascii_lowercase() {
        return Err(RegistryError::Untrusted("manifest sha256 mismatch".into()));
    }
    if trust.revoked.contains(&entry.signing_key_id)
        || trust.disabled.contains(&entry.signing_key_id)
    {
        return Err(RegistryError::Untrusted(
            "signing key is revoked or disabled".into(),
        ));
    }
    let key = trust
        .keys
        .get(&entry.signing_key_id)
        .ok_or_else(|| RegistryError::Untrusted("signing key is not trusted".into()))?;
    let key = decode_hex(key).map_err(RegistryError::Invalid)?;
    let signature = decode_hex(&entry.signature).map_err(RegistryError::Invalid)?;
    let verifying_key = VerifyingKey::from_bytes(
        key.as_slice()
            .try_into()
            .map_err(|_| RegistryError::Invalid("invalid public key length".into()))?,
    )
    .map_err(|_| RegistryError::Untrusted("invalid public key".into()))?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| RegistryError::Untrusted("invalid signature".into()))?;
    verifying_key
        .verify(&canonical_registry_bytes(entry, manifest), &signature)
        .map_err(|_| RegistryError::Untrusted("signature verification failed".into()))?;
    let manifest = serde_json::from_slice(manifest)
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    validate(&manifest).map_err(|error| RegistryError::Invalid(error.to_string()))?;
    if manifest.name != entry.id || manifest.version != entry.version {
        return Err(RegistryError::Invalid(
            "manifest identity does not match registry entry".into(),
        ));
    }
    Ok(manifest)
}

pub fn install_from_registry(
    registry_source: impl AsRef<str>,
    trust: &TrustStore,
    workspace: impl AsRef<Path>,
    plugin_id: &str,
) -> Result<PathBuf, RegistryError> {
    let workspace = workspace.as_ref().canonicalize()?;
    let trust_state = workspace.join(".agent/registry-state.json");
    let index = load_signed_registry(registry_source.as_ref(), trust, trust_state)?;
    let entry = index
        .payload
        .entries
        .iter()
        .find(|entry| entry.id == plugin_id)
        .ok_or_else(|| RegistryError::Invalid("plugin id not found".into()))?;
    let manifest = fetch_registry_manifest(registry_source.as_ref(), entry)?;
    let _validated = verify_registry_entry(entry, &manifest, trust)?;
    let plugins = workspace.join(".agent/plugins");
    fs::create_dir_all(&plugins)?;
    let destination = plugins.join(plugin_id);
    if !destination.starts_with(&plugins) || !safe_id(plugin_id) {
        return Err(RegistryError::Invalid("unsafe plugin destination".into()));
    }
    if destination
        .symlink_metadata()
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(RegistryError::Invalid(
            "symlinked plugin destination is rejected".into(),
        ));
    }
    let temporary = plugins.join(format!(".{plugin_id}.install-{}", unique_suffix()));
    if temporary.exists() {
        return Err(RegistryError::Invalid("temporary install collision".into()));
    }
    fs::create_dir(&temporary)?;
    if let Err(error) = fs::write(temporary.join("plugin.json"), &manifest) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error.into());
    }
    let backup = if destination.exists() {
        let trash = plugins.join(".trash");
        fs::create_dir_all(&trash)?;
        let backup = trash.join(format!("{plugin_id}-{}", unique_suffix()));
        fs::rename(&destination, &backup)?;
        Some(backup)
    } else {
        None
    };
    if let Err(error) = fs::rename(&temporary, &destination) {
        let _ = fs::remove_dir_all(&temporary);
        if let Some(backup) = backup {
            let _ = fs::rename(backup, &destination);
        }
        return Err(error.into());
    }
    Ok(destination)
}

pub fn fetch_registry_manifest(
    registry_source: &str,
    entry: &RegistryEntry,
) -> Result<Vec<u8>, RegistryError> {
    let registry_path = registry_source
        .strip_prefix("file://")
        .unwrap_or(registry_source);
    let manifest_source = if entry.manifest.starts_with("https://") {
        entry.manifest.clone()
    } else if registry_path.starts_with("https://") {
        return Err(RegistryError::Invalid(
            "HTTPS registry requires HTTPS manifest URL".into(),
        ));
    } else {
        let base = Path::new(registry_path)
            .parent()
            .ok_or_else(|| RegistryError::Invalid("registry has no parent".into()))?;
        let path = base.join(&entry.manifest);
        if !path.starts_with(base) || entry.manifest.split('/').any(|part| part == "..") {
            return Err(RegistryError::Invalid(
                "manifest path escapes registry".into(),
            ));
        }
        let canonical_base = base.canonicalize()?;
        let canonical_manifest = path.canonicalize()?;
        if !canonical_manifest.starts_with(&canonical_base)
            || path
                .symlink_metadata()
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
        {
            return Err(RegistryError::Invalid(
                "manifest symlink escapes registry".into(),
            ));
        }
        path.display().to_string()
    };
    if manifest_source.starts_with("https://") {
        fetch_https(&manifest_source, MAX_MANIFEST_BYTES)
    } else if manifest_source.starts_with("http://") {
        Err(RegistryError::InsecureNetwork)
    } else {
        read_bounded(Path::new(&manifest_source), MAX_MANIFEST_BYTES)
    }
}

pub fn uninstall_exact(
    workspace: impl AsRef<Path>,
    plugin_id: &str,
) -> Result<PathBuf, RegistryError> {
    if !safe_id(plugin_id) {
        return Err(RegistryError::Invalid("unsafe plugin id".into()));
    }
    let plugins = workspace.as_ref().canonicalize()?.join(".agent/plugins");
    let destination = plugins.join(plugin_id);
    if !destination.starts_with(&plugins) {
        return Err(RegistryError::Invalid("unsafe uninstall path".into()));
    }
    if !destination.exists() {
        return Err(RegistryError::Invalid("plugin is not installed".into()));
    }
    let trash = plugins.join(".trash");
    fs::create_dir_all(&trash)?;
    let target = trash.join(format!("{plugin_id}-{}", unique_suffix()));
    fs::rename(&destination, &target)?;
    Ok(target)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, RegistryError> {
    let bytes = fs::read(path)?;
    if bytes.len() > limit {
        return Err(RegistryError::Oversized(limit));
    }
    Ok(bytes)
}

fn fetch_https(url: &str, limit: usize) -> Result<Vec<u8>, RegistryError> {
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| RegistryError::Network(error.to_string()))?
        .get(url)
        .send()
        .map_err(|error| RegistryError::Network(error.to_string()))?;
    let bytes = response
        .bytes()
        .map_err(|error| RegistryError::Network(error.to_string()))?;
    if bytes.len() > limit {
        return Err(RegistryError::Oversized(limit));
    }
    Ok(bytes.to_vec())
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("hex value has odd length".into());
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16).map_err(|_| "invalid hex".into())
        })
        .collect()
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PluginManifest {
    pub schema_version: u32,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct HookSpec {
    pub event: HookEvent,
    pub timeout_ms: u64,
    pub policy: HookPolicy,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PreEdit,
    PostEdit,
    PermissionRequest,
    PreCompact,
    Stop,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub enum HookPolicy {
    FailClosed,
    FailOpen,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plugin {
    pub manifest: PluginManifest,
    pub path: PathBuf,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookStatus {
    NotExecuted,
    SkippedDisabled,
    Succeeded,
    FailedOpen,
    FailedClosed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookOutput {
    pub status: HookStatus,
    pub output: String,
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid plugin manifest: {0}")]
    InvalidManifest(String),
    #[error("unknown plugin: {0}")]
    UnknownPlugin(String),
    #[error("hook process capability is not declared")]
    MissingProcessCapability,
    #[error("hook cwd escapes workspace")]
    OutsideWorkspace,
    #[error("hook permission denied: {0}")]
    Permission(String),
    #[error("hook blocked by fail-closed policy: {0}")]
    HookFailedClosed(String),
}

pub struct PluginRegistry {
    plugins: Vec<Plugin>,
}

pub fn default_plugin_root(paths: &agent_paths::AppPaths) -> &Path {
    &paths.plugins_dir
}

impl PluginRegistry {
    pub fn discover(root: impl AsRef<Path>) -> Result<Self, PluginError> {
        let mut plugins = Vec::new();
        if root.as_ref().exists() {
            for entry in fs::read_dir(root)? {
                let path = entry?.path();
                let manifest_path = path.join("plugin.json");
                if !manifest_path.is_file() {
                    continue;
                }
                let content = fs::read_to_string(&manifest_path)?;
                let manifest: PluginManifest = serde_json::from_str(&content)
                    .map_err(|error| PluginError::InvalidManifest(error.to_string()))?;
                validate(&manifest)?;
                plugins.push(Plugin {
                    manifest,
                    path,
                    enabled: false,
                });
            }
        }
        plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        Ok(Self { plugins })
    }

    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    pub fn restrict_environment(&mut self, allow: &[String], deny: &[String]) {
        for plugin in &mut self.plugins {
            for hook in &mut plugin.manifest.hooks {
                hook.env_allowlist.retain(|name| {
                    !deny.iter().any(|blocked| blocked == name)
                        && (allow.is_empty() || allow.iter().any(|permitted| permitted == name))
                });
            }
        }
    }
    pub fn enable(&mut self, name: &str) -> Result<(), PluginError> {
        let plugin = self
            .plugins
            .iter_mut()
            .find(|plugin| plugin.manifest.name == name)
            .ok_or_else(|| PluginError::UnknownPlugin(name.into()))?;
        plugin.enabled = true;
        Ok(())
    }
    pub fn disable(&mut self, name: &str) -> Result<(), PluginError> {
        let plugin = self
            .plugins
            .iter_mut()
            .find(|plugin| plugin.manifest.name == name)
            .ok_or_else(|| PluginError::UnknownPlugin(name.into()))?;
        plugin.enabled = false;
        Ok(())
    }
    pub fn run_hook(&self, plugin_name: &str, event: HookEvent) -> Result<HookStatus, PluginError> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.name == plugin_name)
            .ok_or_else(|| PluginError::UnknownPlugin(plugin_name.into()))?;
        if !plugin.enabled {
            return Ok(HookStatus::SkippedDisabled);
        }
        let hook = plugin
            .manifest
            .hooks
            .iter()
            .find(|hook| hook.event == event)
            .ok_or_else(|| PluginError::InvalidManifest("hook not declared".into()))?;
        let _timeout = Duration::from_millis(hook.timeout_ms);
        Ok(HookStatus::NotExecuted)
    }

    /// Execute only an explicitly declared argv hook through the shared process boundary.
    pub fn execute_hook(
        &self,
        plugin_name: &str,
        event: HookEvent,
        workspace: &Path,
        permissions: &PermissionEngine,
        cancellation: &CancellationToken,
        output_limit: usize,
    ) -> Result<HookOutput, PluginError> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.name == plugin_name)
            .ok_or_else(|| PluginError::UnknownPlugin(plugin_name.into()))?;
        if !plugin.enabled {
            return Ok(HookOutput {
                status: HookStatus::SkippedDisabled,
                output: String::new(),
                truncated: false,
            });
        }
        let hook = plugin
            .manifest
            .hooks
            .iter()
            .find(|hook| hook.event == event)
            .ok_or_else(|| PluginError::InvalidManifest("hook not declared".into()))?;
        if !plugin
            .manifest
            .capabilities
            .iter()
            .any(|capability| capability == "hook_process")
        {
            return Err(PluginError::MissingProcessCapability);
        }
        let program = hook
            .command
            .clone()
            .ok_or_else(|| PluginError::InvalidManifest("hook command is required".into()))?;
        permissions
            .authorize(PermissionClass::Process)
            .map_err(|error| PluginError::Permission(error.to_string()))?;
        let workspace = workspace.canonicalize()?;
        let cwd = plugin.path.canonicalize()?;
        if !cwd.starts_with(&workspace) || !cwd.starts_with(permissions.root()) {
            return Err(PluginError::OutsideWorkspace);
        }
        let spill_dir = workspace.join(".agent/hooks-spill");
        let result = run(
            CommandSpec {
                program,
                args: hook.args.clone(),
                cwd,
                timeout: Duration::from_millis(hook.timeout_ms),
                output_limit,
                spill_dir,
                requires_pty: false,
                env_allowlist: hook.env_allowlist.clone(),
            },
            cancellation,
        );
        match result {
            Ok(result) if matches!(result.status, CommandStatus::Exited(0)) => Ok(HookOutput {
                status: HookStatus::Succeeded,
                output: format!("{}{}", result.stdout.text, result.stderr.text),
                truncated: result.stdout.truncated || result.stderr.truncated,
            }),
            Ok(result) => {
                let output = format!("{}{}", result.stdout.text, result.stderr.text);
                if hook.policy == HookPolicy::FailOpen {
                    Ok(HookOutput {
                        status: HookStatus::FailedOpen,
                        output,
                        truncated: result.stdout.truncated || result.stderr.truncated,
                    })
                } else {
                    Err(PluginError::HookFailedClosed(output))
                }
            }
            Err(error) if hook.policy == HookPolicy::FailOpen => Ok(HookOutput {
                status: HookStatus::FailedOpen,
                output: error.to_string(),
                truncated: false,
            }),
            Err(error) => Err(PluginError::HookFailedClosed(error.to_string())),
        }
    }

    pub fn execute_event(
        &self,
        event: HookEvent,
        workspace: &Path,
        permissions: &PermissionEngine,
        cancellation: &CancellationToken,
        output_limit: usize,
    ) -> Result<Vec<HookOutput>, PluginError> {
        let mut outputs = Vec::new();
        for plugin in &self.plugins {
            if plugin.enabled && plugin.manifest.hooks.iter().any(|hook| hook.event == event) {
                outputs.push(self.execute_hook(
                    &plugin.manifest.name,
                    event.clone(),
                    workspace,
                    permissions,
                    cancellation,
                    output_limit,
                )?);
            }
        }
        Ok(outputs)
    }
}

fn validate(manifest: &PluginManifest) -> Result<(), PluginError> {
    if manifest.schema_version != 1 {
        return Err(PluginError::InvalidManifest(
            "unsupported schema_version".into(),
        ));
    }
    if manifest.name.is_empty() || manifest.name.contains('/') {
        return Err(PluginError::InvalidManifest("invalid plugin name".into()));
    }
    let allowed = [
        "native_tool",
        "skill",
        "hook",
        "hook_process",
        "mcp",
        "provider",
    ];
    if manifest
        .capabilities
        .iter()
        .any(|capability| !allowed.contains(&capability.as_str()))
    {
        return Err(PluginError::InvalidManifest("unknown capability".into()));
    }
    if manifest
        .hooks
        .iter()
        .any(|hook| hook.timeout_ms == 0 || hook.timeout_ms > 60_000)
    {
        return Err(PluginError::InvalidManifest(
            "hook timeout out of bounds".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_sandbox::{PermissionEngine, PermissionMode};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::BTreeMap;

    fn root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agent-plugins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("z")).expect("dir");
        fs::create_dir_all(root.join("a")).expect("dir");
        fs::write(root.join("z/plugin.json"), r#"{"schema_version":1,"name":"zed","version":"1","capabilities":["hook"],"hooks":[{"event":"Stop","timeout_ms":100,"policy":"FailOpen"}]}"#).expect("manifest");
        fs::write(
            root.join("a/plugin.json"),
            r#"{"schema_version":1,"name":"alpha","version":"1","capabilities":["skill"]}"#,
        )
        .expect("manifest");
        root
    }

    #[test]
    fn discovery_is_sorted_and_lifecycle_is_explicit() {
        let mut registry = PluginRegistry::discover(root()).expect("registry");
        assert_eq!(
            registry
                .plugins()
                .iter()
                .map(|plugin| plugin.manifest.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zed"]
        );
        registry.enable("zed").expect("enable");
        assert_eq!(
            registry.run_hook("zed", HookEvent::Stop).expect("hook"),
            HookStatus::NotExecuted
        );
        registry.disable("zed").expect("disable");
        assert_eq!(
            registry.run_hook("zed", HookEvent::Stop).expect("hook"),
            HookStatus::SkippedDisabled
        );
    }

    #[test]
    fn invalid_capability_and_policy_are_rejected() {
        let root = root();
        fs::create_dir_all(root.join("bad")).expect("dir");
        fs::write(
            root.join("bad/plugin.json"),
            r#"{"schema_version":1,"name":"bad","version":"1","capabilities":["arbitrary_code"]}"#,
        )
        .expect("manifest");
        assert!(matches!(
            PluginRegistry::discover(root),
            Err(PluginError::InvalidManifest(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_hooks_use_permission_process_boundary_and_policy() {
        let root = root();
        fs::write(
            root.join("z/plugin.json"),
            r#"{"schema_version":1,"name":"zed","version":"1","capabilities":["hook","hook_process"],"hooks":[{"event":"Stop","timeout_ms":500,"policy":"FailClosed","command":"printf","args":["hook-ok"],"env_allowlist":[]},{"event":"PreEdit","timeout_ms":500,"policy":"FailOpen","command":"false","args":[]},{"event":"PostEdit","timeout_ms":500,"policy":"FailClosed","command":"false","args":[]}]}"#,
        )
        .expect("manifest");
        let registry = PluginRegistry::discover(&root).expect("registry");
        let mut registry = registry;
        registry.enable("zed").expect("enable");
        let permissions =
            PermissionEngine::new(&root, PermissionMode::WorkspaceWrite).expect("permissions");
        let output = registry
            .execute_hook(
                "zed",
                HookEvent::Stop,
                &root,
                &permissions,
                &CancellationToken::default(),
                32,
            )
            .expect("hook");
        assert_eq!(output.status, HookStatus::Succeeded);
        assert_eq!(output.output, "hook-ok");
        assert_eq!(
            registry
                .execute_hook(
                    "zed",
                    HookEvent::PreEdit,
                    &root,
                    &permissions,
                    &CancellationToken::default(),
                    32
                )
                .expect("open")
                .status,
            HookStatus::FailedOpen
        );
        assert!(matches!(
            registry.execute_hook(
                "zed",
                HookEvent::PostEdit,
                &root,
                &permissions,
                &CancellationToken::default(),
                32
            ),
            Err(PluginError::HookFailedClosed(_))
        ));
    }

    #[test]
    fn signed_registry_install_rejects_tamper_and_untrusted_keys() {
        let root = root();
        let registry_path = root.join("registry.json");
        let manifest_path = root.join("signed-plugin.json");
        let manifest =
            br#"{"schema_version":1,"name":"signed","version":"1","capabilities":["skill"]}"#;
        fs::write(&manifest_path, manifest).expect("manifest");
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let key_id = "fixture-key";
        let mut entry = RegistryEntry {
            id: "signed".into(),
            version: "1".into(),
            manifest: "signed-plugin.json".into(),
            sha256: sha256_hex(manifest),
            signing_key_id: key_id.into(),
            signature: String::new(),
        };
        entry.signature = signing_key
            .sign(&canonical_registry_bytes(&entry, manifest))
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        write_signed_envelope(&registry_path, vec![entry.clone()], &signing_key, key_id, 1);
        let mut keys = BTreeMap::new();
        keys.insert(
            key_id.into(),
            signing_key
                .verifying_key()
                .to_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        );
        let trust = TrustStore {
            keys,
            revoked: BTreeSet::new(),
            disabled: BTreeSet::new(),
        };
        let installed =
            install_from_registry(registry_path.to_str().unwrap(), &trust, &root, "signed")
                .expect("install");
        assert!(installed.join("plugin.json").is_file());
        assert!(uninstall_exact(&root, "signed")
            .expect("uninstall")
            .exists());

        fs::write(&manifest_path, b"tampered").expect("tamper");
        assert!(
            install_from_registry(registry_path.to_str().unwrap(), &trust, &root, "signed")
                .is_err()
        );
        let untrusted = TrustStore {
            keys: BTreeMap::new(),
            revoked: BTreeSet::new(),
            disabled: BTreeSet::new(),
        };
        fs::write(&manifest_path, manifest).expect("restore");
        assert!(install_from_registry(
            registry_path.to_str().unwrap(),
            &untrusted,
            &root,
            "signed"
        )
        .is_err());
    }

    #[test]
    fn registry_rejects_traversal_manifest_and_revoked_key() {
        let root = root();
        let manifest = br#"{"schema_version":1,"name":"signed","version":"1","capabilities":[]}"#;
        let signing_key = SigningKey::from_bytes(&[8_u8; 32]);
        let mut entry = RegistryEntry {
            id: "signed".into(),
            version: "1".into(),
            manifest: "../outside.json".into(),
            sha256: sha256_hex(manifest),
            signing_key_id: "revoked".into(),
            signature: String::new(),
        };
        entry.signature = signing_key
            .sign(&canonical_registry_bytes(&entry, manifest))
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let index_path = root.join("registry.json");
        write_signed_envelope(&index_path, vec![entry], &signing_key, "revoked", 1);
        let trust = TrustStore {
            keys: BTreeMap::new(),
            revoked: ["revoked".into()].into_iter().collect(),
            disabled: BTreeSet::new(),
        };
        assert!(
            install_from_registry(index_path.to_str().unwrap(), &trust, &root, "signed").is_err()
        );
    }

    fn write_signed_envelope(
        path: &Path,
        entries: Vec<RegistryEntry>,
        key: &SigningKey,
        key_id: &str,
        sequence: u64,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let mut envelope = RegistryEnvelope {
            schema_version: 1,
            registry_id: "fixture-registry".into(),
            sequence,
            generated_at: now,
            expires_at: now + 3600,
            entries,
            key_id: key_id.into(),
            signature: String::new(),
        };
        envelope.signature = key
            .sign(&canonical_envelope_payload(&envelope).expect("payload"))
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        fs::write(path, serde_json::to_vec(&envelope).expect("envelope")).expect("index");
    }

    #[test]
    fn registry_envelope_checks_signature_freshness_and_sequence_state() {
        let root = root();
        let key = SigningKey::from_bytes(&[9_u8; 32]);
        let key_id = "envelope-key";
        let trust = TrustStore {
            keys: [(key_id.into(), hex_key(&key))].into_iter().collect(),
            revoked: BTreeSet::new(),
            disabled: BTreeSet::new(),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let envelope = signed_envelope(Vec::new(), &key, key_id, 2, now, now + 3600);
        verify_registry_envelope(&envelope, &trust, now).expect("valid envelope");

        let mut tampered = envelope.clone();
        tampered.entries.push(RegistryEntry {
            id: "tampered".into(),
            version: "1".into(),
            manifest: "x.json".into(),
            sha256: "00".repeat(32),
            signing_key_id: key_id.into(),
            signature: String::new(),
        });
        assert!(matches!(
            verify_registry_envelope(&tampered, &trust, now),
            Err(RegistryError::Untrusted(_))
        ));
        assert!(matches!(
            verify_registry_envelope(
                &signed_envelope(Vec::new(), &key, key_id, 2, now - 1, now - 1),
                &trust,
                now
            ),
            Err(RegistryError::Freshness)
        ));
        assert!(matches!(
            verify_registry_envelope(
                &signed_envelope(Vec::new(), &key, key_id, 2, now + 301, now + 3600),
                &trust,
                now
            ),
            Err(RegistryError::Freshness)
        ));

        let registry_path = root.join("registry.json");
        let state_path = root.join(".agent/registry-state.json");
        fs::write(&registry_path, serde_json::to_vec(&envelope).expect("json")).expect("write");
        load_signed_registry(registry_path.to_str().unwrap(), &trust, &state_path)
            .expect("accept sequence");
        load_signed_registry(registry_path.to_str().unwrap(), &trust, &state_path)
            .expect("same sequence identical");
        let lower = signed_envelope(Vec::new(), &key, key_id, 1, now, now + 3600);
        fs::write(&registry_path, serde_json::to_vec(&lower).expect("json")).expect("write");
        assert!(matches!(
            load_signed_registry(registry_path.to_str().unwrap(), &trust, &state_path),
            Err(RegistryError::Rollback)
        ));
        let changed = signed_envelope(
            vec![RegistryEntry {
                id: "changed".into(),
                version: "1".into(),
                manifest: "x.json".into(),
                sha256: "00".repeat(32),
                signing_key_id: key_id.into(),
                signature: String::new(),
            }],
            &key,
            key_id,
            2,
            now,
            now + 3600,
        );
        fs::write(&registry_path, serde_json::to_vec(&changed).expect("json")).expect("write");
        assert!(matches!(
            load_signed_registry(registry_path.to_str().unwrap(), &trust, &state_path),
            Err(RegistryError::Equivocation)
        ));
    }

    fn hex_key(key: &SigningKey) -> String {
        key.verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn signed_envelope(
        entries: Vec<RegistryEntry>,
        key: &SigningKey,
        key_id: &str,
        sequence: u64,
        generated_at: u64,
        expires_at: u64,
    ) -> RegistryEnvelope {
        let mut envelope = RegistryEnvelope {
            schema_version: 1,
            registry_id: "fixture-registry".into(),
            sequence,
            generated_at,
            expires_at,
            entries,
            key_id: key_id.into(),
            signature: String::new(),
        };
        envelope.signature = key
            .sign(&canonical_envelope_payload(&envelope).expect("payload"))
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        envelope
    }

    #[test]
    fn hook_execution_rejects_workspace_escape() {
        let plugin_root = root();
        let permission_root =
            std::env::temp_dir().join(format!("agent-plugin-permissions-{}", std::process::id()));
        fs::create_dir_all(&permission_root).expect("permissions root");
        fs::write(plugin_root.join("z/plugin.json"), r#"{"schema_version":1,"name":"zed","version":"1","capabilities":["hook_process"],"hooks":[{"event":"Stop","timeout_ms":100,"policy":"FailClosed","command":"true"}]}"#).expect("manifest");
        let mut registry = PluginRegistry::discover(&plugin_root).expect("registry");
        registry.enable("zed").expect("enable");
        let permissions = PermissionEngine::new(&permission_root, PermissionMode::WorkspaceWrite)
            .expect("permissions");
        assert!(matches!(
            registry.execute_hook(
                "zed",
                HookEvent::Stop,
                &plugin_root,
                &permissions,
                &CancellationToken::default(),
                32
            ),
            Err(PluginError::OutsideWorkspace)
        ));
    }
}
