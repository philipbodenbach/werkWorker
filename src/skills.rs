//! Explicit discovery; immutable skill snapshots. No scripts are executed.
use crate::core::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: String,
    pub hash: String,
}
#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}
impl Skill {
    pub fn parse(directory_name: &str, source: &str, text: &str) -> Result<Self> {
        if text.len() > 65_536 {
            return Err(WorkerError::new("skill_limit", "Skill exceeds 64 KiB"));
        }
        let normalized = text.replace("\r\n", "\n");
        let rest = normalized
            .strip_prefix("---\n")
            .ok_or_else(|| WorkerError::new("invalid_skill", "Missing YAML frontmatter"))?;
        let (yaml, body) = rest
            .split_once("\n---\n")
            .ok_or_else(|| WorkerError::new("invalid_skill", "Unterminated YAML frontmatter"))?;
        let meta: Frontmatter = serde_yaml::from_str(yaml)
            .map_err(|_| WorkerError::new("invalid_skill", "Invalid skill metadata"))?;
        if meta.name != directory_name
            || meta.name.is_empty()
            || meta.name.len() > 64
            || meta.name.starts_with('-')
            || meta.name.ends_with('-')
            || meta.name.contains("--")
            || !meta
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            || meta.description.trim().is_empty()
            || meta.description.len() > 1024
        {
            return Err(WorkerError::new(
                "invalid_skill",
                "Invalid name, directory match or description",
            ));
        }
        Ok(Self {
            name: meta.name,
            description: meta.description,
            body: body.into(),
            source: source.into(),
            hash: hash(text),
        })
    }
}
#[derive(Clone, Default)]
pub struct SkillRegistry {
    pub(crate) skills: BTreeMap<String, Skill>,
}
impl SkillRegistry {
    pub fn bundled() -> Result<Self> {
        let mut registry = Self::default();
        for (name, text) in [
            (
                "task-execution",
                include_str!("../skills/task-execution/SKILL.md"),
            ),
            (
                "coding-basic",
                include_str!("../skills/coding-basic/SKILL.md"),
            ),
            (
                "ticket-drafting",
                include_str!("../skills/ticket-drafting/SKILL.md"),
            ),
            (
                "document-analysis",
                include_str!("../skills/document-analysis/SKILL.md"),
            ),
        ] {
            registry.register(Skill::parse(name, "bundled", text)?)?;
        }
        Ok(registry)
    }
    pub fn register(&mut self, skill: Skill) -> Result<()> {
        if self.skills.contains_key(&skill.name) {
            return Err(WorkerError::new("skill_conflict", "Duplicate skill name"));
        }
        self.skills.insert(skill.name.clone(), skill);
        Ok(())
    }
    pub fn list(&self) -> Vec<&Skill> {
        self.skills.values().collect()
    }
    pub fn get(&self, name: &str) -> Result<&Skill> {
        self.skills
            .get(name)
            .ok_or_else(|| WorkerError::new("unknown_skill", "Skill not registered"))
    }
    /// Host-selected directory only. Symlinked metadata/resources are rejected.
    pub async fn load_directory(&mut self, root: &Path) -> Result<()> {
        match tokio::fs::symlink_metadata(root).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| WorkerError::new("invalid_skill", "Invalid directory name"))?;
        let workspace = crate::runtime::Workspace::open(root).await?;
        let data = match workspace.read("SKILL.md", 65_536).await {
            Err(e) if e.code == "not_found" => return Ok(()),
            result => result?,
        };
        let text = String::from_utf8(data)
            .map_err(|_| WorkerError::new("invalid_skill", "Skill must be UTF-8"))?;
        self.register(Skill::parse(name, &root.display().to_string(), &text)?)
    }
}
