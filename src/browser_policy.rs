//! Host-owned browser mode and exact-origin permissions. Never model arguments.
use crate::{
    core::*,
    policy::{PolicyDecision, ToolPermission},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const TOOLS: &[&str] = &[
    "browser.open",
    "browser.observe",
    "browser.navigate",
    "browser.action",
    "browser.close",
    "browser.evaluate",
    "browser.history",
    "browser.download",
];
pub const CATEGORIES: &[&str] = &[
    "browsing",
    "actions",
    "unlabelled",
    "downloads",
    "history",
    "scripts",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserPolicy {
    pub enabled: bool,
    pub defaults: BTreeMap<String, ToolPermission>,
    pub sites: BTreeMap<String, BTreeMap<String, ToolPermission>>,
}
impl Default for BrowserPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            defaults: CATEGORIES
                .iter()
                .map(|c| ((*c).into(), ToolPermission::Ask))
                .collect(),
            sites: BTreeMap::new(),
        }
    }
}
pub fn origin(input: &str) -> Result<String> {
    let url = reqwest::Url::parse(input)
        .map_err(|_| WorkerError::new("browser_site", "Use an exact http(s) origin"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(WorkerError::new(
            "browser_site",
            "Use scheme, host and optional port; no credentials, path or wildcard",
        ));
    }
    Ok(url.origin().ascii_serialization())
}
impl BrowserPolicy {
    pub fn set(&mut self, site: &str, category: &str, permission: ToolPermission) -> Result<()> {
        if !CATEGORIES.contains(&category)
            || category == "scripts" && permission == ToolPermission::Allow
        {
            return Err(WorkerError::new(
                "browser_permission",
                "Choose browsing/actions/unlabelled/downloads/history/scripts and ask/allow/deny; scripts always require approval",
            ));
        }
        if site == "*" {
            self.defaults.insert(category.into(), permission);
        } else {
            let origin = origin(site)?;
            if self.sites.len() >= 200 && !self.sites.contains_key(&origin) {
                return Err(WorkerError::new(
                    "browser_permission",
                    "At most 200 site rules",
                ));
            }
            self.sites
                .entry(origin)
                .or_default()
                .insert(category.into(), permission);
        }
        Ok(())
    }
    pub fn rule(&self, site: &str, category: &str) -> ToolPermission {
        self.sites
            .get(site)
            .and_then(|r| r.get(category))
            .or_else(|| self.defaults.get(category))
            .copied()
            .unwrap_or(ToolPermission::Ask)
    }
    pub fn evaluate(&self, tool: &str, args: &Value) -> PolicyDecision {
        if !self.enabled {
            return PolicyDecision::Deny;
        }
        // Closing always remains possible, including after a site is blocked.
        if tool == "browser.close" {
            return PolicyDecision::Allow;
        }
        let site = args["origin"].as_str().unwrap_or("");
        if self.rule(site, "browsing") == ToolPermission::Deny
            || args["allowed_origins"].as_array().is_some_and(|origins| {
                origins.iter().any(|o| {
                    self.rule(o.as_str().unwrap_or(""), "browsing") == ToolPermission::Deny
                })
            })
        {
            return PolicyDecision::Deny;
        }
        let category = args["browser_category"].as_str().unwrap_or("actions");
        if args["protected_field"] == true {
            return PolicyDecision::Deny;
        }
        let rule = self.rule(site, category);
        if rule == ToolPermission::Deny {
            return PolicyDecision::Deny;
        }
        if category == "scripts" {
            return PolicyDecision::Ask;
        }
        // A saved site rule must not silently grant a new private-network exception.
        if args["new_private_origins"]
            .as_array()
            .is_some_and(|origins| !origins.is_empty())
        {
            return PolicyDecision::Ask;
        }
        // A rule for one site must not silently authorize another origin.
        if args["new_origins"].as_array().is_some_and(|origins| {
            origins
                .iter()
                .any(|o| self.rule(o.as_str().unwrap_or(""), "browsing") != ToolPermission::Allow)
        }) {
            return PolicyDecision::Ask;
        }
        // Once a page was approved, observations/scrolling need no repeated click.
        if category == "browsing" && args["within_session"] == true {
            return PolicyDecision::Allow;
        }
        match rule {
            ToolPermission::Allow => PolicyDecision::Allow,
            ToolPermission::Ask => PolicyDecision::Ask,
            ToolPermission::Deny => PolicyDecision::Deny,
        }
    }
}
