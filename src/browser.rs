//! Local, one-shot browser verification. No ambient browser or host network.
use crate::{core::*, tools::ToolContext};
use serde_json::{Value, json};
use std::path::PathBuf;

pub(crate) fn runtime_path() -> Option<PathBuf> {
    let root = std::env::var_os("WERK_WORKER_BROWSER_RUNTIME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
                .map(|p| p.join("werk-worker/tools/browser"))
        })?;
    (root.join("python/playwright").is_dir() && root.join("browsers").is_dir()).then_some(root)
}
pub(crate) fn definition() -> ToolDefinition {
    let text = json!({"type":"string","maxLength":1024});
    let action = |name: &str, props: Value, fields: &[&str]| {
        let mut props = props;
        props["type"] = json!({"const":name});
        let mut required = vec!["type"];
        required.extend_from_slice(fields);
        json!({"type":"object","properties":props,"required":required,"additionalProperties":false})
    };
    ToolDefinition { name:"browser.check".into(), version:"1".into(), effect:Effect::Process,
        description:"Test a local HTML entry point in isolated Chromium with a read-only workspace. Supply ordered real clicks/keys and assertions; returns JS errors, assertion results and screenshot. Example actions: press ArrowLeft, expect expression='document.querySelector(\"#score\").textContent !== \"0\"'. A load/screenshot alone is NOT verification. Include verification criteria and all relevant source/test paths to record evidence. External network unavailable; runtime must be installed by host.".into(),
        input_schema:json!({"type":"object","properties":{
            "path":{"type":"string","maxLength":1024},"screenshot":{"type":"boolean"},
            "actions":{"type":"array","maxItems":16,"items":{"oneOf":[
                action("click",json!({"selector":text}), &["selector"]), action("press",json!({"key":text}), &["key"]),
                action("fill",json!({"selector":text,"text":text}), &["selector","text"]),
                action("wait",json!({"ms":{"type":"integer","minimum":0,"maximum":2000}}), &["ms"]),
                action("expect_visible",json!({"selector":text}), &["selector"]), action("expect_text",json!({"selector":text,"text":text}), &["selector","text"]),
                action("expect",json!({"expression":text}), &["expression"])
            ]}},
            "verification":{"type":"object","properties":{"kind":{"type":"string","enum":["behavior"]},"replaces":{"type":"string","minLength":1,"maxLength":128},"criteria":{"type":"array","minItems":1,"maxItems":16,"items":text},"paths":{"type":"array","minItems":1,"maxItems":64,"items":text}},"required":["criteria","paths"],"additionalProperties":false}
        },"required":["path","actions"],"additionalProperties":false}), timeout_ms:60000, output_limit:8192, concurrency_safe:true }
}
pub(crate) async fn prepare(mut args: Value, ctx: &ToolContext) -> Result<Value> {
    args["path"] = json!(crate::policy::workspace_path(
        args["path"].as_str().unwrap_or("")
    )?);
    let entry = args["path"].as_str().unwrap().to_owned();
    if let Some(paths) = args
        .get_mut("verification")
        .and_then(|v| v.get_mut("paths"))
        .and_then(Value::as_array_mut)
        && !paths.iter().any(|p| {
            p.as_str()
                .is_some_and(|p| crate::policy::workspace_path(p).is_ok_and(|p| p == entry))
        })
    {
        if paths.len() >= 64 {
            return Err(WorkerError::new(
                "invalid_verification",
                "Include the HTML entry in verification.paths (at most 64 paths)",
            ));
        }
        paths.push(json!(entry));
    }
    let bytes = ctx
        .workspace
        .read(args["path"].as_str().unwrap(), 1_048_576)
        .await?;
    args["entry_sha256"] = json!(hash(bytes));
    // Bind review/approval to every declared verification input, as well as entry.
    args["observed_files"] =
        serde_json::to_value(crate::completion::verification_files(&args, &ctx.workspace).await?)?;
    if args["actions"].to_string().len() > 6000 {
        return Err(WorkerError::new(
            "browser_step_too_large",
            "Use at most 6000 bytes of complete browser actions per check",
        ));
    }
    Ok(args)
}
pub(crate) async fn execute(args: Value, ctx: &ToolContext) -> Result<ToolResult> {
    if runtime_path().is_none() {
        return Err(WorkerError::new(
            "browser_unavailable",
            "Install scripts/install-browser.sh, then restart WW. Browser checks cannot be replaced by a claimed success.",
        ));
    }
    let path = args["path"].as_str().unwrap();
    if json!(hash(ctx.workspace.read(path, 1_048_576).await?)) != args["entry_sha256"] {
        return Err(WorkerError::new(
            "write_conflict",
            "Browser entry changed since preparation; retry with current files",
        ));
    }
    let before = crate::completion::verification_files(&args, &ctx.workspace).await?;
    if serde_json::to_value(&before)? != args["observed_files"] {
        return Err(WorkerError::new(
            "write_conflict",
            "Browser verification inputs changed since preparation",
        ));
    }
    let input = json!({"path":path,"actions":args["actions"],"screenshot":args["screenshot"].as_bool().unwrap_or(true)});
    let argv = vec![
        "/usr/bin/python3".into(),
        "-c".into(),
        include_str!("../scripts/browser_check.py").into(),
        input.to_string(),
    ];
    let out = ctx
        .sandbox
        .run_browser(&ctx.workspace, &argv, ctx.cancel.clone())
        .await?;
    if out.exit_code != Some(0) || out.timed_out || out.cancelled {
        return Err(WorkerError::new(
            "browser_failed",
            format!(
                "Browser did not finish: exit={:?}, timeout/resource_limit={}, cancelled={}. {}",
                out.exit_code,
                out.timed_out,
                out.cancelled,
                crate::completion::excerpt(&out.stderr, 1500)
            ),
        ));
    }
    let mut data: Value = serde_json::from_str(&out.stdout)?;
    let encoded = data
        .as_object_mut()
        .and_then(|o| o.remove("screenshot_base64"))
        .and_then(|v| v.as_str().map(str::to_owned));
    let after = crate::completion::verification_files(&args, &ctx.workspace).await?;
    let entry_unchanged = ctx
        .workspace
        .read(path, 1_048_576)
        .await
        .is_ok_and(|bytes| json!(hash(bytes)) == args["entry_sha256"]);
    let passed = data["passed"] == true && before == after && entry_unchanged;
    data["passed"] = json!(passed);
    if !passed && !out.stderr.is_empty() {
        data["runtime_stderr"] = json!(crate::completion::excerpt(&out.stderr, 2000));
    }
    data["exit_code"] = json!(if passed { 0 } else { 1 });
    data["path"] = json!(path);
    if !before.is_empty() {
        data["verification"] =
            json!({"criteria":args["verification"]["criteria"],"files":after,"passed":passed});
    }
    let mut result = ToolResult::ok(data);
    if let Some(encoded) = encoded {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| WorkerError::new("invalid_image", "Invalid browser screenshot"))?;
        let image = crate::attachments::ImageAttachment::from_bytes(&bytes)?;
        result.data["screenshot"] = json!({"width":image.width,"height":image.height});
        if let Some(store) = &ctx.artifacts {
            result
                .artifacts
                .push(store.put(&ctx.session_id, &ctx.run_id, bytes).await?);
        }
        result.images.push(image);
    }
    Ok(result)
}
