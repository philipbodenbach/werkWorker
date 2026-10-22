//! Emulate chat backends that treat a trailing assistant message as prefill:
//! immediately stopping returns that prefix without a fresh model decision.
use async_trait::async_trait;
use std::sync::Mutex;
use werk_worker::{Result, Role, providers::*};

pub struct PrefillProvider {
    inner: ScriptedProvider,
    requests: Mutex<Vec<ModelRequest>>,
}

impl PrefillProvider {
    pub fn new(turns: Vec<ScriptedTurn>) -> Self {
        Self {
            inner: ScriptedProvider::new(turns),
            requests: Mutex::new(vec![]),
        }
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelProvider for PrefillProvider {
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    fn identity(&self) -> String {
        "assistant-prefill-fixture".into()
    }

    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        self.requests.lock().unwrap().push(request.clone());
        if let Some(tail) = request
            .messages
            .last()
            .filter(|m| m.role == Role::Assistant)
        {
            let deltas = ScriptedTurn::text(&tail.content).deltas;
            return Ok(Box::pin(futures_util::stream::iter(
                deltas.into_iter().map(Ok),
            )));
        }
        self.inner.stream(request).await
    }
}
