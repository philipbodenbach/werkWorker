//! Opt-in, offline comparison of prompt preparation on the same saved history.
//! No credentials, model calls or writes to the session database are used.
//! Use WW_REPLAY_SNAPSHOT, or WW_REPLAY_DATABASE plus WW_REPLAY_SESSION.
//! Capture hashes before a change with WW_PROMPT_CAPTURE; compare afterwards
//! with WW_PROMPT_BASELINE on the same frozen input. Timing includes request
//! hashing, uses the active Cargo build profile, and is not an inference or
//! end-to-end latency measurement. No timing threshold is asserted.
use std::{collections::BTreeMap, time::Instant};
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder},
    *,
};

#[tokio::test]
#[ignore = "set WW_REPLAY_SNAPSHOT or WW_REPLAY_DATABASE/WW_REPLAY_SESSION; optional WW_PROMPT_BASELINE/WW_PROMPT_CAPTURE"]
async fn saved_prompt_preparation_replay() {
    let data: String = if let Ok(path) = std::env::var("WW_REPLAY_SNAPSHOT") {
        std::fs::read_to_string(path).unwrap()
    } else {
        let connection = rusqlite::Connection::open_with_flags(
            std::env::var("WW_REPLAY_DATABASE").expect("WW_REPLAY_DATABASE"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        connection
            .query_row(
                "SELECT data FROM sessions WHERE id = ?1",
                [std::env::var("WW_REPLAY_SESSION").expect("WW_REPLAY_SESSION")],
                |row| row.get(0),
            )
            .unwrap()
    };
    let saved: SessionData = serde_json::from_str(&data).unwrap();
    let mut hashes = BTreeMap::new();
    for mode in ["classic", "minimal", "adaptive"] {
        let mut session = saved.clone();
        let run = session.runs.last_mut().unwrap();
        run.contexts.clear();
        // Widen the planning budget. Very large snapshots can still compact;
        // no provider is invoked, and the canonical stored limits stay untouched.
        run.limits.context_tokens = 1_000_000;
        let run = run.clone();
        session.metadata["_werkworker_harness"][&run.id]["config"]["mode"] = mode.into();
        let start = Instant::now();
        let mut expected = None;
        for _ in 0..5 {
            let (request, _) = DefaultContextBuilder
                .build(&session, &run, vec![])
                .await
                .unwrap();
            let hash = fingerprint(&request);
            if let Some(expected) = &expected {
                assert_eq!(&hash, expected, "request changed during {mode} replay");
            }
            expected = Some(hash);
        }
        eprintln!(
            "prompt replay mode={mode}, invocations={}, mean_ms={:.3}",
            run.invocations.len(),
            start.elapsed().as_secs_f64() * 200.0
        );
        hashes.insert(mode, expected.unwrap());
    }
    if let Ok(path) = std::env::var("WW_PROMPT_BASELINE") {
        let baseline: BTreeMap<String, String> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&hashes).unwrap(),
            serde_json::to_value(baseline).unwrap()
        );
    }
    if let Ok(path) = std::env::var("WW_PROMPT_CAPTURE") {
        // Store hashes only: the replay must not duplicate private transcript data.
        std::fs::write(path, serde_json::to_vec_pretty(&hashes).unwrap()).unwrap();
    }
}
