use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    pub workspace: PathBuf,
    pub project_dir: PathBuf,
    pub project_config: PathBuf,
    pub legacy_project_dir: PathBuf,
    pub legacy_config: PathBuf,
    pub global_config: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub db_path: PathBuf,
    pub jobs_dir: PathBuf,
    pub reports_dir: PathBuf,
    pub spill_dir: PathBuf,
    pub plugins_dir: PathBuf,
    pub trust_store: PathBuf,
    pub registry_state: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("workspace path is not a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("failed to resolve workspace: {0}")]
    Workspace(#[from] std::io::Error),
}

impl AppPaths {
    pub fn discover(workspace: impl AsRef<Path>) -> Result<Self, PathError> {
        let workspace = workspace.as_ref();
        let workspace = if workspace.exists() {
            workspace.canonicalize()?
        } else {
            return Err(PathError::NotDirectory(workspace.to_path_buf()));
        };
        if !workspace.is_dir() {
            return Err(PathError::NotDirectory(workspace));
        }
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let config_home = env_path("XDG_CONFIG_HOME", home.join(".config"));
        let data_home = env_path("XDG_DATA_HOME", home.join(".local/share"));
        let state_home = env_path("XDG_STATE_HOME", home.join(".local/state"));
        let cache_home = env_path("XDG_CACHE_HOME", home.join(".cache"));
        let project_dir = workspace.join(".helios");
        let legacy_project_dir = workspace.join(".agent");
        let data_dir = data_home.join("helios");
        let state_dir = state_home.join("helios");
        let cache_dir = cache_home.join("helios");
        Ok(Self {
            workspace,
            project_config: project_dir.join("config.toml"),
            legacy_config: legacy_project_dir.join("config.toml"),
            global_config: config_home.join("helios/config.toml"),
            project_dir,
            legacy_project_dir,
            db_path: data_dir.join("state.db"),
            jobs_dir: state_dir.join("jobs"),
            reports_dir: state_dir.join("reports"),
            spill_dir: cache_dir.join("spill"),
            plugins_dir: data_dir.join("plugins"),
            trust_store: data_dir.join("trust-store.json"),
            registry_state: state_dir.join("registry-state.json"),
            data_dir,
            state_dir,
            cache_dir,
        })
    }

    pub fn effective_project_config(&self) -> &Path {
        if self.project_config.is_file() {
            &self.project_config
        } else {
            &self.legacy_config
        }
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for path in [
            &self.project_dir,
            &self.data_dir,
            &self.state_dir,
            &self.cache_dir,
        ] {
            fs::create_dir_all(path)?;
            set_private_dir(path)?;
        }
        Ok(())
    }
}

fn env_path(name: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(name).map(PathBuf::from).unwrap_or(fallback)
}

fn set_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolves_xdg_paths_and_project_legacy_locations() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = env::temp_dir().join(format!("helios-paths-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let config = root.join("config");
        let data = root.join("data");
        let state = root.join("state");
        let cache = root.join("cache");
        let old = [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ]
        .map(|name| (name, env::var_os(name)));
        for (name, value) in [
            ("XDG_CONFIG_HOME", &config),
            ("XDG_DATA_HOME", &data),
            ("XDG_STATE_HOME", &state),
            ("XDG_CACHE_HOME", &cache),
        ] {
            env::set_var(name, value);
        }
        let paths = AppPaths::discover(&root).unwrap();
        assert_eq!(paths.project_config, root.join(".helios/config.toml"));
        assert_eq!(paths.db_path, data.join("helios/state.db"));
        assert_eq!(paths.jobs_dir, state.join("helios/jobs"));
        assert_eq!(paths.spill_dir, cache.join("helios/spill"));
        for (name, value) in old {
            if let Some(value) = value {
                env::set_var(name, value);
            } else {
                env::remove_var(name);
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn canonicalizes_workspace_and_rejects_non_directory() {
        use std::os::unix::fs::symlink;
        let root = env::temp_dir().join(format!("helios-paths-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let alias = root.join("alias");
        symlink(&root, &alias).unwrap();
        assert_eq!(
            AppPaths::discover(&alias).unwrap().workspace,
            root.canonicalize().unwrap()
        );
        assert!(matches!(
            AppPaths::discover(root.join("missing")),
            Err(PathError::NotDirectory(_))
        ));
    }
}
