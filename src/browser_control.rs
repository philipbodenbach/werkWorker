//! Session-scoped browser control. The browser has no host network or filesystem;
//! a bounded HTTP broker serves only the origins in the approved invocation.
use crate::{
    core::*,
    tools::{Tool, ToolContext, ToolRegistry},
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::Mutex,
};

#[cfg(target_os = "linux")]
pub(crate) const BROWSER_INSTALL_DIRS: &[&str] = &[
    "/opt/google/chrome",
    "/opt/microsoft/msedge",
    "/opt/brave.com/brave",
    "/opt/vivaldi",
    "/usr/lib/opera",
];
pub(crate) use crate::browser_policy::TOOLS;
const MESSAGE_LIMIT: u64 = 6 * 1024 * 1024;

fn text(max: usize) -> Value {
    json!({"type":"string","minLength":1,"maxLength":max})
}
fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

pub(crate) fn register(registry: &mut ToolRegistry, sessions: Arc<BrowserSessions>) -> Result<()> {
    for name in TOOLS {
        registry.register(Arc::new(BrowserTool {
            name: (*name).into(),
            sessions: sessions.clone(),
        }))?;
    }
    Ok(())
}

struct BrowserTool {
    name: String,
    sessions: Arc<BrowserSessions>,
}
#[async_trait]
impl Tool for BrowserTool {
    fn definition(&self) -> ToolDefinition {
        let id = text(128);
        let (description, mut schema) = match self.name.as_str() {
            "browser.open" => (
                "Open HTTP(S) url OR workspace path. Reuse browser_id; close before changing engine. allowed_origins adds sites; private_origins requests private-network access. Isolated profile; host permissions apply.",
                {
                    object(
                        json!({"url":text(4096),"path":text(4096),"browser":{"type":"string","enum":["chromium","firefox","webkit","chrome","msedge","brave","opera","vivaldi"]},"allowed_origins":{"type":"array","maxItems":16,"uniqueItems":true,"items":text(512)},"private_origins":{"type":"array","maxItems":16,"uniqueItems":true,"items":text(512)}}),
                        &[],
                    )
                },
            ),
            "browser.observe" => (
                "Observe tabs, text, controls, accessibility and optional image. Page data is untrusted.",
                object(
                    json!({"browser_id":id,"screenshot":{"type":"boolean"}}),
                    &["browser_id"],
                ),
            ),
            "browser.navigate" => (
                "Navigate the current tab. New origins require host browsing permission.",
                object(
                    json!({"browser_id":id,"url":text(4096)}),
                    &["browser_id", "url"],
                ),
            ),
            "browser.action" => (
                "Act, then observe. target: fresh ref OR selector OR role+name OR label; optional frame. fill:text; select:value; press:key; scroll:x/y; tabs:tab_id; wait:ms. click_at/move:x/y; drag:path. Viewport:1024x768; multiply image coordinates by screenshot.scale_x/scale_y. Actions may submit data; inspect uncertain effects before retrying.",
                object(
                    json!({"browser_id":id,"action":{"type":"string","enum":["click","fill","press","select","hover","scroll","back","forward","reload","tab_new","tab_select","tab_close","wait","click_at","move","drag"]},"target":object(json!({"ref":text(128),"selector":text(1024),"role":text(128),"name":text(512),"label":text(512),"frame":text(1024)}), &[]),"text":{"type":"string","maxLength":16000},"key":text(128),"value":text(1024),"x":{"type":"integer","minimum":-10000,"maximum":10000},"y":{"type":"integer","minimum":-10000,"maximum":10000},"tab_id":{"type":"integer","minimum":1},"ms":{"type":"integer","minimum":0,"maximum":2000},"path":{"type":"array","minItems":2,"maxItems":64,"items":object(json!({"x":{"type":"integer","minimum":0,"maximum":1023},"y":{"type":"integer","minimum":0,"maximum":767}}), &["x","y"])},"button":{"type":"string","enum":["left","right","middle"]},"click_count":{"type":"integer","minimum":1,"maximum":2},"screenshot":{"type":"boolean"}}),
                    &["browser_id", "action"],
                ),
            ),
            "browser.evaluate" => (
                "Run displayed page JavaScript with explicit approval EVERY time. May read/change/submit page data; network stays scoped. Return bounded JSON.",
                object(
                    json!({"browser_id":id,"script":text(12000)}),
                    &["browser_id", "script"],
                ),
            ),
            "browser.history" => (
                "Read this agent's navigation history with host permission; excludes personal history.",
                object(json!({"browser_id":id}), &["browser_id"]),
            ),
            "browser.download" => (
                "Download an approved HTTP(S) URL with browser cookies to an owned artifact (max 4 MiB); never execute it.",
                object(
                    json!({"browser_id":id,"url":text(4096)}),
                    &["browser_id", "url"],
                ),
            ),
            _ => (
                "Close this browser and discard cookies/tabs; remote effects remain.",
                object(json!({"browser_id":id}), &["browser_id"]),
            ),
        };
        if matches!(
            self.name.as_str(),
            "browser.open" | "browser.navigate" | "browser.observe" | "browser.action"
        ) {
            schema["properties"]["screenshot"] = json!({"type":"boolean","description":"Image override: true requests, false suppresses."});
            schema["properties"]["visual"] =
                json!({"type":"boolean","description":"Request image feedback in Auto mode."});
        }
        if self.name == "browser.open" {
            // Native grammar builders may visit oneOf branches without merging
            // their siblings. Each branch must therefore be a complete object,
            // not just `required`/`not` constraints on root-level properties.
            // Keep root properties too for templates that list parameters there.
            let alternatives: Vec<_> = [("url", "path"), ("path", "url")]
                .into_iter()
                .map(|(required, excluded)| {
                    let mut branch = schema.clone();
                    branch["properties"]
                        .as_object_mut()
                        .unwrap()
                        .remove(excluded);
                    branch["required"] = json!([required]);
                    // Descriptions remain on root properties for templates;
                    // avoid tripling prose in constrained model contexts.
                    for property in branch["properties"].as_object_mut().unwrap().values_mut() {
                        property.as_object_mut().unwrap().remove("description");
                    }
                    branch
                })
                .collect();
            schema["oneOf"] = json!(alternatives);
        }
        ToolDefinition {
            name: self.name.clone(),
            version: if self.name == "browser.open" {
                "4"
            } else {
                "3"
            }
            .into(),
            effect: Effect::ExternalWrite,
            description: description.into(),
            input_schema: schema,
            timeout_ms: 60000,
            output_limit: 48000,
            concurrency_safe: false,
        }
    }

    async fn prepare(&self, mut args: Value, ctx: &ToolContext) -> Result<Value> {
        if self.name == "browser.open" {
            let existing = {
                let sessions = self.sessions.sessions.lock().await;
                sessions.get(&ctx.session_id).map(|s| {
                    (
                        s.id.clone(),
                        s.revision,
                        s.browser.clone(),
                        s.origins.clone(),
                        s.private.clone(),
                        s.settings.budgets(),
                    )
                })
            };
            let local = if let Some(path) = args["path"].as_str() {
                if args.get("url").is_some() {
                    return Err(error("validation_error", "Supply path OR url, not both"));
                }
                Some(crate::browser_workspace::entry(path)?)
            } else {
                crate::browser_workspace::file_entry(args["url"].as_str().unwrap_or(""))?
            };
            let url = if let Some(entry) = local {
                args["entry_sha256"] =
                    json!(crate::browser_workspace::validate(&entry, &ctx.workspace).await?);
                args["workspace_path"] = json!(entry.path);
                entry.url
            } else {
                web_url(args["url"].as_str().unwrap_or(""))?
            };
            args["url"] = json!(url.as_str());
            args["origin"] = json!(url.origin().ascii_serialization());
            let mut private = origins(&args, "private_origins")?;
            let mut origins = origins(&args, "allowed_origins")?;
            origins.push(url.origin().ascii_serialization());
            origins.sort();
            origins.dedup();
            if private.iter().any(|p| !origins.contains(p)) {
                return Err(error(
                    "browser_scope",
                    "private_origins must also be approved in allowed_origins or be the initial URL origin",
                ));
            }
            if let Some((
                browser_id,
                revision,
                browser,
                previous_origins,
                previous_private,
                budgets,
            )) = existing
            {
                if args["browser"]
                    .as_str()
                    .is_some_and(|requested| requested != browser)
                {
                    return Err(error(
                        "browser_already_open",
                        &format!(
                            "Browser {browser_id} uses {browser}; close it with browser.close before choosing another engine"
                        ),
                    ));
                }
                args["_session_budgets"] = budgets;
                args["reuse_browser_id"] = json!(browser_id);
                args["revision"] = json!(revision);
                args["browser"] = json!(browser);
                args["new_private_origins"] = json!(
                    private
                        .iter()
                        .filter(|p| !previous_private.contains(p))
                        .collect::<Vec<_>>()
                );
                args["new_origins"] = json!(
                    origins
                        .iter()
                        .filter(|p| !previous_origins.contains(p))
                        .collect::<Vec<_>>()
                );
                origins.extend(previous_origins);
                private.extend(previous_private);
                args["within_session"] = json!(
                    args["new_origins"] == json!([]) && args["new_private_origins"] == json!([])
                );
            } else {
                args["new_private_origins"] = json!(private);
                args["new_origins"] = json!(origins);
            }
            origins.sort();
            origins.dedup();
            private.sort();
            private.dedup();
            if origins.len() > 32 {
                return Err(error(
                    "browser_scope",
                    "At most 32 origins per browser; close and reopen to change scope",
                ));
            }
            args["browser_category"] = json!("browsing");
            args["allowed_origins"] = json!(origins);
            args["private_origins"] = json!(private);
            if args.get("browser").is_none() {
                args["browser"] = json!("chromium");
            }
            args["isolation"] = json!(
                "empty browser filesystem, isolated profile; approved workspace pages served read-only through HTTP broker; no direct network; bounded host-configured lifetime, idle and action limits"
            );
            return Ok(args);
        }
        if self.name == "browser.action" {
            validate_action(&args)?;
        }
        let sessions = self.sessions.sessions.lock().await;
        let session = sessions.get(&ctx.session_id).ok_or_else(missing)?;
        if args["browser_id"] != session.id {
            return Err(missing());
        }
        let mut origins = session.origins.clone();
        let mut current_origin = web_url(&session.url)
            .ok()
            .map(|u| u.origin().ascii_serialization())
            .unwrap_or(session.origin.clone());
        args["new_origins"] = json!([]);
        if matches!(self.name.as_str(), "browser.navigate" | "browser.download") {
            let target = crate::browser_workspace::navigation(
                args["url"].as_str().unwrap_or(""),
                session.workspace.is_some(),
            )?;
            let url = web_url(&target)?;
            current_origin = url.origin().ascii_serialization();
            if !origins.contains(&current_origin) {
                if origins.len() >= 32 {
                    return Err(error(
                        "browser_scope",
                        "At most 32 origins per browser; close and reopen with a new scope",
                    ));
                }
                origins.push(current_origin.clone());
                args["new_origins"] = json!([current_origin]);
            }
            args["url"] = json!(url.as_str());
        }
        let (category, protected) = category(&self.name, &args, &session.snapshot);
        args["browser_category"] = json!(category);
        args["protected_field"] = json!(protected);
        args["within_session"] = json!(category == "browsing" && args["new_origins"] == json!([]));
        args["origin"] = json!(current_origin);
        args["allowed_origins"] = json!(origins);
        args["private_origins"] = json!(session.private);
        args["browser"] = json!(session.browser);
        args["revision"] = json!(session.revision);
        args["expected_url"] = json!(session.url);
        args["_session_budgets"] = session.settings.budgets();
        Ok(args)
    }

    async fn execute(&self, mut args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        if ctx.cancel.is_cancelled() {
            return Err(error(
                "cancelled",
                "Browser action cancelled before dispatch",
            ));
        }
        if let Some(path) = args["workspace_path"].as_str()
            && json!(hash(ctx.workspace.read(path, 4 * 1024 * 1024).await?)) != args["entry_sha256"]
        {
            return Err(error(
                "write_conflict",
                "Workspace browser entry changed since preparation; open the current file again",
            ));
        }
        let settings: crate::browser_policy::BrowserSettings = args
            .get("_browser_settings")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        settings.validate()?;
        let images_supported = args["_images_supported"].as_bool().unwrap_or(true);
        let perceptual = matches!(
            self.name.as_str(),
            "browser.open"
                | "browser.navigate"
                | "browser.observe"
                | "browser.action"
                | "browser.evaluate"
        );
        let explicitly_visual = perceptual
            && (args["screenshot"] == true
                || (args["screenshot"] != false
                    && (settings.perception == crate::browser_policy::Perception::Vision
                        || args["visual"] == true)));
        if explicitly_visual && !images_supported {
            return Err(error(
                "images_unsupported",
                "This task requests visual feedback but image input is disabled in the configured backend. Enable image input with a capable backend or use DOM for text-only work. No browser action executed.",
            ));
        }
        let opening_new = self.name == "browser.open" && !args["reuse_browser_id"].is_string();
        if !opening_new
            && !matches!(self.name.as_str(), "browser.observe" | "browser.close")
            && self.sessions.refresh.lock().await.contains(&ctx.session_id)
        {
            return Err(error(
                "browser_changed",
                "Observe the browser after Continue or context compaction before acting",
            ));
        }
        let screenshot_override = args["screenshot"].as_bool();
        let visual_requested = args["visual"] == true
            || matches!(args["action"].as_str(), Some("click_at" | "drag" | "move"));
        // Own the process across awaits: cancelling/dropping the tool kills it,
        // rather than leaving a detached browser with unreviewed work pending.
        let mut session = {
            let mut sessions = self.sessions.sessions.lock().await;
            if self.name == "browser.open" && args["reuse_browser_id"].is_null() {
                if sessions.contains_key(&ctx.session_id) {
                    return Err(error(
                        "browser_changed",
                        "A browser opened since preparation; retry browser.open to reuse it",
                    ));
                }
                if sessions.len() >= 4 {
                    return Err(error(
                        "browser_limit",
                        "At most four isolated browsers may be open; close an idle browser",
                    ));
                }
                let permit = self
                    .sessions
                    .slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| {
                        error(
                            "browser_limit",
                            "At most four browsers can run at once; close an idle session",
                        )
                    })?;
                let view = self.sessions.view_for(&ctx.session_id).await?;
                BrowserSession::start(&args, ctx, permit, view).await?
            } else {
                let current = sessions.get(&ctx.session_id).ok_or_else(missing)?;
                let requested_id = args.get("reuse_browser_id").unwrap_or(&args["browser_id"]);
                if requested_id.as_str() != Some(current.id.as_str()) {
                    return Err(missing());
                }
                if args["revision"] != current.revision {
                    return Err(error(
                        "browser_changed",
                        "Browser changed since approval; inspect and prepare the action again",
                    ));
                }
                if current.needs_observation
                    && !matches!(self.name.as_str(), "browser.observe" | "browser.close")
                {
                    return Err(error(
                        "browser_changed",
                        "Browser was manually controlled or context was compacted; observe before acting",
                    ));
                }
                sessions.remove(&ctx.session_id).unwrap()
            }
        };
        if session.actions >= session.settings.max_actions && self.name != "browser.close" {
            session
                .view
                .stopped("Browser action budget exhausted")
                .await;
            session.stop().await;
            return Err(error(
                "browser_action_limit",
                "Host browser action budget exhausted; session closed. No action executed.",
            ));
        }
        session.actions += 1;
        *session.last_activity.lock().await = tokio::time::Instant::now();
        if self.name == "browser.open" {
            session.private = serde_json::from_value(args["private_origins"].clone())?;
            if args["workspace_path"].is_string() {
                session.workspace = Some(ctx.workspace.clone());
            }
            args["expected_url"] = json!(session.url);
        }
        if self.name == "browser.close" {
            session.stop().await;
            return Ok(ToolResult::ok(
                json!({"closed":true,"browser_id":session.id}),
            ));
        }
        session.origins = serde_json::from_value(args["allowed_origins"].clone())?;
        if self.name == "browser.history" {
            let result = ToolResult::ok(json!({"history":session.history,"browser_id":session.id}));
            self.sessions
                .sessions
                .lock()
                .await
                .insert(ctx.session_id.clone(), session);
            return Ok(result);
        }
        if self.name == "browser.download" && ctx.artifacts.is_none() {
            self.sessions
                .sessions
                .lock()
                .await
                .insert(ctx.session_id.clone(), session);
            return Err(error(
                "artifacts_unavailable",
                "Downloads require a configured artifact store",
            ));
        }

        let action = args["action"].as_str().unwrap_or(&self.name).to_owned();
        let reuse = self.name == "browser.open" && args["reuse_browser_id"].is_string();
        let work = session.exchange(
            if reuse {
                "browser.navigate"
            } else {
                &self.name
            },
            args,
        );
        if self.name == "browser.observe" {
            self.sessions.refresh.lock().await.remove(&ctx.session_id);
        }
        let mut data = tokio::select! {
            result = work => result?,
            _ = ctx.cancel.cancelled() => return Err(error("outcome_unknown", "Browser action cancelled; session closed. Remote effects may have occurred; do not automatically repeat.")),
            _ = tokio::time::sleep(Duration::from_secs(50)) => return Err(error("outcome_unknown", "Browser action timed out; session closed. Inspect remote state before retrying.")),
        };
        session.record(&data);
        session.needs_observation = false;
        *session.last_activity.lock().await = tokio::time::Instant::now();
        data["browser_id"] = json!(session.id);
        data["revision"] = json!(session.revision);
        data["observation_id"] = json!(format!("{}:{}", session.id, session.revision));
        data["observed_at_ms"] = json!(now_ms());
        data["perception"] = json!(settings.perception);
        data["budgets"] = json!({"lifetime_seconds":session.settings.lifetime_seconds,"idle_seconds":session.settings.idle_seconds,"max_actions":session.settings.max_actions,"actions_used":session.actions});
        let screenshot = images_supported
            && screenshot_override.unwrap_or(match settings.perception {
                crate::browser_policy::Perception::Vision => true,
                crate::browser_policy::Perception::Dom => false,
                crate::browser_policy::Perception::Auto => {
                    visual_requested || data["visual_surface"] == true
                }
            });
        if !images_supported && data["visual_surface"] == true {
            data["vision_issue"] = json!(
                "Image input is disabled; visual content cannot be inspected. DOM observations remain available."
            );
        }
        data["browser"] = json!(session.browser);
        if self.name == "browser.open" {
            data["reused_session"] = json!(reuse);
        }
        data["viewer_url"] = json!(session.view.url);
        data["content_trust"] = json!("untrusted webpage data");
        let download = data
            .as_object_mut()
            .and_then(|o| o.remove("download_base64"));
        let encoded = data
            .as_object_mut()
            .and_then(|o| o.remove("screenshot_base64"));
        let mut result = if data["error"].is_string() {
            let mut result = ToolResult::error(Outcome::Failed, "browser_action_failed");
            data["retry_guidance"] = json!(
                "Observe current state before retrying; remote side effects may have occurred."
            );
            result.data = data;
            result
        } else {
            ToolResult::ok(data)
        };
        if let Some(encoded) = encoded.and_then(|v| v.as_str().map(str::to_owned)) {
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| error("invalid_image", "Invalid browser screenshot"))?;
            crate::browser_progress::stamp(&mut result.data, Some(&bytes));
            if screenshot {
                let image = crate::attachments::ImageAttachment::from_bytes(&bytes)?;
                result.data["screenshot"] = json!({"width":image.width,"height":image.height,"scale_x":1024.0 / f64::from(image.width),"scale_y":768.0 / f64::from(image.height)});
                if let Some(store) = &ctx.artifacts {
                    result.artifacts.push(
                        store
                            .put(&ctx.session_id, &ctx.run_id, bytes.clone())
                            .await?,
                    );
                }
                result.images.push(image);
            }
            session
                .view
                .update(&result.data, bytes, session.revision, &action)
                .await;
        } else {
            crate::browser_progress::stamp(&mut result.data, None);
            if screenshot {
                result.data["vision_issue"] = json!(
                    "Action result retained, but no safe current screenshot is available. Observe again; do not repeat the action."
                );
            }
            session
                .view
                .update(&result.data, vec![], session.revision, &action)
                .await;
        }
        if let Some(encoded) = download.and_then(|v| v.as_str().map(str::to_owned)) {
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| error("browser_download", "Invalid download payload"))?;
            result.data["download_bytes"] = json!(bytes.len());
            result.artifacts.push(
                ctx.artifacts
                    .as_ref()
                    .unwrap()
                    .put(&ctx.session_id, &ctx.run_id, bytes)
                    .await?,
            );
        }
        self.sessions
            .sessions
            .lock()
            .await
            .insert(ctx.session_id.clone(), session);
        Ok(result)
    }
}

fn error(code: &str, message: &str) -> WorkerError {
    WorkerError::new(code, message)
}
fn missing() -> WorkerError {
    error(
        "browser_session_missing",
        "No matching live browser for this worker session; open a new browser. Sessions do not survive engine restart or cancelled actions.",
    )
}

fn category<'a>(tool: &str, args: &Value, snapshot: &Value) -> (&'a str, bool) {
    match tool {
        "browser.evaluate" => return ("scripts", false),
        "browser.history" => return ("history", false),
        "browser.download" => return ("downloads", false),
        "browser.action" => {}
        _ => return ("browsing", false),
    }
    let action = args["action"].as_str().unwrap_or("");
    if matches!(action, "click_at" | "drag") {
        return ("unlabelled", false);
    }
    if matches!(
        action,
        "move"
            | "scroll"
            | "wait"
            | "tab_new"
            | "tab_select"
            | "tab_close"
            | "back"
            | "forward"
            | "reload"
            | "hover"
    ) {
        return ("browsing", false);
    }
    let target = &args["target"];
    let controls = snapshot["controls"].as_array();
    let info = controls.and_then(|items| {
        items.iter().find(|e| {
            target["ref"].is_string() && target["ref"] == e["ref"]
                || target["selector"].as_str().is_some_and(|s| {
                    e["id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty() && s == format!("#{id}"))
                })
                || target["label"].is_string() && target["label"] == e["name"]
                || target["role"].is_string()
                    && target["role"] == e["role"]
                    && target["name"] == e["name"]
        })
    });
    if let Some(info) = info {
        if info["protected"] == true && matches!(action, "fill" | "select") {
            return ("actions", true);
        }
        if action == "fill" {
            return ("browsing", false);
        }
        if action == "click"
            && info["tag"] == "a"
            && info["consequential"] == false
            && info["download"] == false
        {
            return ("browsing", false);
        }
    }
    ("actions", false)
}
fn web_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| error("invalid_url", "Expected an absolute HTTP(S) URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(error(
            "url_denied",
            "Only HTTP(S) URLs without embedded credentials are supported",
        ));
    }
    Ok(url)
}
fn origins(args: &Value, key: &str) -> Result<Vec<String>> {
    args[key].as_array().into_iter().flatten().map(|value| {
        let url = web_url(value.as_str().unwrap_or(""))?;
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(error("browser_scope", "Use exact origins (scheme, host, optional port), without paths, query or fragment"));
        }
        Ok(url.origin().ascii_serialization())
    }).collect()
}
fn validate_action(args: &Value) -> Result<()> {
    let kind = args["action"].as_str().unwrap_or("");
    let required: &[&str] = match kind {
        "click" | "hover" => &["target"],
        "fill" => &["target", "text"],
        "select" => &["target", "value"],
        "press" => &["key"],
        "scroll" | "click_at" | "move" => &["x", "y"],
        "drag" => &["path"],
        "tab_select" | "tab_close" => &["tab_id"],
        "wait" => &["ms"],
        _ => &[],
    };
    if required.iter().any(|key| args.get(key).is_none()) {
        return Err(error(
            "invalid_browser_action",
            "Missing arguments required by this browser action",
        ));
    }
    if let Some(target) = args.get("target")
        && (["ref", "selector", "role", "label"]
            .iter()
            .filter(|key| target.get(key).is_some())
            .count()
            != 1
            || target.get("role").is_some() != target.get("name").is_some())
    {
        return Err(error(
            "invalid_browser_target",
            "Specify exactly ref, selector, label, or role plus name; optionally frame",
        ));
    }
    if matches!(kind, "click_at" | "move")
        && (!args["x"].as_i64().is_some_and(|x| (0..1024).contains(&x))
            || !args["y"].as_i64().is_some_and(|y| (0..768).contains(&y)))
    {
        return Err(error(
            "invalid_browser_coordinates",
            "Coordinates must be inside the 1024x768 viewport",
        ));
    }

    Ok(())
}

pub(crate) struct BrowserSessions {
    sessions: Mutex<BTreeMap<String, BrowserSession>>,
    slots: Arc<tokio::sync::Semaphore>,
    views: Mutex<BTreeMap<String, Arc<crate::browser_view::BrowserView>>>,
    paused: Mutex<std::collections::BTreeSet<String>>,
    refresh: Mutex<std::collections::BTreeSet<String>>,
}
impl Default for BrowserSessions {
    fn default() -> Self {
        Self {
            sessions: Mutex::default(),
            slots: Arc::new(tokio::sync::Semaphore::new(4)),
            views: Mutex::default(),
            paused: Mutex::default(),
            refresh: Mutex::default(),
        }
    }
}
impl BrowserSessions {
    pub(crate) async fn panel(
        &self,
        id: &str,
        host: Arc<dyn crate::browser_view::BrowserHost>,
    ) -> Result<String> {
        let mut views = self.views.lock().await;
        if let Some(view) = views.get(id) {
            return Ok(view.url.clone());
        }
        let view = Arc::new(crate::browser_view::BrowserView::start_with_host(Some(host)).await?);
        let url = view.url.clone();
        views.insert(id.into(), view);
        Ok(url)
    }
    async fn view_for(&self, id: &str) -> Result<Arc<crate::browser_view::BrowserView>> {
        if let Some(view) = self.views.lock().await.get(id) {
            return Ok(view.clone());
        }
        Ok(Arc::new(crate::browser_view::BrowserView::start().await?))
    }
    pub(crate) async fn is_paused(&self, id: &str) -> bool {
        self.paused.lock().await.contains(id)
    }
    pub(crate) async fn pause(&self, id: &str, pause: bool) {
        let mut paused = self.paused.lock().await;
        if pause {
            paused.insert(id.into());
        } else {
            paused.remove(id);
            self.refresh.lock().await.insert(id.into());
            if let Some(session) = self.sessions.lock().await.get_mut(id) {
                session.revision += 1;
                session.needs_observation = true;
            }
        }
    }
    pub(crate) async fn require_observation(&self, id: &str) {
        self.refresh.lock().await.insert(id.into());
    }
    pub(crate) async fn current(&self, id: &str) -> Value {
        let mut sessions = self.sessions.lock().await;
        if sessions
            .get(id)
            .is_some_and(|s| s.stop_reason.load(std::sync::atomic::Ordering::Acquire) != 0)
            && let Some(mut expired) = sessions.remove(id)
        {
            expired.stop().await;
        }
        sessions.get(id).map_or(Value::Null,|s|json!({"browser_id":s.id,"revision":s.revision,"url":s.url,"tabs":s.snapshot["tabs"],"viewer_url":s.view.url,"history":s.history,"workspace":s.workspace.is_some(),"budgets":s.settings,"actions_used":s.actions}))
    }
    pub(crate) async fn manual(self: &Arc<Self>, id: &str, mut args: Value) -> Result<Value> {
        if !args.is_object() {
            return Err(error(
                "invalid_browser_action",
                "Manual arguments must be an object",
            ));
        }
        if !self.is_paused(id).await {
            return Err(error(
                "browser_takeover",
                "Take over before controlling the browser yourself",
            ));
        }
        let tool = if args.get("url").is_some() {
            "browser.navigate"
        } else if args.get("action").is_some() {
            "browser.action"
        } else {
            "browser.observe"
        };
        let mut session = self.sessions.lock().await.remove(id).ok_or_else(|| {
            error(
                "browser_busy",
                "No idle browser; wait for the current action to finish",
            )
        })?;
        args["browser_id"] = json!(session.id);
        let mut validation = args.clone();
        if validation["action"] == "type" {
            validation["action"] = json!("fill");
            validation["target"] = json!({"selector":":focus"});
        }
        let definition = BrowserTool {
            name: tool.into(),
            sessions: self.clone(),
        }
        .definition();
        if let Err(e) = crate::tools::validate_arguments(&definition.input_schema, &validation) {
            self.sessions.lock().await.insert(id.into(), session);
            return Err(e);
        }
        if let Some(url) = args["url"].as_str() {
            let origin =
                match crate::browser_workspace::navigation(url, session.workspace.is_some())
                    .and_then(|u| web_url(&u))
                {
                    Ok(u) => {
                        args["url"] = json!(u.as_str());
                        u.origin().ascii_serialization()
                    }
                    Err(e) => {
                        self.sessions.lock().await.insert(id.into(), session);
                        return Err(e);
                    }
                };
            if !session.origins.contains(&origin) {
                if session.origins.len() >= 32 {
                    self.sessions.lock().await.insert(id.into(), session);
                    return Err(error(
                        "browser_scope",
                        "At most 32 origins per browser; reopen to change scope",
                    ));
                }
                session.origins.push(origin);
            }
        }
        args["human"] = json!(true);
        args["expected_url"] = json!(session.url);
        let action = args["action"].as_str().unwrap_or(tool).to_owned();
        *session.last_activity.lock().await = tokio::time::Instant::now();
        let mut data = tokio::time::timeout(Duration::from_secs(50), session.exchange(tool, args))
            .await
            .map_err(|_| error("outcome_unknown", "Manual action timed out; browser closed"))??;
        session.record(&data);
        *session.last_activity.lock().await = tokio::time::Instant::now();
        if let Some(encoded) = data
            .as_object_mut()
            .and_then(|o| o.remove("screenshot_base64"))
            .and_then(|v| v.as_str().map(str::to_owned))
        {
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| error("browser_image", "Invalid screenshot"))?;
            session
                .view
                .update(&data, bytes, session.revision, &format!("You · {action}"))
                .await;
        } else {
            session
                .view
                .update(&data, vec![], session.revision, &format!("You · {action}"))
                .await;
        }
        self.sessions.lock().await.insert(id.into(), session);
        Ok(json!({"ok":data.get("error").is_none(),"error":data["error"]}))
    }
    pub(crate) async fn stop_browser(&self, id: &str) {
        let session = self.sessions.lock().await.remove(id);
        if let Some(mut session) = session {
            session.stop().await;
        }
        self.pause(id, false).await;
    }
    pub(crate) async fn close(&self, session_id: Option<&str>) {
        let mut sessions = self.sessions.lock().await;
        let closing = if let Some(id) = session_id {
            sessions.remove(id).into_iter().collect()
        } else {
            std::mem::take(&mut *sessions)
                .into_values()
                .collect::<Vec<_>>()
        };
        drop(sessions);
        for mut session in closing {
            session.stop().await;
        }
        if let Some(id) = session_id {
            self.views.lock().await.remove(id);
            self.paused.lock().await.remove(id);
            self.refresh.lock().await.remove(id);
        } else {
            self.views.lock().await.clear();
            self.paused.lock().await.clear();
            self.refresh.lock().await.clear();
        }
    }
}

struct BrowserSession {
    _permit: tokio::sync::OwnedSemaphorePermit,
    view: Arc<crate::browser_view::BrowserView>,
    snapshot: Value,
    history: Vec<Value>,
    id: String,
    origin: String,
    origins: Vec<String>,
    private: Vec<String>,
    browser: String,
    workspace: Option<crate::runtime::Workspace>,
    revision: u64,
    needs_observation: bool,
    settings: crate::browser_policy::BrowserSettings,
    actions: usize,
    last_activity: Arc<Mutex<tokio::time::Instant>>,
    url: String,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    stderr: tokio::task::JoinHandle<()>,
    watchdog: tokio::task::JoinHandle<()>,
    stop_reason: Arc<std::sync::atomic::AtomicU8>,
}
impl Drop for BrowserSession {
    fn drop(&mut self) {
        self.watchdog.abort();
        self.stderr.abort();
        let _ = self.child.start_kill(); // bwrap PID namespace teardown also kills descendants
    }
}
impl BrowserSession {
    fn record(&mut self, data: &Value) {
        self.revision += 1;
        let url = data["url"].as_str().unwrap_or("about:blank");
        if url != self.url && self.history.last().is_none_or(|h| h["url"] != url) {
            self.history.push(json!({"url":url,"title":data["title"]}));
            if self.history.len() > 100 {
                self.history.remove(0);
            }
        }
        self.url = url.into();
        self.snapshot = data.clone();
        if let Some(o) = self.snapshot.as_object_mut() {
            o.remove("screenshot_base64");
            o.remove("download_base64");
        }
    }

    #[cfg(not(target_os = "linux"))]
    async fn start(
        _args: &Value,
        _ctx: &ToolContext,
        _permit: tokio::sync::OwnedSemaphorePermit,
        _view: Arc<crate::browser_view::BrowserView>,
    ) -> Result<Self> {
        Err(error(
            "sandbox_unavailable",
            "Browser control currently requires Linux/WSL with Bubblewrap; no unsandboxed fallback",
        ))
    }
    #[cfg(target_os = "linux")]
    async fn start(
        args: &Value,
        ctx: &ToolContext,
        permit: tokio::sync::OwnedSemaphorePermit,
        view: Arc<crate::browser_view::BrowserView>,
    ) -> Result<Self> {
        let origins = serde_json::from_value(args["allowed_origins"].clone())?;
        let private = serde_json::from_value(args["private_origins"].clone())?;
        let mut command = ctx.sandbox.browser_command().await?;
        command.args([
            "/usr/bin/python3",
            "-u",
            "-c",
            include_str!("../scripts/browser_control.py"),
        ]);
        let mut child = command.spawn().map_err(|_| {
            error(
                "browser_unavailable",
                "Cannot start browser sandbox; run scripts/install-browser.sh and check Bubblewrap",
            )
        })?;
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut stderr = child.stderr.take().unwrap();
        let debug = std::env::var("WERK_WORKER_BROWSER_DEBUG").as_deref() == Ok("1");
        let stderr = tokio::spawn(async move {
            // Discard browser logs: never flood output or expose cookie/header values.
            let mut buf = [0; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if debug {
                    eprint!("{}", String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
        let pid = child.id().unwrap();
        let memory = ctx.sandbox.limits.browser_session_memory_bytes;
        let stop_reason = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let reason = stop_reason.clone();
        let settings: crate::browser_policy::BrowserSettings = args
            .get("_browser_settings")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        settings.validate()?;
        let lifetime = settings.lifetime_seconds;
        let idle = settings.idle_seconds;
        let last_activity = Arc::new(Mutex::new(tokio::time::Instant::now()));
        let activity = last_activity.clone();
        let watchdog_view = view.clone();
        let watchdog = tokio::spawn(async move {
            let code = tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(lifetime)) => 1,
                _ = crate::runtime::browser_memory_limit(pid, memory) => 2,
                _ = async { loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if activity.lock().await.elapsed() >= Duration::from_secs(idle) { break; }
                }} => 3,
            };
            reason.store(code, std::sync::atomic::Ordering::Release);
            watchdog_view
                .stopped(match code {
                    1 => "Browser lifetime expired",
                    3 => "Browser idle timeout expired",
                    _ => "Browser memory limit exceeded",
                })
                .await;
            // Child remains owned and unreaped until this task is aborted in Drop.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        });
        Ok(Self {
            _permit: permit,
            view,
            snapshot: Value::Null,
            history: Vec::new(),
            id: id("browser"),
            origin: args["origin"].as_str().unwrap().into(),
            origins,
            private,
            browser: args["browser"].as_str().unwrap().into(),
            workspace: None,
            revision: 0,
            needs_observation: false,
            settings,
            actions: 0,
            last_activity,
            url: "about:blank".into(),
            child,
            input,
            output,
            stderr,
            watchdog,
            stop_reason,
        })
    }
    async fn stop(&mut self) {
        self.watchdog.abort();
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
    async fn send(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await.map_err(|_| {
            error(
                "outcome_unknown",
                "Browser connection closed; remote effects may have occurred",
            )
        })
    }
    async fn exchange(&mut self, name: &str, args: Value) -> Result<Value> {
        self.send(&json!({"tool":name,"args":args})).await?;
        let mut requests = 0;
        let mut transferred = 0usize;
        let mut blocked = Vec::new();
        loop {
            let mut line = Vec::new();
            let count = (&mut self.output)
                .take(MESSAGE_LIMIT + 1)
                .read_until(b'\n', &mut line)
                .await?;
            if count == 0 || count as u64 > MESSAGE_LIMIT || line.last() != Some(&b'\n') {
                let reason = match self.stop_reason.load(std::sync::atomic::Ordering::Acquire) {
                    1 => "host-configured session lifetime exceeded",
                    3 => "host-configured idle timeout exceeded",
                    2 => "browser process-tree memory limit exceeded",
                    _ => "browser transport closed or exceeded its output limit",
                };
                return Err(error(
                    "outcome_unknown",
                    &format!(
                        "Browser stopped: {reason}. Session closed; do not automatically repeat remote actions. Check scripts/install-browser.sh and sandbox support."
                    ),
                ));
            }
            let message: Value = serde_json::from_slice(&line)?;
            if message["kind"] == "result" {
                let mut data = message["data"].clone();
                data["blocked_requests"] = json!(blocked);
                return Ok(data);
            }
            if message["kind"] != "http" {
                return Err(error(
                    "browser_protocol",
                    "Invalid browser transport message",
                ));
            }
            requests += 1;
            let response = if requests <= 96 && transferred <= 24 * 1024 * 1024 {
                if web_url(message["url"].as_str().unwrap_or("")).is_ok_and(|u| {
                    u.origin().ascii_serialization() == crate::browser_workspace::ORIGIN
                }) {
                    match &self.workspace {
                        Some(workspace)
                            if self
                                .origins
                                .iter()
                                .any(|o| o == crate::browser_workspace::ORIGIN) =>
                        {
                            crate::browser_workspace::serve(workspace, &message).await
                        }
                        _ => Err(error(
                            "browser_scope",
                            "Open a workspace page with browser.open path before accessing local files",
                        )),
                    }
                } else {
                    broker(&message, &self.origins, &self.private).await
                }
            } else {
                Err(error(
                    "browser_network_limit",
                    "Per-action request/body limit exceeded",
                ))
            };
            let reply = match response {
                Ok(value) => {
                    transferred += value["body"].as_str().map_or(0, str::len);
                    value
                }
                Err(e) => {
                    if blocked.len() < 16 {
                        blocked.push(json!({"origin":message["url"].as_str().and_then(|u| web_url(u).ok()).map(|u|u.origin().ascii_serialization()),"reason":e.code}));
                    }
                    json!({"error":e.code})
                }
            };
            self.send(&reply).await?;
        }
    }
}

async fn broker(message: &Value, origins: &[String], private: &[String]) -> Result<Value> {
    let mut url = web_url(message["url"].as_str().unwrap_or(""))?;
    url.set_fragment(None);
    let origin = url.origin().ascii_serialization();
    if !origins.contains(&origin) {
        return Err(error("browser_scope", "Origin not approved"));
    }
    // Reserve live control-panel ports irrespective of hostname/IP spelling:
    // mapped IPv6, unspecified addresses and DNS aliases must not bypass this.
    if crate::browser_view::protected_port(url.port_or_known_default().unwrap()) {
        return Err(error(
            "browser_control_denied",
            "Agent traffic cannot reach browser host control panels",
        ));
    }
    let host = url.host_str().unwrap().trim_matches(['[', ']']);
    let addresses = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, url.port_or_known_default().unwrap())),
    )
    .await
    .map_err(|_| error("network_timeout", "DNS timed out"))??
    .collect::<Vec<_>>();
    if addresses.is_empty()
        || addresses.len() > 16
        || (!private.contains(&origin)
            && addresses.iter().any(|a| !crate::runtime::public_ip(a.ip())))
    {
        return Err(error(
            "network_denied",
            "Private/reserved addresses require explicit private_origins approval",
        ));
    }
    let method = message["method"].as_str().unwrap_or("");
    if !matches!(
        method,
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
    ) {
        return Err(error("method_denied", "Unsupported browser request method"));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|_| error("http_client", "Cannot build browser network broker"))?;
    let mut request = client.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
        url.clone(),
    );
    if let Some(headers) = message["headers"].as_object() {
        if headers.len() > 128 {
            return Err(error("network_limit", "Too many request headers"));
        }
        for (name, value) in headers {
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "host"
                    | "content-length"
                    | "connection"
                    | "transfer-encoding"
                    | "upgrade"
                    | "accept-encoding"
                    | "te"
                    | "trailer"
            ) || lower.starts_with("proxy-")
            {
                continue;
            }
            if let Some(value) = value.as_str() {
                request = request.header(name, value);
            }
        }
    }
    request = request.header("accept-encoding", "identity");
    if let Some(encoded) = message["body"].as_str() {
        if encoded.len() > 1_400_000 {
            return Err(error("network_limit", "Request body exceeds 1 MiB"));
        }
        let body = STANDARD
            .decode(encoded)
            .map_err(|_| error("browser_protocol", "Invalid request body"))?;
        if body.len() > 1_048_576 {
            return Err(error("network_limit", "Request body exceeds 1 MiB"));
        }
        request = request.body(body);
    }
    let response = request
        .send()
        .await
        .map_err(|_| error("network_error", "Browser HTTP request failed"))?;
    if response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
    {
        return Err(error(
            "browser_streaming_unsupported",
            "Server-sent event streams are not supported by the bounded browser broker",
        ));
    }
    let status = response.status().as_u16();
    let headers: Vec<_> = response
        .headers()
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "transfer-encoding" | "content-length" | "connection"
            )
        })
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect();
    let body = crate::runtime::bounded_body(response, 4 * 1024 * 1024).await?;
    Ok(json!({"status":status,"headers":headers,"body":STANDARD.encode(body)}))
}

#[cfg(test)]
mod host_panel_tests {
    use super::*;
    #[test]
    fn open_alternatives_are_complete_native_schemas() {
        let schema = BrowserTool {
            name: "browser.open".into(),
            sessions: Arc::default(),
        }
        .definition()
        .input_schema;
        for invalid in [
            json!({}),
            json!({"url":"https://example.com","path":"index.html"}),
        ] {
            assert!(crate::tools::validate_arguments(&schema, &invalid).is_err());
        }
        for branch in schema["oneOf"].as_array().unwrap() {
            let key = branch["required"][0].as_str().unwrap();
            let value = if key == "url" {
                "https://example.com"
            } else {
                "index.html"
            };
            let valid = json!({key:value,"screenshot":true,"visual":true,"allowed_origins":["https://example.com"]});
            // Native builders consume each branch independently. Both the
            // required target and every optional field must survive that path.
            assert_eq!(branch["type"], "object");
            assert_eq!(branch["properties"][key]["type"], "string");
            assert!(crate::tools::validate_arguments(branch, &json!({})).is_err());
            crate::tools::validate_arguments(branch, &valid).unwrap();
            crate::tools::validate_arguments(&schema, &valid).unwrap();
            let mut invalid = valid.clone();
            invalid["screenshot"] = json!("true");
            assert!(crate::tools::validate_arguments(branch, &invalid).is_err());
            invalid = valid;
            invalid["unexpected"] = json!(true);
            assert!(crate::tools::validate_arguments(branch, &invalid).is_err());
        }
    }

    #[tokio::test]
    async fn continue_during_owned_action_also_blocks_reopening_the_existing_browser() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext {
            workspace: crate::runtime::Workspace::open(dir.path()).await.unwrap(),
            workspace_lease: None,
            session_id: id("session"),
            run_id: id("run"),
            invocation_id: id("invocation"),
            writable: false,
            cancel: tokio_util::sync::CancellationToken::new(),
            sandbox: crate::runtime::Sandbox::default(),
            jobs: Arc::default(),
            artifacts: None,
            artifact_refs: vec![],
            skills: Default::default(),
            native_jobs: vec![],
        };
        let sessions = Arc::new(BrowserSessions::default());
        // An executing action owns the process outside the session map. The
        // independent refresh marker must survive Continue in that interval.
        sessions.pause(&ctx.session_id, true).await;
        sessions.pause(&ctx.session_id, false).await;
        let tool = BrowserTool {
            name: "browser.open".into(),
            sessions,
        };
        let result = tool
            .execute(json!({"reuse_browser_id":"existing-browser"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(result.code, "browser_changed");
    }

    #[tokio::test]
    async fn panel_ports_are_blocked_before_dns_including_address_aliases() {
        let view = crate::browser_view::BrowserView::start().await.unwrap();
        let port = reqwest::Url::parse(&view.url).unwrap().port().unwrap();
        for host in [
            "127.0.0.1",
            "0.0.0.0",
            "[::ffff:127.0.0.1]",
            "unresolvable.invalid",
        ] {
            let origin = reqwest::Url::parse(&format!("http://{host}:{port}"))
                .unwrap()
                .origin()
                .ascii_serialization();
            let result = broker(
                &json!({"url":format!("{origin}/state"),"method":"GET"}),
                std::slice::from_ref(&origin),
                std::slice::from_ref(&origin),
            )
            .await
            .unwrap_err();
            assert_eq!(result.code, "browser_control_denied");
        }
    }
}
