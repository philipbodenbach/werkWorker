//! Model transport and complete-response assembly. Fragments never execute tools.
use crate::{core::*, runtime::bounded_body};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelDelta>> + Send>>;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCapabilities {
    pub tool_calling: Support,
    pub streaming: Support,
    pub parallel_tools: Support,
    pub structured_output: Support,
    pub images: Support,
    pub reasoning: Support,
    pub context_tokens: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Host selection captured when planning this request. None inherits the
    /// connection configuration; "default" explicitly restores server defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tier: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: usize,
    pub output_schema: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelDelta {
    Text {
        text: String,
    },
    Tool {
        index: usize,
        id: String,
        name: String,
        arguments: String,
    },
    Usage {
        usage: Usage,
    },
    Metadata {
        value: Value,
    },
    Finish {
        reason: String,
    },
    Done,
}
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn capabilities(&self) -> ModelCapabilities;
    fn identity(&self) -> String;
    /// Model selector on this connection, when supported. Not a provider identity.
    fn model_id(&self) -> Option<&str> {
        None
    }
    /// Host-selected child model on the same service; never changes credentials.
    async fn for_model(&self, _model: &str) -> Result<Arc<dyn ModelProvider>> {
        Err(WorkerError::new(
            "subagent_model_unsupported",
            "This provider does not support selecting a subagent model",
        ))
    }
    async fn reasoning_options(&self, _output: usize) -> Result<Option<ReasoningOptions>> {
        Ok(None)
    }
    /// Verified generation ceiling, distinct from the desired initial reserve.
    fn max_output_tokens(&self) -> Option<usize> {
        None
    }
    /// Stable, content-free identity of a concrete request, including provider
    /// settings that affect its wire payload. No network or model work here.
    fn request_fingerprint(&self, request: &ModelRequest) -> String {
        fingerprint(&(self.identity(), request))
    }
    /// Optional server admission plan, possibly an explicitly labeled estimate.
    /// Kept separate from exact native counts; generation still revalidates.
    async fn admission_input_tokens(&self, _request: &ModelRequest) -> Result<Option<usize>> {
        Ok(None)
    }
    /// Exact native input count when supported; None keeps conservative fallback.
    async fn count_input_tokens(&self, _request: &ModelRequest) -> Result<Option<usize>> {
        Ok(None)
    }
    /// Optional, connection-local estimate for these exact image inputs. This
    /// never substitutes an estimate into the native/exact count cache.
    fn estimated_image_tokens(&self, _request: &ModelRequest) -> Option<usize> {
        None
    }
    /// Called only after a complete, validated response, never partial streams.
    fn observe_completed_usage(&self, _request: &ModelRequest, _usage: &Usage) {}
    /// Prefer the server admission plan, then exact native counts or native
    /// text/tool tokens plus an image reserve. Keep estimates labeled as such.
    async fn estimate_input_tokens(&self, request: &ModelRequest) -> Result<usize> {
        if let Some(count) = self.admission_input_tokens(request).await? {
            return Ok(count);
        }
        if let Some(count) = self.count_input_tokens(request).await? {
            return Ok(count);
        }
        let image_reserve = self
            .estimated_image_tokens(request)
            .unwrap_or_else(|| crate::context::image_token_reserve(request));
        if image_reserve > 0 {
            let mut text_request = request.clone();
            for message in &mut text_request.messages {
                message.images.clear();
            }
            if let Some(text_tokens) = self.count_input_tokens(&text_request).await? {
                return Ok(text_tokens
                    .saturating_add(image_reserve)
                    .saturating_add(256));
            }
        }
        Ok(crate::context::estimate_request(request))
    }
    fn cached_input_tokens(&self, _request: &ModelRequest) -> Option<usize> {
        None
    }
    /// Provenance for the last estimate of this request, without another count.
    /// Providers with custom estimates can optionally refine this diagnostic.
    fn input_token_source(&self, request: &ModelRequest) -> &'static str {
        if self.cached_input_tokens(request).is_some() {
            "native_exact"
        } else {
            "provider_unspecified"
        }
    }
    /// Verified server capacity, independent of parallel tool-call support.
    fn concurrency(&self) -> InferenceConcurrency {
        InferenceConcurrency::default()
    }
    /// Optional, throttled refresh of model capacity. No generation or loading.
    async fn refresh_concurrency(&self) {}
    /// Bounded cleanup allowance after a run deadline interrupts a dispatched
    /// request. Keep polling the owned future to confirm remote completion;
    /// never use this allowance to dispatch another request or execute tools.
    /// Providers without remote work retain immediate deadline cancellation.
    fn deadline_drain_timeout(&self) -> Duration {
        Duration::ZERO
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream>;
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceConcurrency {
    pub max_requests: Option<usize>,
    pub source: String,
    pub detail: String,
}
impl Default for InferenceConcurrency {
    fn default() -> Self {
        Self {
            max_requests: None,
            source: "unknown".into(),
            detail: "No verified model request capacity; using one inference slot".into(),
        }
    }
}

#[derive(Default)]
pub struct ResponseAssembler {
    text: String,
    tools: BTreeMap<usize, (String, String, String)>,
    pub usage: Option<Usage>,
    pub metadata: Value,
    finish: Option<String>,
    done: bool,
    bytes: usize,
}
impl ResponseAssembler {
    pub fn push(&mut self, delta: &ModelDelta, max: usize) -> Result<()> {
        if self.done {
            return Err(WorkerError::new(
                "invalid_stream",
                "Data after stream completion",
            ));
        }
        match delta {
            ModelDelta::Text { text } => {
                if self.finish.is_some() {
                    return Err(WorkerError::new("invalid_stream", "Text after finish"));
                }
                self.bytes += text.len();
                self.text.push_str(text);
            }
            ModelDelta::Tool {
                index,
                id,
                name,
                arguments,
            } => {
                if *index >= 128 || self.finish.is_some() {
                    return Err(WorkerError::new(
                        "invalid_stream",
                        "Invalid tool index or late delta",
                    ));
                }
                self.bytes += id.len() + name.len() + arguments.len();
                let t = self.tools.entry(*index).or_default();
                t.0.push_str(id);
                t.1.push_str(name);
                t.2.push_str(arguments);
            }
            ModelDelta::Usage { usage } => self.usage = Some(usage.clone()),
            ModelDelta::Metadata { value } => self.metadata = value.clone(),
            ModelDelta::Finish { reason } => {
                if self.finish.replace(reason.clone()).is_some() {
                    return Err(WorkerError::new(
                        "invalid_stream",
                        "Duplicate finish marker",
                    ));
                }
            }
            ModelDelta::Done => self.done = true,
        }
        if self.bytes > max {
            return Err(WorkerError::new(
                "output_limit",
                "Model response exceeds output limit",
            ));
        }
        Ok(())
    }
    pub fn finish(self) -> Result<(Message, Option<Usage>, Value)> {
        if self.finish.as_deref() == Some("length") {
            return Err(WorkerError::new(
                "model_output_limit",
                "Server stopped at its output token limit; increase reserve_output_tokens or split the work into smaller tool calls. Incomplete output is not executable",
            ));
        }
        if !self.done || !matches!(self.finish.as_deref(), Some("stop" | "tool_calls")) {
            return Err(WorkerError::transient("interrupted_stream"));
        }
        let mut ids = BTreeSet::new();
        let mut calls = Vec::new();
        for (expected, (index, (id, name, args))) in self.tools.into_iter().enumerate() {
            if expected != index || id.is_empty() || name.is_empty() || !ids.insert(id.clone()) {
                return Err(WorkerError::new(
                    "invalid_tool_calls",
                    "Invalid or duplicate tool call ID/index",
                ));
            }
            let arguments = serde_json::from_str(&args)
                .map_err(|_| WorkerError::new("invalid_tool_calls", "Incomplete tool JSON"))?;
            calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        if (self.finish.as_deref() == Some("tool_calls")) != !calls.is_empty() {
            return Err(WorkerError::new(
                "invalid_tool_calls",
                "Finish reason disagrees with tool calls",
            ));
        }
        Ok((
            Message {
                role: Role::Assistant,
                content: self.text,
                images: vec![],
                tool_calls: calls,
                tool_call_id: None,
            },
            self.usage,
            self.metadata,
        ))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptedTurn {
    pub deltas: Vec<ModelDelta>,
    #[serde(default)]
    pub delay_ms: u64,
    pub error: Option<WorkerError>,
}
impl ScriptedTurn {
    pub fn text(text: &str) -> Self {
        Self {
            deltas: vec![
                ModelDelta::Text { text: text.into() },
                ModelDelta::Finish {
                    reason: "stop".into(),
                },
                ModelDelta::Done,
            ],
            delay_ms: 0,
            error: None,
        }
    }
    pub fn calls(text: &str, calls: Vec<ToolCall>) -> Self {
        let mut deltas = vec![ModelDelta::Text { text: text.into() }];
        for (index, c) in calls.into_iter().enumerate() {
            deltas.push(ModelDelta::Tool {
                index,
                id: c.id,
                name: c.name,
                arguments: c.arguments.to_string(),
            });
        }
        deltas.extend([
            ModelDelta::Finish {
                reason: "tool_calls".into(),
            },
            ModelDelta::Done,
        ]);
        Self {
            deltas,
            delay_ms: 0,
            error: None,
        }
    }
}
#[derive(Default)]
pub struct ScriptedProvider {
    turns: Mutex<VecDeque<ScriptedTurn>>,
    requests: Mutex<Vec<ModelRequest>>,
}
impl ScriptedProvider {
    pub fn new(turns: Vec<ScriptedTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(vec![]),
        }
    }
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}
#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn concurrency(&self) -> InferenceConcurrency {
        InferenceConcurrency {
            max_requests: Some(64),
            source: "scripted".into(),
            detail: "Offline fixture; no inference hardware measurement".into(),
        }
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calling: Support::Supported,
            streaming: Support::Supported,
            parallel_tools: Support::Supported,
            ..Default::default()
        }
    }
    fn identity(&self) -> String {
        "scripted-v1".into()
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        self.requests.lock().unwrap().push(request);
        let turn =
            self.turns.lock().unwrap().pop_front().ok_or_else(|| {
                WorkerError::new("script_exhausted", "No scripted response remains")
            })?;
        Ok(Box::pin(
            async_stream::stream! {for delta in turn.deltas {if turn.delay_ms>0 {tokio::time::sleep(Duration::from_millis(turn.delay_ms)).await;}yield Ok(delta);}if let Some(e)=turn.error {yield Err(e);}},
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Supported,
    Unsupported,
    Unavailable,
    Experimental,
    ExternallyManaged,
    MetadataOnly,
}
impl CapabilityStatus {
    pub fn executable(&self, experimental: bool) -> bool {
        *self == Self::Supported || (*self == Self::Experimental && experimental)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCapability {
    pub id: String,
    pub status: CapabilityStatus,
    pub detail: String,
    #[serde(default)]
    pub operations: Vec<String>,
}

/// Defaults reported for matching live workers by Werk Protocol. They are
/// informational: inheriting them must not turn a template default into an
/// explicit override (which can change native thinking semantics).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningDefaults {
    pub source: String,
    pub effort: Option<Value>,
    pub thinking: Option<bool>,
    pub budget_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningTier {
    pub id: String,
    pub label: String,
    pub available: bool,
    pub reasoning_tokens: Option<u64>,
    pub minimum_output_tokens: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningProfile {
    pub schema_version: u32,
    pub model: String,
    pub supported: bool,
    pub tiers: Vec<ReasoningTier>,
    pub default_tier: Option<String>,
    pub default_adjusted_to_limits: bool,
    pub limits: ReasoningLimits,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningLimits {
    pub context_tokens: Option<u64>,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub answer_reserve_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningOptions {
    pub model: String,
    pub requested: String,
    pub native_effort: Option<Value>,
    pub profile: Option<ReasoningProfile>,
}
impl ReasoningOptions {
    pub fn label(&self) -> String {
        if let Some(value) = &self.native_effort {
            return format!("Native: {value}");
        }
        let tier = self.profile.as_ref().and_then(|p| {
            let id = if self.requested == "default" {
                p.default_tier.as_deref()
            } else {
                Some(self.requested.as_str())
            };
            p.tiers.iter().find(|t| Some(t.id.as_str()) == id)
        });
        let label = tier.map_or_else(|| self.requested.clone(), |t| t.label.clone());
        let label = if self.requested == "default" {
            if tier.is_some() {
                format!("Server default: {label}")
            } else {
                "Server default (not reported)".into()
            }
        } else {
            label
        };
        format!(
            "{label}{}{}",
            tier.and_then(|t| t.reasoning_tokens)
                .map(|n| format!("; {n} reasoning tokens"))
                .unwrap_or_default(),
            if tier.is_some_and(|t| !t.available) {
                " (unavailable at these limits)"
            } else {
                ""
            }
        )
    }
}

pub fn validate_reasoning_tier(value: &Value) -> Result<()> {
    if value.is_null()
        || value.as_str().is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        })
    {
        Ok(())
    } else {
        Err(WorkerError::new(
            "invalid_reasoning_tier",
            "reasoning_tier must be a protocol tier ID, default, or null",
        ))
    }
}

/// Credentials are resolved separately and never serialized with configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WerkConfig {
    pub base_url: String,
    pub model: String,
    pub deployment: Option<String>,
    pub capabilities: ModelCapabilities,
    pub stream: bool,
    pub timeout_ms: u64,
    /// Cleanup only, separate from the run budget and transport idle timeout.
    #[serde(default = "default_deadline_drain_timeout_ms")]
    pub deadline_drain_timeout_ms: u64,
    pub max_body_bytes: usize,
    pub options: BTreeMap<String, Value>,
}
impl WerkConfig {
    pub fn local(model: impl Into<String>) -> Self {
        Self {
            base_url: "http://127.0.0.1:11434".into(),
            model: model.into(),
            deployment: None,
            capabilities: ModelCapabilities::default(),
            stream: true,
            timeout_ms: 120_000,
            deadline_drain_timeout_ms: default_deadline_drain_timeout_ms(),
            max_body_bytes: 8 * 1024 * 1024,
            options: BTreeMap::new(),
        }
    }
}

fn default_deadline_drain_timeout_ms() -> u64 {
    600_000
}

pub(crate) fn validate_reasoning_effort(value: &Value) -> Result<()> {
    if value.is_null() || value.is_string() || value.is_number() {
        Ok(())
    } else {
        Err(WorkerError::new(
            "invalid_reasoning_effort",
            "reasoning_effort must be a string, number, or null (server default); supported values depend on the server/model",
        ))
    }
}

pub trait SecretResolver: Send + Sync {
    fn resolve(&self, reference: &str) -> Result<Option<String>>;
}
pub struct EnvironmentSecrets;
impl SecretResolver for EnvironmentSecrets {
    fn resolve(&self, reference: &str) -> Result<Option<String>> {
        validate_id(reference)?;
        Ok(std::env::var(reference).ok())
    }
}
#[derive(Clone)]
pub struct WerkClient {
    client: reqwest::Client,
    base: reqwest::Url,
    secret: Option<Arc<String>>,
    deployment: Option<String>,
    max_body: usize,
    timeout: Duration,
}
impl std::fmt::Debug for WerkClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WerkClient")
            .field("authentication", &self.secret.is_some())
            .finish()
    }
}
impl WerkClient {
    pub fn new(config: &WerkConfig, secret: Option<String>) -> Result<Self> {
        let base = reqwest::Url::parse(&config.base_url)
            .map_err(|_| WorkerError::new("invalid_url", "Invalid Werk base URL"))?;
        let local = base
            .host_str()
            .is_some_and(|h| h.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback()));
        if !(base.scheme() == "https" || (base.scheme() == "http" && local))
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path() != "/"
        {
            return Err(WorkerError::new(
                "transport_denied",
                "Werk requires a bare HTTPS origin or literal loopback HTTP origin",
            ));
        }
        if config.timeout_ms == 0
            || config.timeout_ms > 600_000
            || config.deadline_drain_timeout_ms > 600_000
            || config.max_body_bytes == 0
            || config.max_body_bytes > 16 * 1024 * 1024
        {
            return Err(WorkerError::new(
                "invalid_limits",
                "Invalid Werk transport limits",
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|_| WorkerError::new("http_client", "Cannot construct Werk client"))?;
        Ok(Self {
            client,
            base,
            secret: secret.map(Arc::new),
            deployment: config.deployment.clone(),
            max_body: config.max_body_bytes,
            timeout: Duration::from_millis(config.timeout_ms),
        })
    }
    fn request(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        if !path.starts_with('/')
            || path.starts_with("//")
            || path.contains("..")
            || path.contains('#')
        {
            return Err(WorkerError::new("route_denied", "Invalid fixed Werk route"));
        }
        let url = self
            .base
            .join(path)
            .map_err(|_| WorkerError::new("route_denied", "Invalid Werk route"))?;
        if url.origin() != self.base.origin() {
            return Err(WorkerError::new("origin_denied", "Werk origin changed"));
        }
        let mut request = self
            .client
            .request(method, url)
            .header("Accept", "application/json");
        if let Some(secret) = &self.secret {
            request = request.bearer_auth(secret.as_str());
        }
        if let Some(deployment) = &self.deployment {
            request = request.header("x-werk-deployment", deployment);
        }
        Ok(request)
    }
    pub fn origin(&self) -> String {
        self.base.origin().ascii_serialization()
    }
    pub fn redact(&self, text: &str) -> String {
        match &self.secret {
            Some(s) if !s.is_empty() => text.replace(s.as_str(), "[redacted]"),
            _ => text.into(),
        }
    }
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        self.send_chat(request.timeout(self.timeout)).await
    }
    // Streaming inference is bounded by inactivity and the engine's run/cleanup deadlines,
    // not an HTTP total-duration timer that discards a healthy long generation.
    async fn send_chat(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        request.send().await.map_err(|e| {
            if e.is_timeout() {
                WorkerError::transient("timeout")
            } else {
                WorkerError::transient("network_error")
            }
        })
    }
    pub async fn control(&self, path: &str) -> Result<Value> {
        self.control_request(reqwest::Method::GET, path, None).await
    }
    pub async fn control_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut request = self
            .request(method, path)?
            .header("X-Werk-Protocol-Version", "1.0");
        if let Some(body) = body {
            if body.to_string().len() > 1_048_576 {
                return Err(WorkerError::new(
                    "body_limit",
                    "Werk Protocol request exceeds 1 MiB",
                ));
            }
            request = request.json(&body);
        }
        let response = self.send(request).await?;
        let status = response.status();
        let headers = response.headers().clone();
        // Authentication middleware may reject before producing a Werk envelope.
        if matches!(status.as_u16(), 401 | 403) {
            return Err(http_error(status.as_u16()));
        }
        let bytes = bounded_body(response, self.max_body).await?;
        let value: Value = serde_json::from_str(&self.redact(&String::from_utf8_lossy(&bytes)))?;
        if value["protocol"] != json!({"major":1,"minor":0}) {
            return Err(WorkerError::new(
                "protocol_mismatch",
                "Unsupported Werk protocol version",
            ));
        }
        if headers
            .get("x-werk-protocol-version")
            .is_some_and(|v| v != "1.0")
        {
            return Err(WorkerError::new(
                "protocol_mismatch",
                "Werk header/envelope version disagreement",
            ));
        }
        let request_id = value["request_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| WorkerError::new("invalid_envelope", "Missing request ID"))?;
        validate_id(request_id)
            .map_err(|_| WorkerError::new("invalid_envelope", "Unsafe request ID"))?;
        if headers
            .get("x-werk-request-id")
            .is_some_and(|v| v.as_bytes() != request_id.as_bytes())
        {
            return Err(WorkerError::new(
                "invalid_envelope",
                "Request ID header/envelope disagreement",
            ));
        }
        if value.get("data").is_some() == value.get("error").is_some() {
            return Err(WorkerError::new(
                "invalid_envelope",
                "Exactly one data or error member is required",
            ));
        }
        if !status.is_success() {
            let error = &value["error"];
            let code = error["code"]
                .as_str()
                .filter(|code| validate_id(code).is_ok())
                .ok_or_else(|| {
                    WorkerError::new("invalid_envelope", "Missing protocol error code")
                })?;
            let message = error["message"].as_str().ok_or_else(|| {
                WorkerError::new("invalid_envelope", "Missing protocol error message")
            })?;
            let mut e = WorkerError::new(
                &format!("werk_protocol_{code}"),
                format!("{message} [request {request_id}]"),
            );
            e.retryable = error["retryable"].as_bool().unwrap_or(false)
                && matches!(status.as_u16(), 429 | 500..=599);
            e.retry_after_ms = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| s.saturating_mul(1000).min(30_000));
            return Err(e);
        }
        value
            .get("data")
            .cloned()
            .ok_or_else(|| WorkerError::new("invalid_envelope", "Success data missing"))
    }
    pub async fn json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = self.request(method, path)?;
        if let Some(body) = body {
            if body.to_string().len() > 1_048_576 {
                return Err(WorkerError::new("body_limit", "Werk request exceeds 1 MiB"));
            }
            req = req.json(&body);
        }
        let response = self.send(req).await?;
        if !response.status().is_success() {
            return Err(http_error(response.status().as_u16()));
        }
        let bytes = bounded_body(response, self.max_body).await?;
        serde_json::from_str(&self.redact(&String::from_utf8_lossy(&bytes))).map_err(Into::into)
    }
    pub async fn job(&self, job_id: &str, cancel: bool) -> Result<Value> {
        validate_id(job_id)?;
        self.json(
            if cancel {
                reqwest::Method::DELETE
            } else {
                reqwest::Method::GET
            },
            &format!("/v1/jobs/{job_id}"),
            None,
        )
        .await
    }
    pub async fn output(&self, output_id: &str) -> Result<Vec<u8>> {
        validate_id(output_id)?;
        let response = self
            .send(self.request(reqwest::Method::GET, &format!("/v1/outputs/{output_id}"))?)
            .await?;
        if !response.status().is_success() {
            return Err(http_error(response.status().as_u16()));
        }
        bounded_body(response, self.max_body).await
    }
}
fn http_error(status: u16) -> WorkerError {
    let code = match status {
        401 | 403 => "auth_error",
        429 => "rate_limit",
        300..=399 => "redirect_denied",
        400 | 404 | 422 => "invalid_request",
        _ => "server_error",
    };
    let mut e = WorkerError::new(code, format!("Werk returned HTTP {status}"));
    e.retryable = matches!(status, 429 | 500..=599);
    e
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationLimits {
    pub schema_version: u32,
    pub model: String,
    pub context_tokens: Option<usize>,
    pub max_output_tokens: Option<usize>,
    #[serde(default)]
    pub max_concurrent_requests: Option<usize>,
}
impl GenerationLimits {
    fn validate(&self, model: &str) -> Result<()> {
        if self.schema_version != 1
            || self
                .max_concurrent_requests
                .is_some_and(|n| !(1..=65536).contains(&n))
            || self.model != model
            || [self.context_tokens, self.max_output_tokens]
                .into_iter()
                .flatten()
                .any(|n| n == 0 || n > 2_000_000)
            || self
                .context_tokens
                .zip(self.max_output_tokens)
                .is_some_and(|(context, output)| output >= context)
        {
            return Err(WorkerError::new(
                "invalid_generation_limits",
                "Werk reported inconsistent generation limits",
            ));
        }
        Ok(())
    }
}

pub struct WerkProvider {
    pub client: WerkClient,
    config: WerkConfig,
    pub runtime_capabilities: Vec<RuntimeCapability>,
    pub discovery: Value,
    pub reasoning_defaults: BTreeMap<String, ReasoningDefaults>,
    pub inference_concurrency: InferenceConcurrency,
    refreshed_concurrency: Mutex<InferenceConcurrency>,
    capacity_refresh_at: std::sync::atomic::AtomicU64,
    capacity_refresh_gate: tokio::sync::Mutex<()>,
    context_limit: std::sync::atomic::AtomicUsize,
    output_limit: Option<usize>,
    pub context_limit_source: String,
    token_counts: Mutex<BTreeMap<String, usize>>,
    admission_counts: Mutex<BTreeMap<String, (usize, bool)>>,
    admission_unavailable: std::sync::atomic::AtomicBool,
    token_count_unavailable: std::sync::atomic::AtomicBool,
    image_token_count_unavailable: std::sync::atomic::AtomicBool,
    image_estimates: Mutex<BTreeMap<String, usize>>,
    inference_unresolved: Arc<std::sync::atomic::AtomicBool>,
}

// Dropping an HTTP request does not confirm that the remote inference stopped.
// Keep the connection closed to new work until the host reconnects after recovery.
struct InferenceReceipt {
    unresolved: Arc<std::sync::atomic::AtomicBool>,
    settled: bool,
}
impl Drop for InferenceReceipt {
    fn drop(&mut self) {
        if !self.settled {
            self.unresolved
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}
fn no_inference_retry(mut error: WorkerError) -> WorkerError {
    error.retryable = false;
    error
}
impl WerkProvider {
    fn check_inference_available(&self) -> Result<()> {
        if self
            .inference_unresolved
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(WorkerError::new(
                "inference_unresolved",
                "A previous inference ended without confirmed completion. Check or restart the Werk backend, then reconnect before sending more work.",
            ));
        }
        Ok(())
    }
    fn image_key(request: &ModelRequest) -> Option<String> {
        let images: Vec<_> = request
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.images.is_empty())
            .map(|(i, m)| (i, &m.role, &m.images))
            .collect();
        (!images.is_empty()).then(|| fingerprint(&images))
    }
    fn chat_body(&self, request: &ModelRequest) -> Result<Value> {
        if request.messages.iter().any(|m| !m.images.is_empty())
            && self.config.capabilities.images == Support::Unsupported
        {
            return Err(WorkerError::new(
                "images_unsupported",
                "Image input is disabled locally (capabilities.images=unsupported). Set Image input to Auto (unknown) and reconnect.",
            ));
        }
        let mut body = json!({"model":self.config.model,"messages":wire_messages(&request.messages),"stream":self.config.stream,"max_completion_tokens":request.max_output_tokens});
        if !request.tools.is_empty() {
            body["tools"]=Value::Array(request.tools.iter().map(|t|json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}})).collect());
            body["parallel_tool_calls"] =
                json!(self.config.capabilities.parallel_tools == Support::Supported);
        }
        if self.config.stream {
            body["stream_options"] = json!({"include_usage":true});
        }
        if self.config.capabilities.structured_output == Support::Supported
            && let Some(schema) = request.output_schema.clone()
        {
            body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"worker_output","schema":schema,"strict":true}});
        }
        for (key, value) in &self.config.options {
            if key == "reasoning_tier" {
                continue;
            }
            if key == "reasoning_effort" && (value.is_null() || request.reasoning_tier.is_some()) {
                continue;
            }
            body[key] = value.clone();
        }
        if let Some(tier) = &request.reasoning_tier {
            validate_reasoning_tier(&json!(tier))?;
            if tier != "default" && !self.supports_reasoning_tiers() {
                return Err(WorkerError::new(
                    "invalid_reasoning_tier",
                    "This server does not advertise reasoning tiers",
                ));
            }
        }
        if self.supports_reasoning_tiers()
            && (request.reasoning_tier.is_some()
                || !self
                    .config
                    .options
                    .get("reasoning_effort")
                    .is_some_and(|v| !v.is_null()))
        {
            let tier = request
                .reasoning_tier
                .as_deref()
                .or_else(|| {
                    self.config
                        .options
                        .get("reasoning_tier")
                        .and_then(Value::as_str)
                })
                .unwrap_or("default");
            body["werk"] = json!({"reasoning":{"tier":tier}});
        }
        if body.to_string().len() > 1_048_576 {
            return Err(WorkerError::new("body_limit", "Chat request exceeds 1 MiB"));
        }
        Ok(body)
    }
    pub async fn connect(config: WerkConfig, secret: Option<String>) -> Result<Self> {
        let client = WerkClient::new(&config, secret)?;
        Self::connect_client(config, client).await
    }

    async fn connect_client(config: WerkConfig, client: WerkClient) -> Result<Self> {
        for (key, value) in &config.options {
            match key.as_str() {
                "reasoning_effort" => validate_reasoning_effort(value)?,
                "reasoning_tier" => validate_reasoning_tier(value)?,
                "temperature" | "top_p" | "seed" | "stop" => {}
                _ => {
                    return Err(WorkerError::new(
                        "unsupported_option",
                        "Provider option is not explicitly allowed",
                    ));
                }
            }
        }
        if config
            .options
            .get("reasoning_tier")
            .is_some_and(|v| !v.is_null())
            && config
                .options
                .get("reasoning_effort")
                .is_some_and(|v| !v.is_null())
        {
            return Err(WorkerError::new(
                "invalid_reasoning_tier",
                "Choose reasoning_tier or native reasoning_effort, not both",
            ));
        }
        let info = client.control("/werk/v1/info").await?;
        if info["service"] != "werk1112" || info["protocol"] != json!({"major":1,"minor":0}) {
            return Err(WorkerError::new(
                "invalid_service",
                "Endpoint is not Werk Protocol 1.0",
            ));
        }
        // The general /v1/capabilities catalogue probes every installed model
        // and backend. Chat setup needs only protocol capabilities and model IDs;
        // media tools perform their own task discovery when explicitly configured.
        let (envelope, models) = tokio::try_join!(
            client.control("/werk/v1/capabilities"),
            client.json(reqwest::Method::GET, "/v1/models", None),
        )?;
        let runtime_capabilities: Vec<RuntimeCapability> =
            serde_json::from_value(envelope["capabilities"].clone())?;
        let generation_limits = if runtime_capabilities
            .iter()
            .any(|c| c.id == "api.chat.limits" && c.status == CapabilityStatus::Supported)
        {
            let url =
                reqwest::Url::parse_with_params("http://localhost/", &[("model", &config.model)])
                    .map_err(|_| {
                    WorkerError::new("invalid_generation_limits", "Invalid model query")
                })?;
            let value = tokio::time::timeout(
                Duration::from_secs(2),
                client.control(&format!("/werk/v1/chat/limits?{}", url.query().unwrap())),
            )
            .await
            .map_err(|_| {
                WorkerError::new(
                    "generation_limits_timeout",
                    "Werk limit discovery timed out",
                )
            })??;
            let limits: GenerationLimits = serde_json::from_value(value)?;
            limits.validate(&config.model)?;
            Some(limits)
        } else {
            None
        };
        if config
            .options
            .get("reasoning_tier")
            .and_then(Value::as_str)
            .is_some_and(|tier| tier != "default")
            && !runtime_capabilities
                .iter()
                .any(|c| c.id == "api.chat.reasoning" && c.status == CapabilityStatus::Supported)
        {
            return Err(WorkerError::new(
                "invalid_reasoning_tier",
                "This server does not advertise reasoning tiers; select Server default",
            ));
        }
        // Prefer fresh, model-matched Werk Protocol evidence. This samples only
        // running workers; it does not probe/load every installed backend.
        let snapshot = tokio::time::timeout(
            Duration::from_secs(4),
            client.control("/werk/v1/observability"),
        )
        .await
        .ok()
        .and_then(std::result::Result::ok);
        let protocol_capacity = generation_limits
            .as_ref()
            .and_then(|limits| limits.max_concurrent_requests)
            .map(configured_concurrency)
            .or_else(|| {
                snapshot
                    .as_ref()
                    .and_then(|value| observability_concurrency(value, &config.model))
            });
        let protocol_limit = snapshot
            .as_ref()
            .and_then(|value| observability_context(value, &config.model));
        let reasoning_defaults = snapshot.as_ref().map_or_else(BTreeMap::new, |snapshot| {
            snapshot["backends"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|worker| worker["model"].as_str())
                .filter_map(|model| {
                    observability_reasoning_defaults(snapshot, model)
                        .map(|defaults| (model.to_owned(), defaults))
                })
                .collect()
        });
        // Older protocol producers omit per-request context. Their deployment
        // extension is plain JSON and is only an optional fallback.
        let diagnostics = if protocol_capacity.is_none() || protocol_limit.is_none() {
            tokio::time::timeout(
                Duration::from_secs(2),
                client.json(reqwest::Method::GET, "/werk/v1/deployments", None),
            )
            .await
            .ok()
            .and_then(std::result::Result::ok)
        } else {
            None
        };
        let deployment_limit = diagnostics
            .as_ref()
            .and_then(|value| deployment_context(value, &config.model));
        let context_limit = [
            config.capabilities.context_tokens,
            generation_limits.as_ref().and_then(|l| l.context_tokens),
            protocol_limit,
            deployment_limit,
        ]
        .into_iter()
        .flatten()
        .min();
        let context_limit_source = if protocol_limit.is_some()
            || generation_limits
                .as_ref()
                .and_then(|l| l.context_tokens)
                .is_some()
        {
            "werk_protocol_and_host_minimum"
        } else if deployment_limit.is_some() {
            "deployment_and_host_minimum"
        } else if context_limit.is_some() {
            "host_config"
        } else {
            "unknown"
        }
        .to_owned();
        let inference_concurrency = protocol_capacity
            .or_else(|| {
                diagnostics
                    .as_ref()
                    .map(|value| deployment_concurrency(value, &config.model))
            })
            .unwrap_or_default();
        Ok(Self {
            client,
            config,
            refreshed_concurrency: Mutex::new(inference_concurrency.clone()),
            capacity_refresh_at: std::sync::atomic::AtomicU64::new(now_ms()),
            capacity_refresh_gate: tokio::sync::Mutex::new(()),
            inference_concurrency,
            context_limit: std::sync::atomic::AtomicUsize::new(context_limit.unwrap_or(0)),
            output_limit: generation_limits.as_ref().and_then(|l| l.max_output_tokens),
            context_limit_source,
            token_counts: Mutex::new(BTreeMap::new()),
            admission_counts: Mutex::new(BTreeMap::new()),
            admission_unavailable: std::sync::atomic::AtomicBool::new(false),
            token_count_unavailable: std::sync::atomic::AtomicBool::new(false),
            image_token_count_unavailable: std::sync::atomic::AtomicBool::new(false),
            image_estimates: Mutex::new(BTreeMap::new()),
            inference_unresolved: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            runtime_capabilities,
            reasoning_defaults,
            discovery: json!({"info":info,"models":models,"generation_limits":generation_limits}),
        })
    }

    pub fn supports_reasoning_tiers(&self) -> bool {
        self.runtime_capabilities
            .iter()
            .any(|c| c.id == "api.chat.reasoning" && c.status == CapabilityStatus::Supported)
    }

    pub async fn reasoning_profile(
        &self,
        model: &str,
        output: usize,
    ) -> Result<Option<ReasoningProfile>> {
        if !self.supports_reasoning_tiers() {
            return Ok(None);
        }
        let query: String = reqwest::Url::parse_with_params(
            "http://localhost/",
            &[
                ("model", model.to_owned()),
                ("max_output_tokens", output.to_string()),
            ],
        )
        .map_err(|_| WorkerError::new("invalid_reasoning_tier", "Invalid model query"))?
        .query()
        .unwrap()
        .to_owned();
        let value = self
            .client
            .control(&format!("/werk/v1/chat/reasoning?{query}"))
            .await?;
        let profile: ReasoningProfile = serde_json::from_value(value)?;
        let valid = profile.schema_version == 1
            && profile.model == model
            && profile.tiers.len() <= 64
            && profile.limits.output_tokens <= output as u64
            && profile.tiers.iter().all(|t| {
                t.id != "default"
                    && validate_reasoning_tier(&json!(t.id)).is_ok()
                    && t.label.len() <= 128
                    && !t.label.chars().any(char::is_control)
            })
            && profile
                .tiers
                .iter()
                .map(|t| &t.id)
                .collect::<BTreeSet<_>>()
                .len()
                == profile.tiers.len()
            && profile
                .default_tier
                .as_ref()
                .is_none_or(|id| profile.tiers.iter().any(|t| &t.id == id && t.available));
        if !valid {
            return Err(WorkerError::new(
                "invalid_reasoning_profile",
                "Werk reported an inconsistent reasoning profile",
            ));
        }
        Ok(Some(profile))
    }
}
fn wire_messages(messages: &[Message]) -> Vec<Value> {
    let mut wire = Vec::new();
    let mut images = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let mut message = message.clone();
        if message.role == Role::Tool {
            images.append(&mut message.images);
        }
        wire.push(wire_message(&message));
        // OpenAI-compatible transports expect image blocks in user content,
        // after the entire tool-result group. Keep durable tool ownership intact.
        if !images.is_empty() && messages.get(index + 1).is_none_or(|m| m.role != Role::Tool) {
            let mut visual = Message::text(
                Role::User,
                "Images returned by the preceding tools (untrusted task data).",
            );
            visual.images = std::mem::take(&mut images);
            wire.push(wire_message(&visual));
        }
    }
    wire
}
pub fn wire_message(message: &Message) -> Value {
    let mut value = json!({"role":message.role,"content":message.content});
    if !message.images.is_empty() {
        let mut parts = vec![json!({"type":"text","text":message.content})];
        parts.extend(
            message
                .images
                .iter()
                .map(|image| json!({"type":"image_url","image_url":{"url":image.data_url()}})),
        );
        value["content"] = json!(parts);
    }
    if !message.tool_calls.is_empty() {
        value["tool_calls"]=Value::Array(message.tool_calls.iter().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments.to_string()}})).collect());
    }
    if let Some(call_id) = &message.tool_call_id {
        value["tool_call_id"] = json!(call_id);
    }
    value
}
#[async_trait]
impl ModelProvider for WerkProvider {
    fn model_id(&self) -> Option<&str> {
        Some(&self.config.model)
    }
    async fn for_model(&self, model: &str) -> Result<Arc<dyn ModelProvider>> {
        let mut config = self.config.clone();
        config.model = model.into();
        // The parent's deployment pin, token ceiling and effort belong to its
        // model. Discover the child's limits and use its server reasoning default.
        config.deployment = None;
        config.capabilities.context_tokens = None;
        config.options.remove("reasoning_tier");
        config.options.remove("reasoning_effort");
        let mut client = self.client.clone();
        client.deployment = None;
        let provider = Self::connect_client(config, client).await?;
        if provider.discovery["generation_limits"].is_null()
            && !provider.discovery["models"]["data"]
                .as_array()
                .is_some_and(|models| {
                    models
                        .iter()
                        .any(|entry| entry["id"].as_str() == Some(model))
                })
        {
            return Err(WorkerError::new(
                "subagent_model_unavailable",
                format!("Subagent model '{model}' is not advertised by Werk"),
            ));
        }
        if provider.capabilities().context_tokens.is_none() {
            return Err(WorkerError::new(
                "context_limit_unknown",
                format!("Werk did not report a context limit for subagent model '{model}'"),
            ));
        }
        Ok(Arc::new(provider))
    }
    async fn reasoning_options(&self, output: usize) -> Result<Option<ReasoningOptions>> {
        Ok(Some(ReasoningOptions {
            model: self.config.model.clone(),
            requested: self
                .config
                .options
                .get("reasoning_tier")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .into(),
            native_effort: self
                .config
                .options
                .get("reasoning_effort")
                .filter(|v| !v.is_null())
                .cloned(),
            profile: self.reasoning_profile(&self.config.model, output).await?,
        }))
    }
    fn max_output_tokens(&self) -> Option<usize> {
        self.output_limit
    }
    fn estimated_image_tokens(&self, request: &ModelRequest) -> Option<usize> {
        let key = Self::image_key(request)?;
        self.image_estimates.lock().unwrap().get(&key).copied()
    }
    fn observe_completed_usage(&self, request: &ModelRequest, usage: &Usage) {
        let Some(key) = Self::image_key(request) else {
            return;
        };
        let Some(input) = usage.input_tokens.and_then(|n| usize::try_from(n).ok()) else {
            return;
        };
        let limit = self
            .context_limit
            .load(std::sync::atomic::Ordering::Relaxed);
        if input == 0 || input > limit {
            return;
        }
        let mut text = request.clone();
        for message in &mut text.messages {
            message.images.clear();
        }
        // Only a native text count for this exact request can establish image
        // overhead. Never subtract byte estimates, cached-token usage, or counts
        // from another request. Eviction safely leaves the fallback in place.
        let Some(text_tokens) = self.cached_input_tokens(&text) else {
            return;
        };
        let Some(overhead) = input.checked_sub(text_tokens).filter(|n| *n > 0) else {
            return;
        };
        let reserve = overhead.saturating_add(overhead.div_ceil(4).max(256));
        let mut estimates = self.image_estimates.lock().unwrap();
        if estimates.len() >= 64 && !estimates.contains_key(&key) {
            estimates.clear();
        }
        // Keep the largest observation for this ordered image set and placement.
        // Model/options are fixed for the connection; reconnect discards it.
        estimates
            .entry(key)
            .and_modify(|n| *n = (*n).max(reserve))
            .or_insert(reserve);
    }
    fn concurrency(&self) -> InferenceConcurrency {
        self.refreshed_concurrency.lock().unwrap().clone()
    }
    async fn refresh_concurrency(&self) {
        use std::sync::atomic::Ordering;
        if now_ms().saturating_sub(self.capacity_refresh_at.load(Ordering::Relaxed)) < 5000 {
            return;
        }
        let Ok(_gate) = self.capacity_refresh_gate.try_lock() else {
            return;
        };
        if now_ms().saturating_sub(self.capacity_refresh_at.load(Ordering::Relaxed)) < 5000 {
            return;
        }
        let mut capacity = None;
        let mut authoritative_unknown = false;
        if self
            .runtime_capabilities
            .iter()
            .any(|c| c.id == "api.chat.limits" && c.status == CapabilityStatus::Supported)
            && let Ok(url) = reqwest::Url::parse_with_params(
                "http://localhost/",
                &[("model", &self.config.model)],
            )
            && let Ok(Ok(value)) = tokio::time::timeout(
                Duration::from_secs(2),
                self.client
                    .control(&format!("/werk/v1/chat/limits?{}", url.query().unwrap())),
            )
            .await
            && let Ok(limits) = serde_json::from_value::<GenerationLimits>(value)
            && limits.validate(&self.config.model).is_ok()
        {
            capacity = limits.max_concurrent_requests.map(configured_concurrency);
            authoritative_unknown = capacity.is_none();
        }
        if capacity.is_none()
            && let Ok(Ok(value)) = tokio::time::timeout(
                Duration::from_secs(4),
                self.client.control("/werk/v1/observability"),
            )
            .await
        {
            capacity = observability_concurrency(&value, &self.config.model);
        }
        if let Some(capacity) = capacity {
            *self.refreshed_concurrency.lock().unwrap() = capacity;
        } else if authoritative_unknown {
            *self.refreshed_concurrency.lock().unwrap() = InferenceConcurrency::default();
        }
        self.capacity_refresh_at.store(now_ms(), Ordering::Relaxed);
    }
    fn capabilities(&self) -> ModelCapabilities {
        let mut caps = self.config.capabilities.clone();
        let limit = self.context_limit.load(std::sync::atomic::Ordering::SeqCst);
        caps.context_tokens = (limit > 0).then_some(limit);
        caps
    }
    fn identity(&self) -> String {
        format!("werk:{}", self.config.model)
    }
    fn request_fingerprint(&self, request: &ModelRequest) -> String {
        fingerprint(&(&self.config, self.supports_reasoning_tiers(), request))
    }
    fn cached_input_tokens(&self, request: &ModelRequest) -> Option<usize> {
        self.token_counts
            .lock()
            .unwrap()
            .get(&fingerprint(request))
            .copied()
    }
    fn input_token_source(&self, request: &ModelRequest) -> &'static str {
        if let Some((_, native)) = self
            .admission_counts
            .lock()
            .unwrap()
            .get(&fingerprint(request))
        {
            return if *native {
                "native_exact"
            } else {
                "server_admission_estimate"
            };
        }
        if self.cached_input_tokens(request).is_some() {
            return "native_exact";
        }
        if request.messages.iter().any(|m| !m.images.is_empty()) {
            let mut text = request.clone();
            for message in &mut text.messages {
                message.images.clear();
            }
            if self.cached_input_tokens(&text).is_some() {
                return "native_text_plus_image_estimate";
            }
        }
        // A custom context builder or an eviction may not have counted this
        // request yet. Absence from an exact cache is not evidence of fallback.
        if !self
            .token_count_unavailable
            .load(std::sync::atomic::Ordering::Relaxed)
            && self.runtime_capabilities.iter().any(|cap| {
                cap.id == "api.chat.token_count"
                    && cap.status == CapabilityStatus::Supported
                    && cap.operations.iter().any(|op| op == "count_tokens")
            })
        {
            return "provider_unspecified";
        }
        "wire_bytes_estimate"
    }
    async fn admission_input_tokens(&self, request: &ModelRequest) -> Result<Option<usize>> {
        self.check_inference_available()?;
        if self
            .admission_unavailable
            .load(std::sync::atomic::Ordering::Relaxed)
            || !self.runtime_capabilities.iter().any(|c| {
                c.id == "api.chat.admission"
                    && c.status == CapabilityStatus::Supported
                    && c.operations.iter().any(|op| op == "admission")
            })
        {
            return Ok(None);
        }
        let key = fingerprint(request);
        if let Some((count, _)) = self.admission_counts.lock().unwrap().get(&key) {
            return Ok(Some(*count));
        }
        let mut body = self.chat_body(request)?;
        body["stream"] = json!(false);
        body.as_object_mut().unwrap().remove("stream_options");
        let mut retries = 0;
        let value = loop {
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                self.client.control_request(
                    reqwest::Method::POST,
                    "/werk/v1/chat/admission",
                    Some(body.clone()),
                ),
            )
            .await
            .unwrap_or_else(|_| Err(WorkerError::transient("admission_timeout")));
            match result {
                Ok(value) => break value,
                Err(error) if error.code == "werk_protocol_unsupported" => {
                    self.admission_unavailable
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    return Ok(None);
                }
                Err(error) if error.retryable && retries < 2 => {
                    retries += 1;
                    tokio::time::sleep(Duration::from_millis(
                        error.retry_after_ms.unwrap_or(100 * retries),
                    ))
                    .await;
                }
                Err(error) => return Err(error),
            }
        };
        let valid_count = |v: &Value| {
            v.as_u64()
                .filter(|n| *n > 0 && *n <= 2_000_000)
                .map(|n| n as usize)
        };
        let count = valid_count(&value["input_tokens"])
            .ok_or_else(|| WorkerError::new("invalid_envelope", "Invalid admission token count"))?;
        if value["model"].as_str() != Some(&self.config.model)
            || value["max_output_tokens"].as_u64() != Some(request.max_output_tokens as u64)
            || !matches!(
                value["token_count_method"].as_str(),
                Some("native" | "estimate")
            )
        {
            return Err(WorkerError::new(
                "invalid_envelope",
                "Admission response does not match this model/request or count method",
            ));
        }
        if !value["context_tokens"].is_null() {
            let context = valid_count(&value["context_tokens"])
                .filter(|n| *n > 512)
                .ok_or_else(|| {
                    WorkerError::new("invalid_envelope", "Invalid admission context limit")
                })?;
            self.context_limit
                .fetch_min(context, std::sync::atomic::Ordering::SeqCst);
        }
        let native = value["token_count_method"] == "native";
        if native {
            let mut exact = self.token_counts.lock().unwrap();
            if exact.len() >= 64 {
                exact.clear();
            }
            exact.insert(key.clone(), count);
        }
        let mut cache = self.admission_counts.lock().unwrap();
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, (count, native));
        Ok(Some(count))
    }
    async fn count_input_tokens(&self, request: &ModelRequest) -> Result<Option<usize>> {
        self.check_inference_available()?;
        if let Some(count) = self.cached_input_tokens(request) {
            return Ok(Some(count));
        }
        let unavailable = if request.messages.iter().any(|m| !m.images.is_empty()) {
            &self.image_token_count_unavailable
        } else {
            &self.token_count_unavailable
        };
        if unavailable.load(std::sync::atomic::Ordering::Relaxed)
            || !self.runtime_capabilities.iter().any(|cap| {
                cap.id == "api.chat.token_count"
                    && cap.status == CapabilityStatus::Supported
                    && cap.operations.iter().any(|op| op == "count_tokens")
            })
        {
            return Ok(None);
        }
        let mut body = self.chat_body(request)?;
        body["stream"] = json!(false);
        body.as_object_mut().unwrap().remove("stream_options");
        let mut retries = 0;
        let value = loop {
            self.check_inference_available()?;
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                self.client.control_request(
                    reqwest::Method::POST,
                    "/werk/v1/chat/count_tokens",
                    Some(body.clone()),
                ),
            )
            .await
            .unwrap_or_else(|_| Err(WorkerError::transient("token_count_timeout")));
            match result {
                Ok(value) => break value,
                Err(error) if error.code == "werk_protocol_unsupported" => {
                    // Cache unsupported separately by modality. Image counting
                    // must not disable the useful text/tool counter, nor be
                    // retried for every context-planning/admission pass.
                    unavailable.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Ok(None);
                }
                Err(error) if error.retryable && retries < 2 => {
                    retries += 1;
                    tokio::time::sleep(Duration::from_millis(
                        error.retry_after_ms.unwrap_or(100 * retries),
                    ))
                    .await;
                }
                Err(error) => return Err(error),
            }
        };
        if value["model"].as_str() != Some(&self.config.model) {
            return Err(WorkerError::new(
                "invalid_envelope",
                "Token count returned a different model",
            ));
        }
        let count = value["input_tokens"]
            .as_u64()
            .filter(|n| *n > 0 && *n <= 2_000_000)
            .ok_or_else(|| WorkerError::new("invalid_envelope", "Invalid native token count"))?
            as usize;
        let mut cache = self.token_counts.lock().unwrap();
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(fingerprint(request), count);
        Ok(Some(count))
    }
    fn deadline_drain_timeout(&self) -> Duration {
        Duration::from_millis(self.config.deadline_drain_timeout_ms)
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        self.check_inference_available()?;
        if self
            .output_limit
            .is_some_and(|limit| request.max_output_tokens > limit)
        {
            return Err(WorkerError::new(
                "output_limit_exceeded",
                "Request exceeds the model/server output ceiling",
            ));
        }
        // Admission may report a smaller live limit than initial discovery.
        let input_tokens = self.estimate_input_tokens(&request).await?;
        let limit = self.capabilities().context_tokens.ok_or_else(|| WorkerError::new("context_limit_unknown", "Set capabilities.context_tokens to the server's per-request context limit; discovery could not establish it"))?;
        if request.max_output_tokens == 0
            || input_tokens
                .saturating_add(request.max_output_tokens)
                .saturating_add(512)
                > limit
        {
            return Err(WorkerError::new(
                "context_overflow",
                "Request exceeds the known server context limit",
            ));
        }
        if !request.tools.is_empty() && self.config.capabilities.tool_calling != Support::Supported
        {
            return Err(WorkerError::new(
                "unsupported_capability",
                "Tool calling must be explicitly confirmed for this model",
            ));
        }
        if self.config.stream && self.config.capabilities.streaming != Support::Supported {
            return Err(WorkerError::new(
                "unsupported_capability",
                "Streaming support is not confirmed",
            ));
        }
        let body = self.chat_body(&request)?;
        let request = self
            .client
            .request(reqwest::Method::POST, "/v1/chat/completions")?
            .json(&body);
        self.check_inference_available()?;
        let mut receipt = InferenceReceipt {
            unresolved: self.inference_unresolved.clone(),
            settled: false,
        };
        let response = if self.config.stream {
            self.client
                .send_chat(request)
                .await
                .map_err(no_inference_retry)?
        } else {
            self.client
                .send(request)
                .await
                .map_err(no_inference_retry)?
        };
        if !response.status().is_success() {
            // An explicit HTTP rejection remains eligible for ordinary bounded
            // retries (e.g. 429/503); an uncertain transport failure does not.
            receipt.settled = true;
            let mut e = http_error(response.status().as_u16());
            e.retry_after_ms = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| s.saturating_mul(1000).min(30_000));
            if response.status().as_u16() == 400 {
                let bytes = bounded_body(response, self.config.max_body_bytes.min(65536)).await?;
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                    && value["error"]["code"] == "context_length_exceeded"
                {
                    let reported = value["error"]["context_length"]
                        .as_u64()
                        .or_else(|| value["error"]["details"]["context_length"].as_u64());
                    if let Some(next) = reported
                        .filter(|n| *n > 512 && *n <= 2_000_000)
                        .map(|n| n as usize)
                    {
                        let previous = self
                            .context_limit
                            .fetch_min(next, std::sync::atomic::Ordering::SeqCst);
                        if next < previous {
                            return Err(WorkerError::new(
                                "context_limit_updated",
                                "Server context limit decreased; rebuild required",
                            ));
                        }
                    }
                    let detail = |field: &str| {
                        value["error"]
                            .get(field)
                            .or_else(|| value["error"]["details"].get(field))
                    };
                    let input = detail("prompt_tokens").and_then(Value::as_u64);
                    let output = detail("max_tokens").and_then(Value::as_u64);
                    let method = detail("token_count_method")
                        .and_then(Value::as_str)
                        .filter(|m| matches!(*m, "native" | "estimate"))
                        .unwrap_or("unknown");
                    return Err(WorkerError::new(
                        "context_overflow",
                        format!(
                            "Server rejected context admission: input={input:?}, output={output:?}, context={reported:?}, count_method={method}. Rebuild against the server admission budget; no smaller context limit was reported."
                        ),
                    ));
                }
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                    && matches!(
                        value["error"]["code"].as_str(),
                        Some("invalid_reasoning_effort" | "invalid_reasoning_tier")
                    )
                {
                    let message = value["error"]["message"].as_str().unwrap_or(
                        "The server/model rejected reasoning_effort; select Server default or a supported level",
                    );
                    return Err(WorkerError::new(
                        value["error"]["code"].as_str().unwrap(),
                        self.client
                            .redact(message)
                            .chars()
                            .take(2048)
                            .collect::<String>(),
                    ));
                }
            }
            return Err(e);
        }
        let client = self.client.clone();
        let max = self.config.max_body_bytes;
        if !self.config.stream {
            let bytes = bounded_body(response, max)
                .await
                .map_err(no_inference_retry)?;
            receipt.settled = true;
            let v: Value = serde_json::from_str(&client.redact(&String::from_utf8_lossy(&bytes)))?;
            let deltas = decode_completion(&v)?;
            return Ok(Box::pin(futures_util::stream::iter(
                deltas.into_iter().map(Ok),
            )));
        }
        Ok(Box::pin(async_stream::try_stream! {
            // Capture the guard even if the returned stream is never polled.
            let mut receipt = receipt;
            let mut bytes=response.bytes_stream();let mut parser=SseDecoder::new(max);
            let mut redactor=StreamRedactor::new(client.secret.as_ref().map(|s|s.as_str().to_owned()));
            while let Some(chunk)=bytes.next().await {
                let chunk=chunk.map_err(|error|no_inference_retry(WorkerError::transient(if error.is_timeout() {"timeout"} else {"interrupted_stream"})))?;
                let deltas = parser.push(&chunk).map_err(|mut error| {
                    error.message = client.redact(&error.message);
                    if error.code == "provider_error" { receipt.settled = true; }
                    error.retryable = false;
                    error
                })?;
                if deltas.iter().any(|delta| matches!(delta, ModelDelta::Finish { .. } | ModelDelta::Done)) {
                    receipt.settled = true;
                }
                for delta in deltas {for clean in redactor.push(delta) {yield clean;}}
            }
            parser.finish().map_err(no_inference_retry)?;
        }))
    }
}

/// Retain the possible secret suffix across arbitrary provider chunk boundaries.
/// Text and each tool field have independent incremental buffers.
pub struct StreamRedactor {
    secret: Option<String>,
    text: String,
    tools: BTreeMap<usize, (String, String, String)>,
}
impl StreamRedactor {
    pub fn new(secret: Option<String>) -> Self {
        Self {
            secret: secret.filter(|s| !s.is_empty()),
            text: String::new(),
            tools: BTreeMap::new(),
        }
    }
    fn take(buffer: &mut String, secret: &str, flush: bool) -> String {
        *buffer = buffer.replace(secret, "[redacted]");
        let mut end = if flush {
            buffer.len()
        } else {
            buffer.len().saturating_sub(secret.len().saturating_sub(1))
        };
        while !buffer.is_char_boundary(end) {
            end -= 1;
        }
        buffer.drain(..end).collect()
    }
    pub fn push(&mut self, delta: ModelDelta) -> Vec<ModelDelta> {
        let Some(secret) = &self.secret else {
            return vec![delta];
        };
        let mut result = vec![];
        match delta {
            ModelDelta::Text { text } => {
                self.text.push_str(&text);
                let text = Self::take(&mut self.text, secret, false);
                if !text.is_empty() {
                    result.push(ModelDelta::Text { text });
                }
            }
            ModelDelta::Tool {
                index,
                id,
                name,
                arguments,
            } => {
                let t = self.tools.entry(index).or_default();
                t.0.push_str(&id);
                t.1.push_str(&name);
                t.2.push_str(&arguments);
                let id = Self::take(&mut t.0, secret, false);
                let name = Self::take(&mut t.1, secret, false);
                let arguments = Self::take(&mut t.2, secret, false);
                result.push(ModelDelta::Tool {
                    index,
                    id,
                    name,
                    arguments,
                });
            }
            ModelDelta::Finish { .. } | ModelDelta::Done => {
                let text = Self::take(&mut self.text, secret, true);
                if !text.is_empty() {
                    result.push(ModelDelta::Text { text });
                }
                for (index, t) in &mut self.tools {
                    let id = Self::take(&mut t.0, secret, true);
                    let name = Self::take(&mut t.1, secret, true);
                    let arguments = Self::take(&mut t.2, secret, true);
                    if !(id.is_empty() && name.is_empty() && arguments.is_empty()) {
                        result.push(ModelDelta::Tool {
                            index: *index,
                            id,
                            name,
                            arguments,
                        });
                    }
                }
                result.push(delta);
            }
            ModelDelta::Metadata { value } => {
                let clean = value.to_string().replace(secret, "[redacted]");
                result.push(ModelDelta::Metadata {
                    value: serde_json::from_str(&clean).unwrap_or(Value::Null),
                });
            }
            _ => result.push(delta),
        }
        result
    }
}
fn usage(v: &Value) -> Option<Usage> {
    if v.is_null() {
        None
    } else {
        Some(Usage {
            input_tokens: v["prompt_tokens"].as_u64(),
            output_tokens: v["completion_tokens"].as_u64(),
            cached_tokens: v["prompt_tokens_details"]["cached_tokens"].as_u64(),
        })
    }
}
// Public Werk chat extension. Persist only documented numeric measurements,
// never arbitrary backend diagnostics or error text as runtime control signals.
fn runtime_measurements(value: &Value) -> Option<Value> {
    let mut timings = serde_json::Map::new();
    for key in [
        "cached_prompt_tokens",
        "load_seconds",
        "warmup_seconds",
        "first_token_seconds",
        "prompt_seconds",
        "decode_seconds",
        "total_seconds",
    ] {
        let measurement = &value["werk"]["timings"][key];
        let valid = if key == "cached_prompt_tokens" {
            measurement.as_u64().is_some()
        } else {
            measurement
                .as_f64()
                .is_some_and(|n| n.is_finite() && n >= 0.0)
        };
        if valid {
            timings.insert(key.into(), value["werk"]["timings"][key].clone());
        }
    }
    (!timings.is_empty()).then(|| json!({"werk":{"timings":timings}}))
}
fn check_provider_error(v: &Value) -> Result<()> {
    if let Some(error) = v.get("error").filter(|error| !error.is_null()) {
        let message = error["message"]
            .as_str()
            .or_else(|| error.as_str())
            .unwrap_or("Backend returned an error without a message");
        // Do not let an upstream code trigger local recovery or retry policies.
        return Err(WorkerError::new("provider_error", message));
    }
    Ok(())
}

pub fn decode_completion(v: &Value) -> Result<Vec<ModelDelta>> {
    check_provider_error(v)?;
    if v["choices"].as_array().is_none_or(|c| c.len() != 1) {
        return Err(WorkerError::new(
            "invalid_response",
            "Expected one completion choice",
        ));
    }
    let choice = &v["choices"][0];
    let message = &choice["message"];
    let mut out = Vec::new();
    if let Some(t) = message["content"].as_str() {
        out.push(ModelDelta::Text { text: t.into() });
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        for (index, c) in calls.iter().enumerate() {
            out.push(ModelDelta::Tool {
                index,
                id: c["id"].as_str().unwrap_or("").into(),
                name: c["function"]["name"].as_str().unwrap_or("").into(),
                arguments: c["function"]["arguments"].as_str().unwrap_or("").into(),
            });
        }
    }
    if let Some(usage) = usage(&v["usage"]) {
        out.push(ModelDelta::Usage { usage });
    }
    if let Some(value) = runtime_measurements(v) {
        out.push(ModelDelta::Metadata { value });
    }
    out.push(ModelDelta::Finish {
        reason: choice["finish_reason"].as_str().unwrap_or("").into(),
    });
    out.push(ModelDelta::Done);
    Ok(out)
}
pub struct SseDecoder {
    buffer: Vec<u8>,
    data: String,
    total: usize,
    max: usize,
    done: bool,
}
impl SseDecoder {
    pub fn new(max: usize) -> Self {
        Self {
            buffer: vec![],
            data: String::new(),
            total: 0,
            max,
            done: false,
        }
    }
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ModelDelta>> {
        self.total += chunk.len();
        if self.total > self.max {
            return Err(WorkerError::new(
                "body_limit",
                "SSE response exceeds body limit",
            ));
        }
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line = self.buffer.drain(..=end).collect::<Vec<_>>();
            let line = std::str::from_utf8(&line)
                .map_err(|_| WorkerError::new("invalid_stream", "Invalid UTF-8 in SSE line"))?
                .trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    let data = std::mem::take(&mut self.data);
                    out.extend(self.decode(data.trim_end_matches('\n'))?);
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
                self.data.push('\n');
            }
        }
        Ok(out)
    }
    fn decode(&mut self, data: &str) -> Result<Vec<ModelDelta>> {
        if self.done {
            return Err(WorkerError::new("invalid_stream", "SSE data after DONE"));
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(vec![ModelDelta::Done]);
        }
        let v: Value = serde_json::from_str(data)?;
        check_provider_error(&v)?;
        let mut out = Vec::new();
        if let Some(value) = runtime_measurements(&v) {
            out.push(ModelDelta::Metadata { value });
        }
        if let Some(usage) = usage(&v["usage"]) {
            out.push(ModelDelta::Usage { usage });
        }
        if let Some(choices) = v["choices"].as_array() {
            for choice in choices {
                if choice["index"].as_u64().unwrap_or(0) != 0 {
                    return Err(WorkerError::new(
                        "invalid_stream",
                        "Multiple choices not supported",
                    ));
                }
                let delta = &choice["delta"];
                if let Some(text) = delta["content"].as_str() {
                    out.push(ModelDelta::Text { text: text.into() });
                }
                if let Some(calls) = delta["tool_calls"].as_array() {
                    for c in calls {
                        out.push(ModelDelta::Tool {
                            index: c["index"].as_u64().ok_or_else(|| {
                                WorkerError::new("invalid_stream", "Missing tool index")
                            })? as usize,
                            id: c["id"].as_str().unwrap_or("").into(),
                            name: c["function"]["name"].as_str().unwrap_or("").into(),
                            arguments: c["function"]["arguments"].as_str().unwrap_or("").into(),
                        });
                    }
                }
                if let Some(reason) = choice["finish_reason"].as_str() {
                    out.push(ModelDelta::Finish {
                        reason: reason.into(),
                    });
                }
            }
        } else {
            return Err(WorkerError::new(
                "invalid_stream",
                "SSE chunk missing choices",
            ));
        }
        Ok(out)
    }
    pub fn finish(&self) -> Result<()> {
        if !self.done || !self.buffer.is_empty() || !self.data.is_empty() {
            return Err(WorkerError::transient("interrupted_stream"));
        }
        Ok(())
    }
}

/// Match the chat routing alias/exact ID, never sum unrelated models or free slots.
fn configured_concurrency(slots: usize) -> InferenceConcurrency {
    InferenceConcurrency {
        max_requests: Some(slots),
        source: "werk_protocol_limits".into(),
        detail: format!(
            "{slots} configured model slots from Werk Protocol; capacity refreshes during execution"
        ),
    }
}

fn deployment_concurrency(value: &Value, model: &str) -> InferenceConcurrency {
    let Some(profiles) = value
        .pointer("/deployments/profiles")
        .and_then(Value::as_array)
    else {
        return InferenceConcurrency::default();
    };
    let mut total = 0usize;
    let mut ids = BTreeSet::new();
    for entry in profiles {
        let profile = &entry["plan"]["profile"];
        if profile["id"].as_str() != Some(model) && profile["alias"].as_str() != Some(model) {
            continue;
        }
        let Some(id) = profile["id"].as_str() else {
            return InferenceConcurrency::default();
        };
        let Some(slots) = profile["parallel"]
            .as_u64()
            .filter(|n| (1..=65536).contains(n))
        else {
            return InferenceConcurrency::default();
        };
        if !ids.insert(id) {
            return InferenceConcurrency::default();
        }
        total = total.saturating_add(slots as usize);
    }
    if total == 0 {
        return InferenceConcurrency::default();
    }
    InferenceConcurrency {
        max_requests: Some(total),
        source: "werk_deployment_diagnostics".into(),
        detail: format!(
            "{total} configured slots across {} matching deployment(s); other clients may consume capacity",
            ids.len()
        ),
    }
}

/// Protocol-validated native slot telemetry for ordinary (non-deployment) servers.
/// Use the minimum across matching workers: telemetry is not a routing guarantee.
fn observability_fresh(value: &Value) -> Option<()> {
    if value["schema_version"] != 1 {
        return None;
    }
    let observed = value["observed_at_ms"].as_u64()?;
    let sampled = value["backend_observed_at_ms"].as_u64()?;
    if observed.abs_diff(sampled) > 10_000 {
        return None;
    }
    Some(())
}

fn observability_reasoning_defaults(value: &Value, model: &str) -> Option<ReasoningDefaults> {
    observability_fresh(value)?;
    let mut defaults = None;
    for worker in value["backends"].as_array()? {
        if worker["model"].as_str() != Some(model) {
            continue;
        }
        if worker["available"] != true {
            return None;
        }
        let reported: ReasoningDefaults =
            serde_json::from_value(worker.get("reasoning_defaults")?.clone()).ok()?;
        if reported.source != "worker_launch_configuration"
            || reported
                .budget_tokens
                .is_some_and(|n| !(-1..=i32::MAX as i64).contains(&n))
            || reported.effort.as_ref().is_some_and(|v| {
                !v.is_number()
                    && !v.as_str().is_some_and(|s| {
                        !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
                    })
            })
            || (reported.effort.is_none()
                && reported.thinking.is_none()
                && reported.budget_tokens.is_none())
        {
            return None;
        }
        if defaults
            .as_ref()
            .is_some_and(|previous| *previous != reported)
        {
            return None;
        }
        defaults = Some(reported);
    }
    defaults
}

fn observability_concurrency(value: &Value, model: &str) -> Option<InferenceConcurrency> {
    observability_fresh(value)?;
    let mut capacity = None;
    for worker in value["backends"].as_array()? {
        if worker["model"].as_str() != Some(model) {
            continue;
        }
        if worker["available"] != true || !worker["backend"].as_str()?.starts_with("llama.cpp / ") {
            return None;
        }
        let slots = worker["gauges"]["slots_total"].as_f64()?;
        if !(1.0..=65536.0).contains(&slots) || slots.fract() != 0.0 {
            return None;
        }
        capacity = Some(capacity.map_or(slots as usize, |n: usize| n.min(slots as usize)));
    }
    capacity.map(|slots| InferenceConcurrency { max_requests:Some(slots), source:"werk_protocol_observability".into(), detail:format!("{slots} native slots from matching live llama.cpp telemetry; reconnect after server capacity changes") })
}

fn deployment_context(value: &Value, model: &str) -> Option<usize> {
    let mut limit: Option<usize> = None;
    for entry in value.pointer("/deployments/profiles")?.as_array()? {
        let p = &entry["plan"]["profile"];
        if p["id"].as_str() != Some(model) && p["alias"].as_str() != Some(model) {
            continue;
        }
        let per_request = p["context"]
            .as_u64()
            .filter(|n| *n > 512 && *n <= 2_000_000)? as usize;
        p["parallel"].as_u64().filter(|n| *n > 0 && *n <= 65536)?;
        // Werk deployment context is per sequence. The llama launcher multiplies
        // it by parallel slots when constructing the native aggregate -c value.
        if !matches!(
            p["runtime"].as_str()?,
            "vllm_cuda" | "llama_cuda" | "llama_cuda_offload"
        ) {
            return None;
        }
        limit = Some(limit.map_or(per_request, |n| n.min(per_request)));
    }
    limit
}
fn observability_context(value: &Value, model: &str) -> Option<usize> {
    observability_concurrency(value, model)?;
    let mut limit: Option<usize> = None;
    for b in value["backends"].as_array()? {
        if b["model"].as_str() != Some(model) {
            continue;
        }
        let tokens = if b["gauges"].get("context_per_request_tokens").is_some() {
            b["gauges"]["context_per_request_tokens"].as_f64()?
        } else if b["gauges"]["slots_total"].as_f64()? == 1.0 {
            // Compatibility with older protocol producers: an aggregate is only
            // safe evidence of an individual limit when exactly one slot exists.
            b["gauges"]["context_capacity_tokens"].as_f64()?
        } else {
            return None;
        };
        if !(513.0..=2_000_000.0).contains(&tokens) || tokens.fract() != 0.0 {
            return None;
        }
        limit = Some(limit.map_or(tokens as usize, |n| n.min(tokens as usize)));
    }
    limit
}

#[cfg(test)]
mod tool_image_transport_tests {
    use super::*;
    #[test]
    fn image_results_follow_the_complete_tool_group() {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(16, 16)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let mut result = ToolResult::ok(json!({"path":"shot.png"}));
        result
            .images
            .push(crate::attachments::ImageAttachment::from_bytes(&bytes.into_inner()).unwrap());
        let messages = vec![
            Message::tool("one", &result),
            Message::tool("two", &ToolResult::ok(json!({"content":"source"}))),
        ];
        let wire = wire_messages(&messages);
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[0]["role"], "tool");
        assert!(wire[0]["content"].is_string());
        assert!(!wire[0].to_string().contains("base64"));
        assert_eq!(wire[1]["tool_call_id"], "two");
        assert_eq!(wire[2]["role"], "user");
        assert_eq!(wire[2]["content"][1]["type"], "image_url");
    }
}
