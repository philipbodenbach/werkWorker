//! Worker control protocol v1. Stdout is exclusively JSONL.
use crate::{
    api::{SessionHandle, WorkerEngine},
    core::*,
    policy::HostPolicy,
    profiles::AgentProfile,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u32 = 1;
pub const METHODS: &[&str] = &[
    "initialize",
    "execution.status",
    "agents.delegate",
    "agents.list",
    "agents.cancel",
    "agents.message",
    "session.create",
    "session.open",
    "session.list",
    "session.rename",
    "session.delete",
    "session.fork",
    "run.submit",
    "run.status",
    "run.cancel",
    "run.steer",
    "run.resume",
    "session.followup",
    "approval.respond",
    "interaction.respond",
    "events.subscribe",
    "session.export",
    "shutdown",
];
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}
/// Authority is selected by the trusted host at startup, never by model output.
pub struct RpcHost {
    pub engine: WorkerEngine,
    pub workspace: PathBuf,
    pub policy: HostPolicy,
    sessions: BTreeMap<String, SessionHandle>,
    initialized: bool,
}
impl RpcHost {
    pub fn new(engine: WorkerEngine, workspace: PathBuf, policy: HostPolicy) -> Self {
        Self {
            engine,
            workspace,
            policy,
            sessions: BTreeMap::new(),
            initialized: false,
        }
    }
    async fn session(&mut self, id: &str) -> Result<SessionHandle> {
        if let Some(s) = self.sessions.get(id) {
            return Ok(s.clone());
        }
        let s = self.engine.open_session(id).await?;
        self.sessions.insert(id.into(), s.clone());
        Ok(s)
    }
    pub async fn dispatch(&mut self, request: &RpcRequest) -> Result<Value> {
        let p = &request.params;
        if request.method == "initialize" {
            if p["protocol_version"] != PROTOCOL_VERSION {
                return Err(WorkerError::new(
                    "protocol_mismatch",
                    "Worker protocol version 1 required",
                ));
            }
            self.initialized = true;
            return Ok(
                json!({"protocol_version":PROTOCOL_VERSION,"methods":METHODS,"max_input_bytes":1_048_576,"event_replay":true}),
            );
        }
        if !self.initialized {
            return Err(WorkerError::new("not_initialized", "Send initialize first"));
        }
        match request.method.as_str() {
            "execution.status" => Ok(self.engine.execution_status()),
            "session.create" => {
                let profile =
                    AgentProfile::bundled(p["profile"].as_str().unwrap_or("general-readonly"))?;
                let s = self
                    .engine
                    .create_session(
                        profile,
                        &self.workspace,
                        self.policy.clone(),
                        p["metadata"].clone(),
                    )
                    .await?;
                let id = s.id().await;
                self.sessions.insert(id.clone(), s);
                Ok(json!({"session_id":id}))
            }
            "session.list" => Ok(json!(self.engine.list_sessions().await?)),
            "session.rename" => {
                self.engine
                    .rename_session(text(p, "session_id")?, text(p, "title")?)
                    .await?;
                Ok(json!({"renamed":true}))
            }
            "session.delete" => {
                if p["confirm"] != true {
                    return Err(WorkerError::new(
                        "confirmation_required",
                        "session.delete requires confirm: true",
                    ));
                }
                let deleted = self.engine.delete_session(text(p, "session_id")?).await?;
                self.sessions.retain(|id, _| !deleted.contains(id));
                Ok(json!({"deleted":deleted}))
            }
            "shutdown" => {
                self.engine.close().await?;
                Ok(json!({"closed":true}))
            }
            method => {
                let session = self.session(text(p, "session_id")?).await?;
                match method {
                    "session.open" => Ok(json!(session.snapshot().await?)),
                    "session.export" => session.export().await,
                    "session.fork" => {
                        let fork = session.fork(self.policy.clone()).await?;
                        let id = fork.id().await;
                        self.sessions.insert(id.clone(), fork);
                        Ok(json!({"session_id":id}))
                    }
                    "run.submit" | "session.followup" => {
                        let limits = if p.get("limits").is_some() {
                            serde_json::from_value(p["limits"].clone())?
                        } else {
                            Limits::default()
                        };
                        let run = if method == "run.submit" {
                            session
                                .submit_with_images(
                                    text(p, "input")?,
                                    serde_json::from_value(
                                        p.get("images").cloned().unwrap_or_else(|| json!([])),
                                    )?,
                                    text(p, "command_id")?,
                                    limits,
                                    p.get("output_schema").cloned(),
                                )
                                .await?
                        } else {
                            session
                                .follow_up_with_images(
                                    text(p, "input")?,
                                    serde_json::from_value(
                                        p.get("images").cloned().unwrap_or_else(|| json!([])),
                                    )?,
                                    text(p, "command_id")?,
                                    limits,
                                )
                                .await?
                        };
                        Ok(json!({"run_id":run.id,"receipt":run.receipt}))
                    }
                    "run.resume" => {
                        let run = self
                            .engine
                            .resume_run(text(p, "session_id")?, text(p, "run_id")?)
                            .await?;
                        Ok(json!({"run_id":run.id}))
                    }
                    "events.subscribe" => Ok(
                        json!({"subscribed":true,"after_cursor":p["after_cursor"].as_u64().unwrap_or(0)}),
                    ),
                    _ => {
                        let run = session.run(text(p, "run_id")?).await?;
                        match method {
                            "agents.delegate" => Ok(json!(
                                run.delegate(
                                    serde_json::from_value(p["tasks"].clone())?,
                                    text(p, "command_id")?
                                )
                                .await?
                            )),
                            "agents.list" => Ok(json!(run.subagents().await?)),
                            "agents.cancel" => Ok(json!(
                                run.subagent(text(p, "child_session_id")?)
                                    .await?
                                    .cancel("Host cancelled child", text(p, "command_id")?)
                                    .await?
                            )),
                            "agents.message" => Ok(json!(
                                run.subagent(text(p, "child_session_id")?)
                                    .await?
                                    .steer(text(p, "input")?, text(p, "command_id")?)
                                    .await?
                            )),
                            "run.status" => Ok(json!(run.status().await?)),
                            "run.cancel" => Ok(json!(
                                run.cancel(
                                    p["reason"].as_str().unwrap_or("host cancellation"),
                                    text(p, "command_id")?
                                )
                                .await?
                            )),
                            "run.steer" => Ok(json!(
                                run.steer_with_images(
                                    text(p, "input")?,
                                    serde_json::from_value(
                                        p.get("images").cloned().unwrap_or_else(|| json!([]))
                                    )?,
                                    text(p, "command_id")?
                                )
                                .await?
                            )),
                            "approval.respond" => Ok(json!(
                                run.respond_to_approval(
                                    text(p, "request_id")?,
                                    serde_json::from_value(p["decision"].clone())?,
                                    text(p, "command_id")?
                                )
                                .await?
                            )),
                            "interaction.respond" => Ok(json!(
                                run.respond_to_interaction(
                                    text(p, "request_id")?,
                                    text(p, "answer")?,
                                    text(p, "command_id")?
                                )
                                .await?
                            )),
                            _ => Err(WorkerError::new(
                                "unknown_method",
                                "Unknown Worker RPC method",
                            )),
                        }
                    }
                }
            }
        }
    }
    pub async fn serve<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin + Send + 'static>(
        &mut self,
        mut reader: R,
        mut writer: W,
    ) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(128);
        let mut output = tokio::spawn(async move {
            while let Some(value) = rx.recv().await {
                let mut line = serde_json::to_vec(&value)?;
                line.push(b'\n');
                writer.write_all(&line).await?;
                writer.flush().await?;
            }
            Ok::<_, WorkerError>(())
        });
        let mut subscriptions: Vec<tokio::task::JoinHandle<()>> = vec![];
        loop {
            let line = bounded_line(&mut reader, 1_048_576).await;
            let line = match line {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    if !matches!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            tx.send(json!({"id":null,"error":error}))
                        )
                        .await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                    continue;
                }
            };
            let request = serde_json::from_slice::<RpcRequest>(&line);
            let request = match request {
                Ok(request) => request,
                Err(_) => {
                    if !matches!(tokio::time::timeout(std::time::Duration::from_secs(5), tx.send(json!({"id":null,"error":{"code":"invalid_request","message":"Invalid JSONL request"}}))).await, Ok(Ok(()))){break;}
                    continue;
                }
            };
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(30), self.dispatch(&request))
                    .await
                    .unwrap_or_else(|_| {
                        Err(WorkerError::new(
                            "command_timeout",
                            "Control operation timed out; inspect state before retry",
                        ))
                    });
            let successful = result.is_ok();
            let response = match result {
                Ok(result) => json!({"id":request.id,"result":result}),
                Err(error) => json!({"id":request.id,"error":error}),
            };
            if !matches!(
                tokio::time::timeout(std::time::Duration::from_secs(5), tx.send(response)).await,
                Ok(Ok(()))
            ) {
                break;
            }
            if successful && request.method == "events.subscribe" {
                if subscriptions.len() >= 16 {
                    let _=tokio::time::timeout(std::time::Duration::from_secs(5), tx.send(json!({"method":"resync_required","params":{"reason":"subscription_limit"}}))).await;
                    continue;
                }
                let session = self.session(text(&request.params, "session_id")?).await?;
                let mut stream =
                    session.events(request.params["after_cursor"].as_u64().unwrap_or(0));
                let events = tx.clone();
                subscriptions.push(tokio::spawn(async move{while let Some(event)=stream.next().await{
                    let value=match event{Ok(event)=>json!({"method":"event","params":event}),Err(error)=>json!({"method":"subscription_error","params":error})};
                    if events.try_send(value).is_err(){let _=tokio::time::timeout(std::time::Duration::from_secs(2),events.send(json!({"method":"resync_required","params":{"reason":"slow_subscriber"}}))).await;break;}
                }}));
            }
            if successful && request.method == "shutdown" {
                break;
            }
        }
        for task in subscriptions {
            task.abort();
            let _ = task.await;
        }
        let closed = self.engine.close().await;
        drop(tx);
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut output).await {
            Ok(result) => {
                result.map_err(|_| WorkerError::new("transport_error", "Stdout task failed"))??
            }
            Err(_) => {
                output.abort();
                let _ = output.await;
                return Err(WorkerError::new(
                    "transport_timeout",
                    "Stdout consumer stalled",
                ));
            }
        }
        closed?;
        Ok(())
    }
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| WorkerError::new("invalid_params", format!("Missing {key}")))
}
async fn bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut exceeded = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            if exceeded {
                return Err(WorkerError::new("input_limit", "RPC line too large"));
            }
            return Ok(if line.is_empty() { None } else { Some(line) });
        }
        let n = buf
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(buf.len());
        let complete = buf[n - 1] == b'\n';
        if line.len() + n > limit {
            exceeded = true;
        }
        if !exceeded {
            line.extend_from_slice(&buf[..n]);
        }
        reader.consume(n);
        if complete {
            if exceeded {
                return Err(WorkerError::new("input_limit", "RPC line too large"));
            }
            return Ok(Some(line));
        }
    }
}
