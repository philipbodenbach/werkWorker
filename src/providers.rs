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
    /// Verified server capacity, independent of parallel tool-call support.
    fn concurrency(&self) -> InferenceConcurrency {
        InferenceConcurrency::default()
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
                "Server stopped at its output token limit; incomplete output is not executable",
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
/// Credentials are resolved separately and never serialized with configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WerkConfig {
    pub base_url: String,
    pub model: String,
    pub deployment: Option<String>,
    pub capabilities: ModelCapabilities,
    pub stream: bool,
    pub timeout_ms: u64,
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
            max_body_bytes: 8 * 1024 * 1024,
            options: BTreeMap::new(),
        }
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
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|_| WorkerError::new("http_client", "Cannot construct Werk client"))?;
        Ok(Self {
            client,
            base,
            secret: secret.map(Arc::new),
            deployment: config.deployment.clone(),
            max_body: config.max_body_bytes,
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
        request.send().await.map_err(|e| {
            if e.is_timeout() {
                WorkerError::transient("timeout")
            } else {
                WorkerError::transient("network_error")
            }
        })
    }
    pub async fn control(&self, path: &str) -> Result<Value> {
        let response = self
            .send(
                self.request(reqwest::Method::GET, path)?
                    .header("X-Werk-Protocol-Version", "1.0"),
            )
            .await?;
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
            let mut e = http_error(status.as_u16());
            e.retryable = value["error"]["retryable"].as_bool().unwrap_or(false)
                && matches!(status.as_u16(), 429 | 500..=599);
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
pub struct WerkProvider {
    pub client: WerkClient,
    config: WerkConfig,
    pub runtime_capabilities: Vec<RuntimeCapability>,
    pub discovery: Value,
    pub inference_concurrency: InferenceConcurrency,
    context_limit: std::sync::atomic::AtomicUsize,
    pub context_limit_source: String,
}
impl WerkProvider {
    pub async fn connect(config: WerkConfig, secret: Option<String>) -> Result<Self> {
        let client = WerkClient::new(&config, secret)?;
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
        for key in config.options.keys() {
            if !matches!(key.as_str(), "temperature" | "top_p" | "seed" | "stop") {
                return Err(WorkerError::new(
                    "unsupported_option",
                    "Provider option is not explicitly allowed",
                ));
            }
        }
        // This deployed Werk route returns ordinary JSON, not a Protocol envelope.
        // Optional discovery must never make older/read-restricted servers unusable.
        let diagnostics = tokio::time::timeout(
            Duration::from_secs(2),
            client.json(reqwest::Method::GET, "/werk/v1/deployments", None),
        )
        .await;
        let deployment_limit = diagnostics
            .as_ref()
            .ok()
            .and_then(|v| v.as_ref().ok())
            .and_then(|v| deployment_context(v, &config.model));
        let mut context_limit = match (config.capabilities.context_tokens, deployment_limit) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let mut context_limit_source = if deployment_limit.is_some() {
            "deployment_and_host_minimum"
        } else if context_limit.is_some() {
            "host_config"
        } else {
            "unknown"
        }
        .to_owned();
        let mut inference_concurrency = match diagnostics {
            Ok(Ok(value)) => deployment_concurrency(&value, &config.model),
            Ok(Err(error)) => InferenceConcurrency {
                detail: format!(
                    "Optional deployment discovery: {}; using one inference slot",
                    error.code
                ),
                ..Default::default()
            },
            Err(_) => InferenceConcurrency {
                detail: "Deployment discovery timed out; using one inference slot".into(),
                ..Default::default()
            },
        };
        if (inference_concurrency.max_requests.is_none() || context_limit.is_none())
            && let Ok(Ok(snapshot)) = tokio::time::timeout(
                Duration::from_secs(2),
                client.control("/werk/v1/observability"),
            )
            .await
            && let Some(capacity) = observability_concurrency(&snapshot, &config.model)
        {
            if inference_concurrency.max_requests.is_none() {
                inference_concurrency = capacity;
            }
            // An aggregate divided by N is an average, not a safe minimum.
            // Only a single-slot snapshot establishes an individual context size.
            if let Some(limit) = observability_context(&snapshot, &config.model) {
                context_limit = Some(context_limit.map_or(limit, |n| n.min(limit)));
                context_limit_source = "observability_and_host_minimum".into();
            }
        }
        Ok(Self {
            client,
            config,
            inference_concurrency,
            context_limit: std::sync::atomic::AtomicUsize::new(context_limit.unwrap_or(0)),
            context_limit_source,
            runtime_capabilities,
            discovery: json!({"info":info,"models":models}),
        })
    }
}
pub fn wire_message(message: &Message) -> Value {
    let mut value = json!({"role":message.role,"content":message.content});
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
    fn concurrency(&self) -> InferenceConcurrency {
        self.inference_concurrency.clone()
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
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let limit = self.capabilities().context_tokens.ok_or_else(|| WorkerError::new("context_limit_unknown", "Set capabilities.context_tokens to the server's per-request context limit; discovery could not establish it"))?;
        if request.max_output_tokens == 0
            || crate::context::estimate_request(&request)
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
        let mut body = json!({"model":self.config.model,"messages":request.messages.iter().map(wire_message).collect::<Vec<_>>(),"stream":self.config.stream,"max_completion_tokens":request.max_output_tokens});
        if !request.tools.is_empty() {
            body["tools"]=Value::Array(request.tools.iter().map(|t|json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}})).collect());
            body["parallel_tool_calls"] =
                json!(self.config.capabilities.parallel_tools == Support::Supported);
        }
        if self.config.stream {
            body["stream_options"] = json!({"include_usage":true});
        }
        if self.config.capabilities.structured_output == Support::Supported
            && let Some(schema) = request.output_schema
        {
            body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"worker_output","schema":schema,"strict":true}});
        }
        for (key, value) in &self.config.options {
            body[key] = value.clone();
        }
        if body.to_string().len() > 1_048_576 {
            return Err(WorkerError::new("body_limit", "Chat request exceeds 1 MiB"));
        }
        let response = self
            .client
            .send(
                self.client
                    .request(reqwest::Method::POST, "/v1/chat/completions")?
                    .json(&body),
            )
            .await?;
        if !response.status().is_success() {
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
                    return Err(WorkerError::new(
                        "context_overflow",
                        "Server rejected the context; no smaller verified limit was supplied",
                    ));
                }
            }
            return Err(e);
        }
        let client = self.client.clone();
        let max = self.config.max_body_bytes;
        if !self.config.stream {
            let bytes = bounded_body(response, max).await?;
            let v: Value = serde_json::from_str(&client.redact(&String::from_utf8_lossy(&bytes)))?;
            let deltas = decode_completion(&v)?;
            return Ok(Box::pin(futures_util::stream::iter(
                deltas.into_iter().map(Ok),
            )));
        }
        Ok(Box::pin(async_stream::try_stream! {
            let mut bytes=response.bytes_stream();let mut parser=SseDecoder::new(max);
            let mut redactor=StreamRedactor::new(client.secret.as_ref().map(|s|s.as_str().to_owned()));
            while let Some(chunk)=bytes.next().await {
                let chunk=chunk.map_err(|_|WorkerError::transient("interrupted_stream"))?;
                for delta in parser.push(&chunk)? {for clean in redactor.push(delta) {yield clean;}}
            }
            parser.finish()?;
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
pub fn decode_completion(v: &Value) -> Result<Vec<ModelDelta>> {
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
        let mut out = Vec::new();
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
fn observability_concurrency(value: &Value, model: &str) -> Option<InferenceConcurrency> {
    if value["schema_version"] != 1 {
        return None;
    }
    let observed = value["observed_at_ms"].as_u64()?;
    let sampled = value["backend_observed_at_ms"].as_u64()?;
    if observed.abs_diff(sampled) > 10_000 {
        return None;
    }
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
        let total = p["context"]
            .as_u64()
            .filter(|n| *n > 512 && *n <= 2_000_000)? as usize;
        let slots = p["parallel"].as_u64().filter(|n| *n > 0 && *n <= 65536)? as usize;
        let per_request = match p["runtime"].as_str()? {
            "vllm_cuda" => total,
            "llama_cuda" | "llama_cuda_offload" => total / slots,
            _ => return None,
        };
        if per_request <= 512 {
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
        if b["gauges"]["slots_total"].as_f64()? != 1.0 {
            return None;
        }
        let tokens = b["gauges"]["context_capacity_tokens"].as_f64()?;
        if !(513.0..=2_000_000.0).contains(&tokens) || tokens.fract() != 0.0 {
            return None;
        }
        limit = Some(limit.map_or(tokens as usize, |n| n.min(tokens as usize)));
    }
    limit
}
