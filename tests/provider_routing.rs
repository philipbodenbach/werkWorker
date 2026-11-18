mod support;

use futures_util::StreamExt;
use serde_json::json;
use std::sync::Arc;
use support::{Response, Server, werk};
use werk_worker::{
    Result, WorkerEngine, policy::HostPolicy, profiles::AgentProfile, providers::*,
    sessions::SqliteStore,
};

struct Credentials;
impl SecretResolver for Credentials {
    fn resolve(&self, reference: &str) -> Result<Option<String>> {
        Ok((reference == "VENDOR_KEY").then(|| "vendor-test-key".into()))
    }
}

fn candidate(name: &str, origin: &str, transport: ProviderTransport) -> ProviderRoute {
    let mut config = WerkConfig::local("fixture-model");
    config.base_url = origin.into();
    config.stream = false;
    config.capabilities.context_tokens = Some(32768);
    config.capabilities.tool_calling = Support::Supported;
    ProviderRoute {
        name: name.into(),
        transport,
        config,
        max_disclosure: Disclosure::WorkspaceContext,
        secret_env: None,
    }
}
fn policy(candidates: Vec<ProviderRoute>) -> ProviderRouting {
    ProviderRouting {
        version: 1,
        prefer_loopback: false,
        required_capabilities: vec![InferenceCapability::ToolCalling],
        candidates,
    }
}
async fn vendor() -> Server {
    Server::raw(|r| match r.path.as_str() {
        "/v1/models" => Response::json(json!({"object":"list","data":[{"id":"fixture-model"}]})),
        "/v1/chat/completions" => Response::json(json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]})),
        _ => failure(404),
    })
    .await
}
fn failure(status: u16) -> Response {
    Response {
        status,
        headers: vec![],
        body: vec![],
    }
}

#[tokio::test]
async fn only_eligible_preflight_falls_back_and_uses_the_targets_credential() {
    let unavailable = Server::raw(|_| failure(503)).await;
    let blocked = vendor().await;
    let remote = vendor().await;
    let mut denied = candidate(
        "private-denied",
        &blocked.origin,
        ProviderTransport::OpenAiCompatible,
    );
    denied.max_disclosure = Disclosure::MetadataOnly;
    let mut fallback = candidate(
        "authorized",
        &remote.origin,
        ProviderTransport::OpenAiCompatible,
    );
    fallback.secret_env = Some("VENDOR_KEY".into());
    let primary = candidate(
        "unavailable",
        &unavailable.origin,
        ProviderTransport::OpenAiCompatible,
    );
    let provider = policy(vec![primary, denied, fallback])
        .connect(&Credentials)
        .await
        .unwrap();
    assert_eq!(provider.discovery["routing"]["selected"], "authorized");
    assert_eq!(
        provider.discovery["routing"]["disclosure"],
        "workspace_context"
    );
    assert_eq!(
        provider.discovery["routing"]["attempts"][0]["status"],
        "provider_unavailable"
    );
    assert!(blocked.requests.lock().unwrap().is_empty());
    assert!(
        !unavailable.requests.lock().unwrap()[0]
            .raw
            .contains("vendor-test-key")
    );
    let stream = provider
        .stream(ModelRequest {
            messages: vec![werk_worker::Message::text(
                werk_worker::Role::User,
                "authorized-private-marker",
            )],
            tools: vec![],
            reasoning_tier: None,
            max_output_tokens: 32,
            output_schema: None,
        })
        .await
        .unwrap();
    assert!(
        stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .all(|event| event.is_ok())
    );
    let requests = remote.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|r| r.raw.contains("Bearer vendor-test-key"))
    );
    assert!(requests[1].raw.contains("authorized-private-marker"));
    assert!(blocked.requests.lock().unwrap().is_empty());
    assert!(
        !unavailable.requests.lock().unwrap()[0]
            .raw
            .contains("authorized-private-marker")
    );
}

#[tokio::test]
async fn auth_and_invalid_protocol_fail_without_trying_another_candidate() {
    for status in [401, 200] {
        let primary = Server::raw(move |_| {
            if status == 401 {
                failure(status)
            } else {
                Response::json(json!({"invalid":"wire envelope"}))
            }
        })
        .await;
        let fallback = vendor().await;
        let error = policy(vec![
            candidate("werk", &primary.origin, ProviderTransport::Werk),
            candidate(
                "vendor",
                &fallback.origin,
                ProviderTransport::OpenAiCompatible,
            ),
        ])
        .connect(&Credentials)
        .await
        .err()
        .unwrap();
        assert_eq!(
            error.code,
            if status == 401 {
                "auth_error"
            } else {
                "protocol_mismatch"
            }
        );
        assert!(fallback.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn capability_check_order_and_first_hop_preference_precede_disclosure() {
    let first = vendor().await;
    let second = Server::start(werk).await;
    let mut lacking = candidate(
        "missing-tools",
        &first.origin,
        ProviderTransport::OpenAiCompatible,
    );
    lacking.config.capabilities.tool_calling = Support::Unknown;
    let selected = candidate("werk", &second.origin, ProviderTransport::Werk);
    let mut nonloopback = candidate(
        "remote",
        "https://routing-fixture.invalid",
        ProviderTransport::OpenAiCompatible,
    );
    // This candidate cannot make a request, even if ordering regresses.
    nonloopback.secret_env = Some("MISSING_KEY".into());
    let mut routes = policy(vec![nonloopback.clone(), lacking.clone(), selected.clone()]);
    routes.prefer_loopback = true;
    let provider = routes.connect(&Credentials).await.unwrap();
    assert_eq!(
        provider.discovery["routing"]["attempts"],
        json!([
            {"candidate":"missing-tools","status":"unsupported_capability"},{"candidate":"werk","status":"selected"}
        ])
    );
    assert!(first.requests.lock().unwrap().is_empty());
    let error = policy(vec![nonloopback, lacking, selected])
        .connect(&Credentials)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "auth_error");
}

#[tokio::test]
async fn unknown_inventory_is_not_a_capability_and_only_discovery_can_switch_origins() {
    let missing = Server::raw(|_| Response::json(json!({"data":[{"id":"another"}]}))).await;
    let terminal = Server::start(|r| {
        if r.path == "/werk/v1/chat/completions" {
            failure(401)
        } else {
            werk(r)
        }
    })
    .await;
    let other = vendor().await;
    let provider = policy(vec![
        candidate(
            "absent",
            &missing.origin,
            ProviderTransport::OpenAiCompatible,
        ),
        candidate("pinned", &terminal.origin, ProviderTransport::Werk),
        candidate("last", &other.origin, ProviderTransport::OpenAiCompatible),
    ])
    .connect(&Credentials)
    .await
    .unwrap();
    let error = provider
        .stream(ModelRequest {
            messages: vec![werk_worker::Message::text(
                werk_worker::Role::User,
                "private-workspace-marker",
            )],
            reasoning_tier: None,
            tools: vec![],
            max_output_tokens: 32,
            output_schema: None,
        })
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "auth_error");
    assert_eq!(
        provider
            .for_model("untargeted-model")
            .await
            .err()
            .unwrap()
            .code,
        "provider_disclosure_denied"
    );
    assert!(other.requests.lock().unwrap().is_empty());
    assert!(
        missing
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| !r.raw.contains("private-workspace-marker"))
    );
    assert!(terminal.requests.lock().unwrap().iter().any(|r| r.path
        == "/werk/v1/chat/completions"
        && r.raw.contains("private-workspace-marker")));
}

#[tokio::test]
async fn route_pins_survive_reopen_and_cannot_be_removed_with_legacy_mode() {
    let first = vendor().await;
    let second = vendor().await;
    let selected = candidate(
        "selected",
        &first.origin,
        ProviderTransport::OpenAiCompatible,
    );
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let store = || async {
        Arc::new(
            SqliteStore::open(state.path().join("sessions.sqlite"))
                .await
                .unwrap(),
        )
    };
    let provider = policy(vec![selected.clone()])
        .connect(&Credentials)
        .await
        .unwrap();
    let engine = WorkerEngine::builder(Arc::new(provider))
        .store(store().await)
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile::bundled("general-readonly").unwrap(),
            workspace.path(),
            HostPolicy::default(),
            json!({}),
        )
        .await
        .unwrap();
    let id = session.id().await;
    assert!(
        session
            .snapshot()
            .await
            .unwrap()
            .inference_disclosure_binding
            .is_some()
    );
    engine.close().await.unwrap();
    let same = policy(vec![selected.clone()])
        .connect(&Credentials)
        .await
        .unwrap();
    let engine = WorkerEngine::builder(Arc::new(same))
        .store(store().await)
        .build()
        .await
        .unwrap();
    engine.open_session(&id).await.unwrap();
    engine.close().await.unwrap();
    let different = policy(vec![candidate(
        "selected",
        &second.origin,
        ProviderTransport::OpenAiCompatible,
    )])
    .connect(&Credentials)
    .await
    .unwrap();
    let mut other_account = selected.clone();
    other_account.secret_env = Some("VENDOR_KEY".into());
    let other_account = policy(vec![other_account])
        .connect(&Credentials)
        .await
        .unwrap();
    let legacy = WerkProvider::connect_openai_compatible(selected.config, None)
        .await
        .unwrap();
    for provider in [different, other_account, legacy] {
        let engine = WorkerEngine::builder(Arc::new(provider))
            .store(store().await)
            .build()
            .await
            .unwrap();
        assert_eq!(
            engine.open_session(&id).await.err().unwrap().code,
            "provider_disclosure_denied"
        );
        engine.close().await.unwrap();
    }
    assert!(
        second
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.path == "/v1/models")
    );
}

#[tokio::test]
async fn cli_selects_only_host_policy_and_rejects_unversioned_or_ambiguous_routes() {
    let server = vendor().await;
    let dir = tempfile::tempdir().unwrap();
    let route = candidate(
        "vendor",
        &server.origin,
        ProviderTransport::OpenAiCompatible,
    );
    let mut policy = serde_json::to_value(policy(vec![route])).unwrap();
    let cli = env!("CARGO_BIN_EXE_ww");
    let run = |policy: &serde_json::Value| {
        let mut cmd = tokio::process::Command::new(cli);
        cmd.env("TEST_TRUSTED_ROUTES", policy.to_string())
            .current_dir(dir.path())
            .args(["--provider-routing-env", "TEST_TRUSTED_ROUTES", "diagnose"]);
        cmd
    };
    let success = run(&policy).output().await.unwrap();
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );
    let diagnosis: serde_json::Value = serde_json::from_slice(&success.stdout).unwrap();
    assert_eq!(diagnosis["discovery"]["routing"]["selected"], "vendor");
    let before = server.requests.lock().unwrap().len();
    policy["version"] = json!(2);
    assert!(!run(&policy).output().await.unwrap().status.success());
    policy["version"] = json!(1);
    policy["candidates"][0]["max_disclosure"] = json!("metadata_only");
    assert!(!run(&policy).output().await.unwrap().status.success());
    policy["candidates"][0]["ignore_disclosure"] = json!(true);
    assert!(!run(&policy).output().await.unwrap().status.success());
    assert_eq!(before, server.requests.lock().unwrap().len());
}

#[test]
fn routing_schema_matches_serialized_offers_and_rejects_invalid_policies() {
    let schema =
        serde_json::from_str(include_str!("../docs/provider-routing.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let mut offer = serde_json::to_value(policy(vec![
        candidate("native", "https://service.example", ProviderTransport::Werk),
        candidate(
            "vendor",
            "https://vendor.example",
            ProviderTransport::OpenAiCompatible,
        ),
    ]))
    .unwrap();
    offer["candidates"][1]["max_disclosure"] = json!("metadata_only");
    assert!(validator.is_valid(&offer));
    assert!(serde_json::from_value::<ProviderRouting>(offer.clone()).is_ok());
    for (key, invalid) in [
        ("max_disclosure", json!(null)),
        ("transport", json!("auto")),
        ("extra", json!(true)),
    ] {
        let mut bad = offer.clone();
        bad["candidates"][0][key] = invalid;
        assert!(!validator.is_valid(&bad));
        assert!(serde_json::from_value::<ProviderRouting>(bad).is_err());
    }
    for (key, value) in [("version", json!(2)), ("candidates", json!([]))] {
        let mut bad = offer.clone();
        bad[key] = value;
        assert!(!validator.is_valid(&bad));
    }
    offer["candidates"][1]["config"]["deployment"] = json!("werk-only");
    assert!(!validator.is_valid(&offer));
}

#[tokio::test]
async fn explicit_http_sessions_bind_the_recipient_and_legacy_history_stays_local() {
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("history.sqlite");
    let engine_for = |provider: Arc<dyn ModelProvider>| async {
        WorkerEngine::builder(provider)
            .store(Arc::new(SqliteStore::open(&path).await.unwrap()))
            .build()
            .await
            .unwrap()
    };
    let profile = || AgentProfile::bundled("general-readonly").unwrap();
    let legacy = engine_for(Arc::new(ScriptedProvider::default())).await;
    let legacy_id = legacy
        .create_session(
            profile(),
            workspace.path(),
            HostPolicy::default(),
            json!({}),
        )
        .await
        .unwrap()
        .id()
        .await;
    legacy.close().await.unwrap();
    for transport in [ProviderTransport::Werk, ProviderTransport::OpenAiCompatible] {
        let server = match transport {
            ProviderTransport::Werk => Server::start(werk).await,
            ProviderTransport::OpenAiCompatible => vendor().await,
        };
        let other_server = match transport {
            ProviderTransport::Werk => Server::start(werk).await,
            ProviderTransport::OpenAiCompatible => vendor().await,
        };
        let config = candidate("session", &server.origin, transport).config;
        let original = WerkProvider::connect_explicit(
            transport,
            config.clone(),
            Some("first-key".into()),
            Some("ACCOUNT"),
        )
        .await
        .unwrap();
        let binding = original.disclosure_binding().unwrap();
        let engine = engine_for(Arc::new(original)).await;
        assert_eq!(
            engine.open_session(&legacy_id).await.err().unwrap().code,
            "provider_disclosure_denied"
        );
        let session = engine
            .create_session(
                profile(),
                workspace.path(),
                HostPolicy::default(),
                json!({}),
            )
            .await
            .unwrap();
        let id = session.id().await;
        engine.close().await.unwrap();
        let rotated = WerkProvider::connect_explicit(
            transport,
            config.clone(),
            Some("rotated-key".into()),
            Some("ACCOUNT"),
        )
        .await
        .unwrap();
        let engine = engine_for(Arc::new(rotated)).await;
        engine.open_session(&id).await.unwrap();
        engine.close().await.unwrap();
        let mut elsewhere = config.clone();
        elsewhere.base_url = other_server.origin.clone();
        let mut changed_model = config.clone();
        changed_model.model = "other-model".into();
        let mut changes = vec![
            (config.clone(), "OTHER_ACCOUNT"),
            (elsewhere, "ACCOUNT"),
            (changed_model, "ACCOUNT"),
        ];
        if transport == ProviderTransport::Werk {
            let mut changed_deployment = config.clone();
            changed_deployment.deployment = Some("other-deployment".into());
            changes.push((changed_deployment, "ACCOUNT"));
        }
        for (config, reference) in changes {
            let changed = WerkProvider::connect_explicit(
                transport,
                config,
                Some("test-key".into()),
                Some(reference),
            )
            .await
            .unwrap();
            let engine = engine_for(Arc::new(changed)).await;
            assert_eq!(
                engine.open_session(&id).await.err().unwrap().code,
                "provider_disclosure_denied"
            );
            engine.close().await.unwrap();
        }
        let anonymous = WerkProvider::connect_explicit(transport, config.clone(), None, None)
            .await
            .unwrap();
        assert_ne!(
            anonymous.disclosure_binding().as_deref(),
            Some(binding.as_str())
        );
        let opaque =
            WerkProvider::connect_explicit(transport, config.clone(), Some("opaque".into()), None)
                .await
                .unwrap();
        let new_connection =
            WerkProvider::connect_explicit(transport, config, Some("opaque".into()), None)
                .await
                .unwrap();
        assert_ne!(
            opaque.disclosure_binding(),
            new_connection.disclosure_binding()
        );
        for endpoint in [&server, &other_server] {
            assert!(
                endpoint
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| request.method == "GET")
            );
        }
        let snapshot = tokio::process::Command::new(env!("CARGO_BIN_EXE_ww"))
            .arg("--store")
            .arg(&path)
            .args(["sessions", "inspect", &id])
            .output()
            .await
            .unwrap();
        assert!(snapshot.status.success());
        let stored: serde_json::Value = serde_json::from_slice(&snapshot.stdout).unwrap();
        assert_eq!(stored["id"], id);
        assert_eq!(stored["inference_disclosure_binding"], binding);
    }
}
