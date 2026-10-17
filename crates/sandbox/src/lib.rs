#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionMode {
    ReadOnly,
    WorkspaceWrite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionClass {
    ReadOnly,
    WorkspaceWrite,
    Process,
    Network,
    Dangerous,
}

#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    #[error("permission denied for {class:?}")]
    Denied { class: PermissionClass },
    #[error("path is outside the workspace: {0}")]
    OutsideWorkspace(String),
    #[error("path does not exist: {0}")]
    MissingPath(String),
    #[error("failed to inspect path: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct PermissionEngine {
    root: std::path::PathBuf,
    mode: PermissionMode,
}

impl PermissionEngine {
    pub fn new(
        root: impl Into<std::path::PathBuf>,
        mode: PermissionMode,
    ) -> Result<Self, PermissionError> {
        let root = root.into().canonicalize()?;
        Ok(Self { root, mode })
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn authorize(&self, class: PermissionClass) -> Result<(), PermissionError> {
        let allowed = match class {
            PermissionClass::ReadOnly => true,
            PermissionClass::WorkspaceWrite | PermissionClass::Process => {
                self.mode == PermissionMode::WorkspaceWrite
            }
            PermissionClass::Network | PermissionClass::Dangerous => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(PermissionError::Denied { class })
        }
    }

    /// Resolve an existing path or a new path below the canonical workspace.
    /// Existing ancestors are canonicalized, so symlinked directories cannot escape.
    /// Dangling symlinks are followed to their (missing) target before the
    /// containment check, so a later write through the link cannot land outside.
    pub fn resolve_path(
        &self,
        requested: impl AsRef<std::path::Path>,
    ) -> Result<std::path::PathBuf, PermissionError> {
        const MAX_SYMLINK_HOPS: usize = 40;
        let requested = requested.as_ref();
        let mut candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };
        for _ in 0..MAX_SYMLINK_HOPS {
            let mut missing = Vec::new();
            let mut cursor = candidate.as_path();
            // `symlink_metadata` does not follow links, so a dangling link is
            // seen as present instead of being mistaken for a missing name.
            while std::fs::symlink_metadata(cursor).is_err() {
                let name = cursor
                    .file_name()
                    .ok_or_else(|| PermissionError::MissingPath(candidate.display().to_string()))?;
                missing.push(name.to_os_string());
                cursor = cursor
                    .parent()
                    .ok_or_else(|| PermissionError::MissingPath(candidate.display().to_string()))?;
            }
            let canonical_parent = match cursor.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // `cursor` is a dangling symlink: retry with its target.
                    let target = std::fs::read_link(cursor)?;
                    let mut next = match cursor.parent() {
                        Some(parent) if target.is_relative() => parent.join(target),
                        _ => target,
                    };
                    for part in missing.iter().rev() {
                        next.push(part);
                    }
                    candidate = next;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let mut resolved = canonical_parent;
            for part in missing.iter().rev() {
                resolved.push(part);
            }
            if !resolved.starts_with(&self.root) {
                return Err(PermissionError::OutsideWorkspace(
                    resolved.display().to_string(),
                ));
            }
            return Ok(resolved);
        }
        Err(PermissionError::OutsideWorkspace(format!(
            "{} (too many symlink hops)",
            requested.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("agent-sandbox-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("fixture");
        path
    }

    #[test]
    fn rejects_parent_escape() {
        let root = fixture("parent");
        let engine = PermissionEngine::new(&root, PermissionMode::WorkspaceWrite).expect("engine");
        assert!(matches!(
            engine.resolve_path("../outside"),
            Err(PermissionError::OutsideWorkspace(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let root = fixture("symlink");
        let outside = fixture("symlink-outside");
        fs::write(outside.join("secret.txt"), "secret").expect("secret");
        symlink(&outside, root.join("link")).expect("symlink");
        let engine = PermissionEngine::new(&root, PermissionMode::WorkspaceWrite).expect("engine");
        assert!(matches!(
            engine.resolve_path("link/secret.txt"),
            Err(PermissionError::OutsideWorkspace(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink_escape() {
        use std::os::unix::fs::symlink;
        let root = fixture("dangling");
        let outside = fixture("dangling-outside");
        symlink(outside.join("new.txt"), root.join("file-link")).expect("file symlink");
        symlink(outside.join("missing-dir"), root.join("dir-link")).expect("dir symlink");
        let engine = PermissionEngine::new(&root, PermissionMode::WorkspaceWrite).expect("engine");
        assert!(matches!(
            engine.resolve_path("file-link"),
            Err(PermissionError::OutsideWorkspace(_))
        ));
        assert!(matches!(
            engine.resolve_path("dir-link/new.txt"),
            Err(PermissionError::OutsideWorkspace(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn follows_dangling_symlink_inside_workspace_and_stops_loops() {
        use std::os::unix::fs::symlink;
        let root = fixture("dangling-inside");
        symlink("target.txt", root.join("link")).expect("relative symlink");
        symlink("loop-b", root.join("loop-a")).expect("loop a");
        symlink("loop-a", root.join("loop-b")).expect("loop b");
        let engine = PermissionEngine::new(&root, PermissionMode::WorkspaceWrite).expect("engine");
        assert_eq!(
            engine.resolve_path("link").expect("inside"),
            engine.root().join("target.txt")
        );
        assert!(engine.resolve_path("loop-a").is_err());
    }

    #[test]
    fn denies_write_in_read_only_mode() {
        let root = fixture("readonly");
        let engine = PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("engine");
        assert!(matches!(
            engine.authorize(PermissionClass::WorkspaceWrite),
            Err(PermissionError::Denied { .. })
        ));
    }
}
