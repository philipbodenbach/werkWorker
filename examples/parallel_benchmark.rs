//! Reproducible scheduler benchmark; simulated latency, no LLM throughput claim.
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use werk_worker::{
    parallel::ExecutionConfig, policy::HostPolicy, profiles::AgentProfile, providers::*, *,
};
struct Delayed {
    slots: Option<usize>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}
struct Guard(Arc<AtomicUsize>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl ModelProvider for Delayed {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn identity(&self) -> String {
        "simulated-100ms".into()
    }
    fn concurrency(&self) -> InferenceConcurrency {
        InferenceConcurrency {
            max_requests: self.slots,
            source: "benchmark_fixture".into(),
            detail: "100 ms simulated response; no network or model".into(),
        }
    }
    async fn stream(&self, _: ModelRequest) -> Result<ModelStream> {
        let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(n, Ordering::SeqCst);
        let guard = Guard(self.active.clone());
        Ok(Box::pin(
            async_stream::stream! {let _guard=guard;tokio::time::sleep(Duration::from_millis(100)).await;for delta in ScriptedTurn::text("completed").deltas {yield Ok(delta);}},
        ))
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let mut rows = vec![];
    for reported in [None, Some(1), Some(2), Some(4)] {
        for repetition in 1..=5 {
            eprintln!("case slots={reported:?} repetition={repetition}: setup");
            let provider = Arc::new(Delayed {
                slots: reported,
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
            });
            let engine = WorkerEngine::builder(provider.clone())
                .execution(ExecutionConfig {
                    max_model_requests: 4,
                    ..Default::default()
                })
                .build()
                .await?;
            let workspace = tempfile::tempdir()?;
            let profile = AgentProfile {
                schema_version: 1,
                name: "benchmark".into(),
                prompt: "Complete the assigned independent task.".into(),
                tools: vec![],
                skills: vec![],
            };
            let mut sessions = vec![];
            for _ in 0..8 {
                sessions.push(
                    engine
                        .create_session(
                            profile.clone(),
                            workspace.path(),
                            HostPolicy::default(),
                            Value::Null,
                        )
                        .await?,
                );
            }
            eprintln!("case slots={reported:?} repetition={repetition}: submit");
            let started = Instant::now();
            let mut runs = vec![];
            for session in sessions {
                runs.push(
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        session.submit("task", "submit", Limits::default(), None),
                    )
                    .await
                    .expect("benchmark submission stalled")?,
                );
            }
            eprintln!("case slots={reported:?} repetition={repetition}: wait");
            for run in runs {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(10), run.wait())
                        .await
                        .expect("benchmark run stalled")?
                        .status,
                    RunStatus::Completed
                );
            }
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            rows.push(json!({"reported_slots":reported,"effective_slots":engine.execution_status()["effective_model_requests"],"repetition":repetition,"tasks":8,"simulated_response_ms":100,"elapsed_ms":elapsed,"peak_inflight":provider.peak.load(Ordering::SeqCst)}));
            eprintln!("case slots={reported:?} repetition={repetition}: close");
            tokio::time::timeout(Duration::from_secs(10), engine.close())
                .await
                .expect("benchmark close stalled")?;
            eprintln!("case slots={reported:?} repetition={repetition}: done");
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"kind":"simulated_scheduler_benchmark","rows":rows}))?
    );
    Ok(())
}
