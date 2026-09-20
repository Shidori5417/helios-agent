use agent_sandbox::{PermissionEngine, PermissionError};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub auto_invoke: bool,
    pub risk: String,
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("skill I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("skill path rejected: {0}")]
    Permission(#[from] PermissionError),
    #[error("invalid skill frontmatter: {0}")]
    InvalidFrontmatter(String),
}

pub struct SkillCatalog {
    root: PathBuf,
    permissions: PermissionEngine,
    skills: Vec<SkillMetadata>,
}

impl SkillCatalog {
    pub fn discover(
        root: impl Into<PathBuf>,
        permissions: PermissionEngine,
    ) -> Result<Self, SkillError> {
        let root = root.into();
        let mut skills = Vec::new();
        if root.exists() {
            for entry in fs::read_dir(&root)? {
                let directory = entry?.path();
                let path = directory.join("SKILL.md");
                if !path.is_file() {
                    continue;
                }
                let resolved = permissions.resolve_path(&path)?;
                skills.push(parse_metadata(&resolved)?);
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            root,
            permissions,
            skills,
        })
    }

    pub fn metadata(&self) -> &[SkillMetadata] {
        &self.skills
    }

    pub fn load_body(&self, name: &str) -> Result<String, SkillError> {
        let skill = self
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(|| SkillError::InvalidFrontmatter(format!("unknown skill: {name}")))?;
        let path = self.permissions.resolve_path(&skill.path)?;
        let content = fs::read_to_string(path)?;
        Ok(strip_frontmatter(&content))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn parse_metadata(path: &Path) -> Result<SkillMetadata, SkillError> {
    let content = fs::read_to_string(path)?;
    let frontmatter = content
        .strip_prefix("---\n")
        .and_then(|value| value.split_once("\n---"))
        .ok_or_else(|| SkillError::InvalidFrontmatter(path.display().to_string()))?
        .0;
    let mut name = None;
    let mut description = None;
    let mut auto_invoke = false;
    let mut risk = "low".to_owned();
    for line in frontmatter.lines() {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| SkillError::InvalidFrontmatter(line.into()))?;
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "name" => name = Some(value.to_owned()),
            "description" => description = Some(value.to_owned()),
            "auto_invoke" => auto_invoke = value == "true",
            "risk" => risk = value.to_owned(),
            _ => {}
        }
    }
    Ok(SkillMetadata {
        name: name
            .filter(|value| !value.is_empty())
            .ok_or_else(|| SkillError::InvalidFrontmatter("missing name".into()))?,
        description: description.unwrap_or_default(),
        auto_invoke,
        risk,
        path: path.to_path_buf(),
    })
}

fn strip_frontmatter(content: &str) -> String {
    content
        .strip_prefix("---\n")
        .and_then(|value| {
            value
                .split_once("\n---")
                .map(|(_, body)| body.trim_start_matches('\n').to_owned())
        })
        .unwrap_or_else(|| content.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_sandbox::PermissionMode;

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agent-skills-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("z-skill")).expect("dirs");
        fs::create_dir_all(root.join("a-skill")).expect("dirs");
        fs::write(
            root.join("z-skill/SKILL.md"),
            "---\nname: zed\ndescription: Z\nauto_invoke: false\nrisk: low\n---\nZ body\n",
        )
        .expect("skill");
        fs::write(
            root.join("a-skill/SKILL.md"),
            "---\nname: alpha\ndescription: A\nauto_invoke: true\nrisk: medium\n---\nAlpha body\n",
        )
        .expect("skill");
        root
    }

    #[test]
    fn metadata_discovery_is_sorted_and_body_is_lazy() {
        let root = fixture();
        let permissions =
            PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("permissions");
        let catalog = SkillCatalog::discover(&root, permissions).expect("catalog");
        assert_eq!(
            catalog
                .metadata()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zed"]
        );
        assert_eq!(catalog.load_body("alpha").expect("body"), "Alpha body\n");
    }

    #[test]
    fn invalid_frontmatter_is_rejected() {
        let root = fixture();
        fs::create_dir_all(root.join("bad")).expect("dir");
        fs::write(root.join("bad/SKILL.md"), "not frontmatter").expect("skill");
        let permissions =
            PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("permissions");
        assert!(matches!(
            SkillCatalog::discover(&root, permissions),
            Err(SkillError::InvalidFrontmatter(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_skill_outside_root_is_rejected() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let outside = root.with_file_name(format!(
            "{}-outside",
            root.file_name().expect("name").to_string_lossy()
        ));
        fs::create_dir_all(&outside).expect("outside");
        fs::write(
            outside.join("SKILL.md"),
            "---\nname: outside\ndescription: no\n---\nbody",
        )
        .expect("skill");
        symlink(&outside, root.join("escape")).expect("symlink");
        let permissions =
            PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("permissions");
        assert!(matches!(
            SkillCatalog::discover(&root, permissions),
            Err(SkillError::Permission(_))
        ));
    }
}
