use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder},
    policy::*,
    profiles::AgentProfile,
    providers::*,
    *,
};

/// The engine must enforce a recovery schema even when a model ignores it.
/// An unopened browser ID proves this fails before browser preparation/execution.
#[tokio::test]
async fn browser_recovery_schema_is_persisted_and_enforced_before_execution() {
    struct Recovery;
    #[async_trait::async_trait]
    impl ContextBuilder for Recovery {
        async fn build(
            &self,
            session: &SessionData,
            run: &Run,
            mut tools: Vec<ToolDefinition>,
        ) -> Result<(ModelRequest, ContextSnapshot)> {
            let action = tools
                .iter_mut()
                .find(|t| t.name == "browser.action")
                .unwrap();
            action.input_schema["properties"]["action"] =
                json!({"type":"string","enum":["click","hover"]});
            action.input_schema["properties"]["target"] = json!({"type":"object","properties":{"ref":{"enum":["alternative"]}},"required":["ref"],"additionalProperties":false});
            action.input_schema["required"] = json!(["browser_id", "action", "target"]);
            action.input_schema["x-werk-browser-strategy"] = json!("inspect");
            DefaultContextBuilder.build(session, run, tools).await
        }
    }
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: "bad-coordinate".into(),
                name: "browser.action".into(),
                arguments: json!({"browser_id":"unopened","action":"click_at","x":400,"y":650}),
            }],
        ),
        ScriptedTurn::text(
            "The attempted coordinate action was rejected; no browser change occurred.",
        ),
    ]));
    let engine = WorkerEngine::builder(provider)
        .context(Arc::new(Recovery))
        .build()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut policy = HostPolicy::default();
    policy
        .tool_permissions
        .insert("browser.action".into(), ToolPermission::Allow);
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "fixture".into(),
                prompt: "Inspect the UI".into(),
                tools: vec!["browser.action".into()],
                skills: vec![],
            },
            dir.path(),
            policy,
            Value::Null,
        )
        .await
        .unwrap();
    let handle = session
        .submit(
            "Inspect an alternative operation",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(handle.wait().await.unwrap().status, RunStatus::Completed);
    let saved: SessionData =
        serde_json::from_slice(&serde_json::to_vec(&session.snapshot().await.unwrap()).unwrap())
            .unwrap();
    let invocation = &saved.runs[0].invocations[0];
    let result = invocation.result.as_ref().unwrap();
    assert_eq!(result.status, Outcome::ValidationError);
    assert_eq!(
        result.error_code.as_deref(),
        Some("browser_strategy_recovery")
    );
    assert_eq!(result.data["executed"], false);
    assert!(invocation.request_input_schema.is_some());
    assert!(invocation.prepared.is_null());
    engine.close().await.unwrap();
}
