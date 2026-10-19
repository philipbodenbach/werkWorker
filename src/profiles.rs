use crate::core::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub schema_version: u32,
    pub name: String,
    pub prompt: String,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
}
impl AgentProfile {
    pub fn bundled(name: &str) -> Result<Self> {
        let text = match name {
            "general-readonly" => include_str!("../profiles/general-readonly.toml"),
            "coding-basic" => include_str!("../profiles/coding-basic.toml"),
            "automation-basic" => include_str!("../profiles/automation-basic.toml"),
            _ => {
                return Err(WorkerError::new(
                    "unknown_profile",
                    "Unknown bundled profile",
                ));
            }
        };
        Self::parse(text)
    }
    pub fn parse(text: &str) -> Result<Self> {
        let p: Self = toml::from_str(text)
            .map_err(|_| WorkerError::new("invalid_profile", "Invalid profile TOML"))?;
        if p.schema_version != 1
            || p.tools.len() > 128
            || p.skills.len() > 64
            || p.prompt.len() > 65_536
        {
            return Err(WorkerError::new(
                "invalid_profile",
                "Unsupported profile version or limits",
            ));
        }
        Ok(p)
    }
    pub fn names() -> [&'static str; 3] {
        ["general-readonly", "coding-basic", "automation-basic"]
    }
}
