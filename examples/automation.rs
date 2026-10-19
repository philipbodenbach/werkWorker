//! Same SDK/loop as coding; no Git or process tools. Optional example API POST.
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{
    policy::*,
    profiles::AgentProfile,
    providers::*,
    tools::{TicketConnector, ToolRegistry},
    *,
};

#[tokio::main]
async fn main() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("meeting.md"),
        include_str!("fixtures/meeting.md"),
    )
    .await?;
    let drafts = json!([{"title":"Retry failed report","description":"Offer retry preserving the original report inputs.","acceptance_criteria":["Retry preserves all inputs"],"source":"meeting.md:3"},{"title":"Export failed report IDs","description":"Export failed report identifiers for operations.","acceptance_criteria":["Export includes IDs and timestamps only"],"source":"meeting.md:4-5"}]);
    let call = |name: &str, arguments: Value| ToolCall {
        id: id("call"),
        name: name.into(),
        arguments,
    };
    let mut turns = vec![
        ScriptedTurn::calls(
            "Load notes and drafting guidance.",
            vec![
                call("files.read", json!({"path":"meeting.md"})),
                call("skills.load", json!({"name":"ticket-drafting"})),
            ],
        ),
        ScriptedTurn::calls(
            "Prepare local previews.",
            vec![call(
                "files.write",
                json!({"path":"drafts.json","content":serde_json::to_string_pretty(&drafts)?}),
            )],
        ),
    ];
    let mut registry = ToolRegistry::builtins()?;
    let mut profile = AgentProfile::bundled("automation-basic")?;
    let mut grants = vec![
        CapabilityGrant {
            tool: "files.read".into(),
            version: "1".into(),
            resource: ".".into(),
            arguments_hash: None,
            expires_at_ms: None,
            preapproved: true,
        },
        CapabilityGrant {
            tool: "files.write".into(),
            version: "1".into(),
            resource: ".".into(),
            arguments_hash: None,
            expires_at_ms: None,
            preapproved: false,
        },
    ];
    // Host opt-in: no endpoint means previews only. This is a generic example
    // ticket API, not a claim of Jira support. No production key is required.
    if let Ok(endpoint) = std::env::var("WERK_WORKER_EXAMPLE_TICKET_ENDPOINT") {
        let origin = reqwest::Url::parse(&endpoint)
            .map_err(|_| WorkerError::new("invalid_url", "Invalid example endpoint"))?
            .origin()
            .ascii_serialization();
        registry.register(Arc::new(TicketConnector::new(&endpoint, None, true)?))?;
        profile.tools.push("tickets.create".into());
        grants.push(CapabilityGrant {
            tool: "tickets.create".into(),
            version: "1".into(),
            resource: origin,
            arguments_hash: None,
            expires_at_ms: None,
            preapproved: false,
        });
        turns.push(ScriptedTurn::calls(
            "Submit approved draft.",
            vec![call("tickets.create", drafts[0].clone())],
        ));
    }
    turns.push(ScriptedTurn::text(&serde_json::to_string(&drafts)?));
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(turns)))
        .tools(registry)
        .build()
        .await?;
    let session = engine
        .create_session(
            profile,
            workspace.path(),
            HostPolicy {
                grants,
                workspace_writable: true,
                ..Default::default()
            },
            Value::Null,
        )
        .await?;
    let run=session.submit("Draft two tickets from the approved meeting notes","automation-demo",Limits::default(),Some(json!({"type":"array","minItems":2,"items":{"type":"object","required":["title","description","acceptance_criteria","source"]}}))).await?;
    let mut events = run.events(0);
    while let Some(event) = events.next().await {
        let event = event?;
        println!("{}", serde_json::to_string(&event)?);
        if let EventKind::ApprovalRequested { request } = event.event {
            // Local preview is an explicit demo-host decision. Real external writes
            // require a concrete terminal confirmation even in this example.
            let external = request.description["effect"] == "external_write";
            let decision = if external {
                eprintln!("Create this ticket? {} [y/N]", request.description);
                let answer = tokio::task::spawn_blocking(|| {
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s).map(|_| s)
                })
                .await
                .map_err(|_| WorkerError::new("io_error", "Input worker failed"))??;
                if answer.trim() == "y" {
                    ApprovalDecision::AllowOnce
                } else {
                    ApprovalDecision::Deny
                }
            } else {
                ApprovalDecision::AllowOnce
            };
            run.respond_to_approval(&request.request_id, decision, &id("demo-approval"))
                .await?;
        }
    }
    let result = run.wait().await?;
    assert_eq!(result.status, RunStatus::Completed);
    println!(
        "{}",
        tokio::fs::read_to_string(workspace.path().join("drafts.json")).await?
    );
    engine.close().await
}
