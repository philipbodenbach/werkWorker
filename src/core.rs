use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub type Result<T> = std::result::Result<T, WorkerError>;

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct WorkerError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
}
impl WorkerError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            retry_after_ms: None,
        }
    }
    pub fn transient(code: &str) -> Self {
        Self {
            retryable: true,
            ..Self::new(code, "Provider attempt failed")
        }
    }
}
impl From<std::io::Error> for WorkerError {
    fn from(e: std::io::Error) -> Self {
        Self::new("io_error", format!("I/O operation failed ({:?})", e.kind()))
    }
}
impl From<serde_json::Error> for WorkerError {
    fn from(_: serde_json::Error) -> Self {
        Self::new("invalid_json", "Invalid JSON data")
    }
}
pub fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4())
}
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn hash(bytes: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
pub fn fingerprint(value: &impl Serialize) -> String {
    hash(serde_json::to_vec(value).expect("serializable contract"))
}
pub fn validate_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
    {
        return Err(WorkerError::new(
            "invalid_id",
            "Identifiers must be 1..160 safe ASCII characters",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}
impl Message {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn tool(call_id: &str, result: &ToolResult) -> Self {
        Self {
            role: Role::Tool,
            content: serde_json::to_string(result).expect("tool result"),
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    WaitingApproval,
    WaitingInput,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}
impl RunStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_steps: u32,
    pub max_model_requests: u32,
    pub max_tool_invocations: u32,
    pub max_wall_ms: u64,
    pub max_output_bytes: usize,
    pub context_tokens: usize,
    pub reserve_output_tokens: usize,
    pub max_retries: u32,
    pub max_repeated_calls: usize,
    /// One optional budgeted model summary of old assistant/tool groups.
    pub model_compaction: bool,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 24,
            max_model_requests: 30,
            max_tool_invocations: 64,
            max_wall_ms: 300_000,
            max_output_bytes: 65_536,
            context_tokens: 32_768,
            reserve_output_tokens: 2048,
            max_retries: 2,
            max_repeated_calls: 3,
            model_compaction: false,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        if self.max_steps == 0
            || self.max_steps > 1000
            || self.max_model_requests == 0
            || self.max_model_requests > 2000
            || self.max_tool_invocations > 4096
            || self.max_wall_ms == 0
            || self.max_wall_ms > 86_400_000
            || !(256..=1_048_576).contains(&self.max_output_bytes)
            || self.context_tokens <= self.reserve_output_tokens + 512
            || self.context_tokens > 2_000_000
            || self.max_retries > 8
        {
            return Err(WorkerError::new(
                "invalid_limits",
                "Run limits are outside supported bounds",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Internal,
    Read,
    Write,
    Process,
    NetworkRead,
    ExternalWrite,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub version: String,
    pub description: String,
    pub input_schema: Value,
    pub effect: Effect,
    pub timeout_ms: u64,
    pub output_limit: usize,
    pub concurrency_safe: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Denied,
    ValidationError,
    CancelledBeforeStart,
    OutcomeUnknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub bytes: usize,
    pub sha256: String,
    pub expires_at_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub status: Outcome,
    pub data: Value,
    pub error_code: Option<String>,
    pub artifacts: Vec<ArtifactRef>,
    pub truncated: bool,
}
impl ToolResult {
    pub fn ok(data: Value) -> Self {
        Self {
            status: Outcome::Succeeded,
            data,
            error_code: None,
            artifacts: vec![],
            truncated: false,
        }
    }
    pub fn error(status: Outcome, code: &str) -> Self {
        Self {
            status,
            data: Value::Null,
            error_code: Some(code.into()),
            artifacts: vec![],
            truncated: false,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InvocationStatus {
    Prepared,
    AwaitingApproval,
    Running,
    Finished,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub invocation_id: String,
    pub step_id: String,
    pub message_index: usize,
    pub call: ToolCall,
    pub definition: Option<ToolDefinition>,
    pub status: InvocationStatus,
    pub prepared: Value,
    pub result: Option<ToolResult>,
    pub approval: Option<ApprovalRequest>,
    pub interaction: Option<InteractionRequest>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub session_id: String,
    pub run_id: String,
    pub invocation_id: String,
    pub arguments_hash: String,
    pub description: Value,
    pub expires_at_ms: u64,
    pub decision: Option<ApprovalDecision>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    Deny,
    AllowSameArguments,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionRequest {
    pub request_id: String,
    pub question: String,
    pub choices: Vec<String>,
    pub answer: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Metrics {
    pub steps: u32,
    pub model_requests: u32,
    pub tool_invocations: u32,
    pub first_text_ms: Option<u64>,
    pub first_tool_ms: Option<u64>,
    pub approval_wait_ms: u64,
    pub total_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub estimated_input_tokens: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub id: String,
    pub hash: String,
    pub estimated_tokens: usize,
    pub message_count: usize,
    pub omitted_messages: Vec<usize>,
    pub summary: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub id: String,
    pub run_id: String,
    pub attempt_id: String,
    pub message_indices: Vec<usize>,
    pub source_hash: String,
    pub summary: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAttempt {
    pub attempt_id: String,
    pub step_id: String,
    pub started_at_ms: u64,
    pub completed: bool,
    pub error: Option<WorkerError>,
    pub partial_text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub status: RunStatus,
    pub input: String,
    pub limits: Limits,
    pub output_schema: Option<Value>,
    pub started_at_ms: u64,
    pub deadline_ms: u64,
    pub history_start: usize,
    pub final_text: Option<String>,
    pub error: Option<WorkerError>,
    pub metrics: Metrics,
    pub attempts: Vec<ModelAttempt>,
    pub contexts: Vec<ContextSnapshot>,
    pub invocations: Vec<ToolInvocation>,
    pub steering: Vec<String>,
    pub plan: Value,
    pub similar_grants: Vec<String>,
    pub repaired_output: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub session_id: String,
    pub run_id: String,
    pub status: RunStatus,
    pub text: Option<String>,
    pub error: Option<WorkerError>,
    pub metrics: Metrics,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Accepted,
    Duplicate,
    Rejected,
    AlreadyTerminal,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub command_id: String,
    pub status: ReceiptStatus,
    pub run_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCommand {
    pub payload_hash: String,
    pub receipt: CommandReceipt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    SessionCreated,
    SessionResumed,
    SessionForked {
        parent: String,
    },
    CommandAccepted {
        command_id: String,
    },
    RunState {
        status: RunStatus,
    },
    StepStarted {
        step_id: String,
    },
    StepCompleted {
        step_id: String,
    },
    ModelRequest {
        attempt_id: String,
        step_id: String,
        context_hash: String,
    },
    TextDelta {
        attempt_id: String,
        text: String,
    },
    ToolDelta {
        attempt_id: String,
        index: usize,
        arguments: String,
    },
    ModelCompleted {
        attempt_id: String,
        usage: Option<Usage>,
        metadata: Value,
    },
    AttemptFailed {
        attempt_id: String,
        code: String,
    },
    ToolPrepared {
        invocation_id: String,
        name: String,
    },
    ToolStarted {
        invocation_id: String,
    },
    ToolCompleted {
        invocation_id: String,
        result: ToolResult,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
    },
    InteractionRequested {
        request: InteractionRequest,
    },
    InteractionAnswered {
        request_id: String,
    },
    SkillLoaded {
        name: String,
        hash: String,
        source: String,
    },
    ContextPlanned {
        snapshot: ContextSnapshot,
    },
    PlanUpdated {
        plan: Value,
    },
    Diagnostic {
        code: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub cursor: u64,
    pub timestamp_ms: u64,
    pub event: EventKind,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub schema_version: u32,
    pub id: String,
    pub revision: u64,
    pub workspace: PathBuf,
    pub workspace_identity: String,
    pub sandbox_id: String,
    pub principal: String,
    pub profile: crate::profiles::AgentProfile,
    pub profile_hash: String,
    pub policy: crate::policy::HostPolicy,
    pub metadata: Value,
    pub messages: Vec<Message>,
    pub runs: Vec<Run>,
    pub commands: BTreeMap<String, StoredCommand>,
    pub events: Vec<WorkerEvent>,
    pub skill_snapshots: BTreeMap<String, crate::skills::Skill>,
    pub compactions: Vec<CompactionRecord>,
}
impl SessionData {
    pub fn emit(&mut self, run_id: Option<&str>, event: EventKind) {
        self.events.push(WorkerEvent {
            schema_version: 1,
            event_id: id("evt"),
            session_id: self.id.clone(),
            run_id: run_id.map(str::to_owned),
            cursor: self.events.len() as u64 + 1,
            timestamp_ms: now_ms(),
            event,
        });
    }
    pub fn run(&self, run_id: &str) -> Result<&Run> {
        self.runs
            .iter()
            .find(|r| r.id == run_id)
            .ok_or_else(|| WorkerError::new("not_found", "Run not found"))
    }
    pub fn run_mut(&mut self, run_id: &str) -> Result<&mut Run> {
        self.runs
            .iter_mut()
            .find(|r| r.id == run_id)
            .ok_or_else(|| WorkerError::new("not_found", "Run not found"))
    }
    pub fn command(
        &mut self,
        command_id: &str,
        payload: &Value,
        run_id: Option<&str>,
    ) -> Result<CommandReceipt> {
        validate_id(command_id)?;
        let payload_hash = fingerprint(payload);
        if let Some(previous) = self.commands.get(command_id) {
            if previous.payload_hash != payload_hash {
                return Err(WorkerError::new(
                    "command_conflict",
                    "Command ID was used with a different payload",
                ));
            }
            return Ok(CommandReceipt {
                status: ReceiptStatus::Duplicate,
                ..previous.receipt.clone()
            });
        }
        if self.commands.len() >= 10_000 {
            return Err(WorkerError::new(
                "session_limit",
                "Command capacity reached; fork a new session",
            ));
        }
        let receipt = CommandReceipt {
            command_id: command_id.into(),
            status: ReceiptStatus::Accepted,
            run_id: run_id.map(str::to_owned),
        };
        self.commands.insert(
            command_id.into(),
            StoredCommand {
                payload_hash,
                receipt: receipt.clone(),
            },
        );
        self.emit(
            run_id,
            EventKind::CommandAccepted {
                command_id: command_id.into(),
            },
        );
        Ok(receipt)
    }
}
