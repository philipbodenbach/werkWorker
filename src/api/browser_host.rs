//! Trusted host controls; no operation here is exposed as a model tool.
use super::*;
use crate::{
    browser_policy::{BrowserPolicy, TOOLS},
    browser_view::BrowserHost,
    policy::ToolPermission,
};
use async_trait::async_trait;
use std::sync::Weak;

struct Host {
    engine: Weak<EngineInner>,
    runtime: Weak<SessionRuntime>,
}
impl Host {
    fn session(&self) -> Result<SessionHandle> {
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| WorkerError::new("closed", "Worker closed"))?;
        if engine.closed.load(Ordering::Acquire) {
            return Err(WorkerError::new("closed", "Worker closed"));
        }
        Ok(SessionHandle {
            engine: WorkerEngine { inner: engine },
            runtime: self
                .runtime
                .upgrade()
                .ok_or_else(|| WorkerError::new("closed", "Session closed"))?,
        })
    }
}
#[async_trait]
impl BrowserHost for Host {
    async fn call(&self, request: Value) -> Result<Value> {
        let session = self.session()?;
        let data = session.snapshot().await?;
        if data.parent.is_some() {
            return Err(WorkerError::new(
                "root_required",
                "Browser controls belong to the root session",
            ));
        }
        let browsers = &session.engine.inner.tools.browsers;
        match request["op"].as_str().unwrap_or("") {
            "status" => {
                let approvals: Vec<_> = data
                    .runs
                    .iter()
                    .filter(|r| {
                        matches!(
                            r.status,
                            RunStatus::WaitingApproval | RunStatus::Interrupted
                        )
                    })
                    .flat_map(|r| r.invocations.iter())
                    .filter_map(|i| i.approval.as_ref())
                    .filter(|a| {
                        a.decision.is_none()
                            && a.expires_at_ms > now_ms()
                            && TOOLS.contains(&a.description["tool"].as_str().unwrap_or(""))
                    })
                    .collect();
                Ok(
                    json!({"enabled":data.policy.browser.as_ref().is_some_and(|p|p.enabled),"paused":browsers.is_paused(&data.id).await,"policy":data.policy.browser,"browser":browsers.current(&data.id).await,"approvals":approvals}),
                )
            }
            "mode" => {
                session
                    .set_browser_mode(
                        request["enabled"].as_bool().ok_or_else(|| {
                            WorkerError::new("browser_mode", "Expected enabled boolean")
                        })?,
                        &id("browser-mode"),
                    )
                    .await?;
                Ok(json!({"ok":true}))
            }
            "rule" => {
                let permission: ToolPermission =
                    serde_json::from_value(request["permission"].clone())?;
                session
                    .set_browser_rule(
                        request["site"].as_str().unwrap_or(""),
                        request["category"].as_str().unwrap_or(""),
                        permission,
                        &id("browser-rule"),
                    )
                    .await?;
                Ok(json!({"ok":true}))
            }
            "approval" => {
                let decision = match request["decision"].as_str() {
                    Some("allow") => ApprovalDecision::AllowOnce,
                    Some("deny") => ApprovalDecision::Deny,
                    Some("site") => ApprovalDecision::AllowForSession,
                    _ => return Err(WorkerError::new("browser_approval", "Unknown decision")),
                };
                let run = session
                    .run(request["run_id"].as_str().unwrap_or(""))
                    .await?;
                // This endpoint can resolve browser approvals only.
                let request_id = request["request_id"].as_str().unwrap_or("");
                if !data.run(&run.id)?.invocations.iter().any(|i| {
                    TOOLS.contains(&i.call.name.as_str())
                        && i.approval
                            .as_ref()
                            .is_some_and(|a| a.request_id == request_id)
                }) {
                    return Err(WorkerError::new(
                        "browser_approval",
                        "Unknown browser approval",
                    ));
                }
                run.respond_to_approval(request_id, decision, &id("browser-approval"))
                    .await?;
                Ok(json!({"ok":true}))
            }
            "pause" | "resume" => {
                browsers.pause(&data.id, request["op"] == "pause").await;
                Ok(json!({"ok":true}))
            }
            "manual" => {
                if !data.policy.browser.as_ref().is_some_and(|p| p.enabled) {
                    return Err(WorkerError::new(
                        "browser_disabled",
                        "Enable browser mode first",
                    ));
                }
                let args = request["arguments"].clone();
                let current = browsers.current(&data.id).await;
                let url = args["url"]
                    .as_str()
                    .or_else(|| current["url"].as_str())
                    .unwrap_or("");
                let origin = reqwest::Url::parse(url)
                    .ok()
                    .map(|u| u.origin().ascii_serialization())
                    .unwrap_or_default();
                if data
                    .policy
                    .browser
                    .as_ref()
                    .is_some_and(|p| p.rule(&origin, "browsing") == ToolPermission::Deny)
                {
                    return Err(WorkerError::new(
                        "browser_denied",
                        "Browsing this site is blocked",
                    ));
                }
                browsers.manual(&data.id, args).await
            }
            "stop" => {
                for run in data.runs.iter().filter(|r| !r.status.terminal()) {
                    session
                        .run(&run.id)
                        .await?
                        .cancel("Stopped from browser panel", &id("browser-stop"))
                        .await?;
                }
                browsers.stop_browser(&data.id).await;
                Ok(json!({"ok":true}))
            }
            _ => Err(WorkerError::new(
                "browser_command",
                "Unknown browser host command",
            )),
        }
    }
}
impl SessionHandle {
    /// A token-scoped loopback panel for trusted host interaction only.
    pub async fn browser_panel(&self) -> Result<String> {
        let data = self.snapshot().await?;
        if data.parent.is_some() {
            return Err(WorkerError::new(
                "root_required",
                "Open the root session browser panel",
            ));
        }
        self.engine
            .inner
            .tools
            .browsers
            .panel(
                &data.id,
                Arc::new(Host {
                    engine: Arc::downgrade(&self.engine.inner),
                    runtime: Arc::downgrade(&self.runtime),
                }),
            )
            .await
    }
    /// Explicit host opt-in adds the model-independent browser tools to this session.
    pub async fn set_browser_mode(
        &self,
        enabled: bool,
        command_id: &str,
    ) -> Result<CommandReceipt> {
        let receipt = self
            .update(|d| {
                if d.parent.is_some() {
                    return Err(WorkerError::new(
                        "root_required",
                        "Browser mode must be configured in the root session",
                    ));
                }
                let receipt = d.command(
                    command_id,
                    &json!({"method":"browser_mode","enabled":enabled}),
                    None,
                )?;
                if matches!(receipt.status, ReceiptStatus::Duplicate) {
                    return Ok(receipt);
                }
                d.policy
                    .browser
                    .get_or_insert_with(BrowserPolicy::default)
                    .enabled = enabled;
                if !enabled {
                    for run in &mut d.runs {
                        for invocation in &mut run.invocations {
                            if TOOLS.contains(&invocation.call.name.as_str())
                                && let Some(approval) = &mut invocation.approval
                                && approval.decision.is_none()
                            {
                                approval.decision = Some(ApprovalDecision::Deny);
                            }
                        }
                    }
                }
                if enabled {
                    for tool in TOOLS {
                        if !d.profile.tools.iter().any(|t| t == tool) {
                            d.profile.tools.push((*tool).into());
                        }
                        d.policy.tool_permissions.remove(*tool);
                    }
                }
                d.emit(
                    None,
                    EventKind::Diagnostic {
                        code: format!("browser_mode:{}", if enabled { "on" } else { "off" }),
                    },
                );
                Ok(receipt)
            })
            .await?;
        if enabled {
            self.browser_panel().await?;
        } else {
            self.engine
                .inner
                .tools
                .browsers
                .pause(&self.snapshot().await?.id, false)
                .await;
        }
        Ok(receipt)
    }
    pub async fn set_browser_rule(
        &self,
        site: &str,
        category: &str,
        permission: ToolPermission,
        command_id: &str,
    ) -> Result<CommandReceipt> {
        self.update(|d|{
            if d.parent.is_some() {return Err(WorkerError::new("root_required","Website permissions belong to the root session"));}
            let receipt=d.command(command_id,&json!({"method":"browser_rule","site":site,"category":category,"permission":permission}),None)?;
            if matches!(receipt.status,ReceiptStatus::Duplicate) {return Ok(receipt);}
            d.policy.browser.get_or_insert_with(||BrowserPolicy{enabled:false,..Default::default()}).set(site,category,permission)?;
            d.emit(None,EventKind::Diagnostic{code:"browser_rule_changed".into()});Ok(receipt)
        }).await
    }
    pub(super) async fn wait_for_browser_operator(
        &self,
        run_id: &str,
        shutdown: &CancellationToken,
    ) -> Result<()> {
        let id = self.snapshot().await?.id;
        if !self.engine.inner.tools.browsers.is_paused(&id).await {
            return Ok(());
        }
        let previous = self
            .update(|d| {
                let r = d.run_mut(run_id)?;
                let previous = r.status;
                r.status = RunStatus::WaitingInput;
                r.approval_wait_started_at_ms.get_or_insert(now_ms());
                d.emit(
                    Some(run_id),
                    EventKind::RunState {
                        status: RunStatus::WaitingInput,
                    },
                );
                Ok(previous)
            })
            .await?;
        while self.engine.inner.tools.browsers.is_paused(&id).await {
            self.cooperative_delay(run_id, shutdown, 50).await?;
        }
        self.update(|d| {
            let r = d.run_mut(run_id)?;
            if r.status == RunStatus::WaitingInput {
                r.status = previous;
                if !matches!(
                    previous,
                    RunStatus::WaitingApproval | RunStatus::WaitingInput
                ) {
                    r.resume_approval_clock(now_ms());
                }
                d.emit(Some(run_id), EventKind::RunState { status: previous });
            }
            Ok(())
        })
        .await
    }
}
