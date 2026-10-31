//! Explicit trusted-tool registration. Every invocation goes through the engine executor.
use crate::{
    core::*,
    providers::{RuntimeCapability, WerkClient},
    runtime::*,
    skills::Skill,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ToolContext {
    pub workspace_lease: Option<Arc<crate::parallel::WorkspaceLease>>,
    pub session_id: String,
    pub run_id: String,
    pub invocation_id: String,
    pub workspace: Workspace,
    pub writable: bool,
    pub cancel: CancellationToken,
    pub sandbox: Sandbox,
    pub jobs: Arc<JobManager>,
    pub artifacts: Option<ArtifactStore>,
    pub artifact_refs: Vec<ArtifactRef>,
    pub skills: BTreeMap<String, Skill>,
    pub native_jobs: Vec<String>,
}
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    /// Pure preparation except scoped reads; must never mutate resources.
    async fn prepare(&self, args: Value, _context: &ToolContext) -> Result<Value> {
        Ok(args)
    }
    async fn execute(&self, prepared: Value, context: &ToolContext) -> Result<ToolResult>;
}
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    pub(crate) browsers: Arc<crate::browser_control::BrowserSessions>,
}
impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let def = tool.definition();
        validate_id(&def.name)?;
        validate_schema(&def.input_schema)?;
        if self.tools.contains_key(&def.name) {
            return Err(WorkerError::new("tool_conflict", "Duplicate tool name"));
        }
        self.tools.insert(def.name, tool);
        Ok(())
    }
    pub fn get(&self, name: &str) -> Result<Arc<dyn Tool>> {
        self.tools
            .get(name)
            .cloned()
            .ok_or_else(|| WorkerError::new("unknown_tool", "Tool not registered"))
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }
    pub fn builtins() -> Result<Self> {
        let mut registry = Self::default();
        for name in [
            "files.read",
            "files.list",
            "files.search",
            "files.find",
            "files.read_image",
            "files.write",
            "files.edit",
            "files.insert",
            "process.run",
            "process.start",
            "process.status",
            "process.cancel",
            "process.wait",
            "process.log",
            "browser.check",
            "http.fetch",
            "skills.list",
            "skills.load",
            "interaction.ask",
            "task.plan",
            "artifacts.read",
        ] {
            registry.register(Arc::new(Builtin { name: name.into() }))?;
        }
        for name in crate::agents::AGENT_TOOLS {
            registry.register(Arc::new(crate::agents::AgentTool((*name).into())))?;
        }
        let browsers = registry.browsers.clone();
        crate::browser_control::register(&mut registry, browsers)?;
        Ok(registry)
    }
}
pub fn validate_schema(schema: &Value) -> Result<()> {
    fn refs(v: &Value) -> bool {
        match v {
            Value::Object(o) => o.iter().any(|(k, v)| {
                ((k == "$ref" || k == "$dynamicRef")
                    && v.as_str().is_some_and(|s| !s.starts_with('#')))
                    || refs(v)
            }),
            Value::Array(a) => a.iter().any(refs),
            _ => false,
        }
    }
    if schema.to_string().len() > 65_536 || refs(schema) {
        return Err(WorkerError::new(
            "invalid_schema",
            "Schema too large or contains external references",
        ));
    }
    jsonschema::validator_for(schema)
        .map_err(|_| WorkerError::new("invalid_schema", "Invalid JSON Schema"))?;
    Ok(())
}
pub fn validate_arguments(schema: &Value, args: &Value) -> Result<()> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| WorkerError::new("invalid_schema", "Invalid tool schema"))?;
    if !validator.is_valid(args) {
        return Err(WorkerError::new(
            "validation_error",
            "Arguments do not match the declared JSON Schema",
        ));
    }
    Ok(())
}
fn object(props: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":props,"required":required,"additionalProperties":false})
}
fn string() -> Value {
    json!({"type":"string","maxLength":65536})
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| WorkerError::new("validation_error", format!("Missing string {key}")))
}
struct Builtin {
    name: String,
}
#[async_trait]
impl Tool for Builtin {
    fn definition(&self) -> ToolDefinition {
        let (effect, description, schema) = match self.name.as_str() {
            "files.read" => (
                Effect::Read,
                "Read UTF-8 file text with its hash and line range. Continue with next_start_line, or next_start_byte for a long-line byte page. start_byte cannot be combined with line bounds; byte positions must be UTF-8 boundaries. Do not reread the same truncated range",
                object(
                    json!({"path":string(),"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1},"start_byte":{"type":"integer","minimum":0}}),
                    &["path"],
                ),
            ),
            "files.list" => (
                Effect::Read,
                "List one workspace directory; hidden files and symlinks excluded",
                object(json!({"path":string()}), &["path"]),
            ),
            "files.search" => (
                Effect::Read,
                "Search a workspace file or directory recursively. Literal by default; regex optional. Returns matched line numbers and bounded context. Follow next_offset with identical arguments; narrow path/include if scan_limited. Example: path=src, query=rotate, include=**/*.js. Read the matched range, not the whole file.",
                object(
                    json!({"path":string(),"query":{"type":"string","minLength":1,"maxLength":1024},"recursive":{"type":"boolean"},"regex":{"type":"boolean"},"case_sensitive":{"type":"boolean"},"hidden":{"type":"boolean"},"include":{"type":"string","maxLength":1024},"context_lines":{"type":"integer","minimum":0,"maximum":3},"offset":{"type":"integer","minimum":0,"maximum":10000},"limit":{"type":"integer","minimum":1,"maximum":100}}),
                    &["path", "query"],
                ),
            ),
            "files.find" => (
                Effect::Read,
                "Find workspace files recursively using *, ** and ? globs. Example: path=., pattern=**/*.html. Heavy dependency/build directories and symlinks are excluded. Follow next_offset; narrow path when scan_limited. No shell required.",
                object(
                    json!({"path":string(),"pattern":{"type":"string","maxLength":1024},"hidden":{"type":"boolean"},"offset":{"type":"integer","minimum":0,"maximum":10000},"limit":{"type":"integer","minimum":1,"maximum":100}}),
                    &["path", "pattern"],
                ),
            ),
            "files.read_image" => (
                Effect::Read,
                "View a bounded local PNG/JPEG image or screenshot. Returns normalized image input, dimensions and file hash; do not read binary images as text.",
                object(json!({"path":string()}), &["path"]),
            ),
            "browser.check" => return crate::browser::definition(),
            "files.write" => (
                Effect::Write,
                "Write a workspace path (relative or /workspace/...); parent directory must already exist (use process.run to create directories). Create-only without expected_hash. Replacing an existing file requires its latest observed SHA-256 as expected_hash",
                object(
                    json!({"path":string(),"content":string(),"expected_hash":string()}),
                    &["path", "content"],
                ),
            ),
            "files.edit" => (
                Effect::Write,
                "Replace exactly one occurrence on a SHA-256-bound file base",
                object(
                    json!({"path":string(),"old":string(),"new":string(),"expected_hash":string()}),
                    &["path", "old", "new", "expected_hash"],
                ),
            ),
            "files.insert" => (
                Effect::Write,
                "Insert one complete section after a numbered line of an existing file, guarded by its latest sha256. after_line=0 prepends; after_line=total_lines appends. Include required newline separators in content; none are added implicitly. Reuse the returned sha256 and next_insert_after_line to append the next section. Never recreate an existing file.",
                object(
                    json!({"path":string(),"expected_hash":string(),"after_line":{"type":"integer","minimum":0},"content":string()}),
                    &["path", "expected_hash", "after_line", "content"],
                ),
            ),
            "process.run" => (
                Effect::Process,
                "Sandboxed literal argv, no implicit shell or network. For scripts use argv:[\"python3\",\"-\"] or [\"sh\"] with stdin containing the script; shell syntax otherwise needs [\"sh\",\"-c\",script]. Finish a runnable core, then check behavior. verification.kind must explicitly be behavior for real entry-point assertions; syntax/inspection or omitted kind are diagnostics. Include criteria and relevant paths. A successful corrected check can replace a broken check via verification.replaces=source_invocation; it must cover its paths. Diagnostics do not prove behavior. /workspace persists; /tmp is fresh per call.",
                object(
                    json!({"argv":{"type":"array","items":string(),"minItems":1,"maxItems":128},"stdin":{"type":"string","maxLength":32768,"description":"Literal standard input, closed after delivery. Use for scripts instead of shell here-documents or temporary files."},"cwd":{"type":"string"},"verification":{"type":"object","properties":{"kind":{"type":"string","enum":["behavior","syntax","inspection"]},"replaces":{"type":"string","minLength":1,"maxLength":128},"criteria":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"string","minLength":1,"maxLength":512}},"paths":{"type":"array","minItems":1,"maxItems":64,"uniqueItems":true,"items":string()}},"required":["criteria","paths"],"additionalProperties":false}}),
                    &["argv"],
                ),
            ),
            "process.start" => (
                Effect::Process,
                "Sandboxed argv, no network. /workspace persists; /tmp is fresh each call. Keep cross-call files in /workspace. Shell argv needs explicit approval",
                object(
                    json!({"argv":{"type":"array","items":string(),"minItems":1,"maxItems":128},"cwd":{"type":"string","description":"Relative to workspace; /workspace paths also accepted"},"read_only":{"type":"boolean","description":"Mount workspace read-only; other readers may proceed. Prefer for inspection/watchers. Writers still wait for this job; use process.wait or cancel."}}),
                    &["argv"],
                ),
            ),
            "process.status" | "process.cancel" => (
                Effect::Internal,
                "Inspect or cancel a managed job owned by the current run",
                object(json!({"job_id":string()}), &["job_id"]),
            ),
            "process.wait" => (
                Effect::Internal,
                "Wait for a run-owned background job to exit without spending inference requests on repeated status calls. Returns on exit/cancellation or timeout. Read live output with process.log. Finished output does not itself establish verification.",
                object(
                    json!({"job_id":string(),"timeout_ms":{"type":"integer","minimum":1,"maximum":30000}}),
                    &["job_id"],
                ),
            ),
            "process.log" => (
                Effect::Internal,
                "Read a bounded live stdout/stderr byte page for a run-owned job. Follow next_offset; dropped_before indicates evicted older output. Use process.wait instead of polling unchanged logs.",
                object(
                    json!({"job_id":string(),"stream":{"enum":["stdout","stderr"]},"offset":{"type":"integer","minimum":0},"length":{"type":"integer","minimum":1,"maximum":4096}}),
                    &["job_id"],
                ),
            ),
            "http.fetch" => (
                Effect::NetworkRead,
                "Bounded public HTTP GET/HEAD; pinned DNS and no redirects",
                object(
                    json!({"url":string(),"method":{"enum":["GET","HEAD"]}}),
                    &["url"],
                ),
            ),
            "skills.list" => (
                Effect::Internal,
                "List available immutable skill metadata",
                object(json!({}), &[]),
            ),
            "skills.load" => (
                Effect::Internal,
                "Load one allowed skill as instructions without changing policy",
                object(json!({"name":string()}), &["name"]),
            ),
            "interaction.ask" => (
                Effect::Internal,
                "Ask the host/user for missing information and wait durably",
                object(
                    json!({"question":string(),"choices":{"type":"array","items":string(),"maxItems":12}}),
                    &["question"],
                ),
            ),
            "task.plan" => (
                Effect::Internal,
                "Derive outcomes and acceptance criteria from the task and references. State each criterion, check method and evidence in step descriptions; complete only when satisfied. For coding, build a runnable version, compare actual behavior/content/visuals, then repair gaps. Save checkpoint at phase changes; replace the current plan and checkpoint, not after every read.",
                object(
                    json!({"steps":{"type":"array","maxItems":32,"items":{"type":"object","properties":{"id":string(),"description":string(),"status":{"enum":["pending","in_progress","completed"]}},"required":["id","description","status"],"additionalProperties":false}},
                        "checkpoint":{"type":"object","properties":{
                            "next_action":{"type":"string","maxLength":512},
                            "decisions":{"type":"array","maxItems":6,"items":{"type":"string","maxLength":256}}
                        },"required":["next_action"],"additionalProperties":false}}),
                    &["steps"],
                ),
            ),
            "artifacts.read" => (
                Effect::Internal,
                "Read a bounded portion of a session-owned artifact",
                object(
                    json!({"id":string(),"offset":{"type":"integer","minimum":0},"length":{"type":"integer","minimum":1,"maximum":65536}}),
                    &["id"],
                ),
            ),
            _ => unreachable!(),
        };
        ToolDefinition {
            name: self.name.clone(),
            version: "1".into(),
            description: description.into(),
            input_schema: schema,
            effect,
            timeout_ms: 30_000,
            output_limit: 8192,
            concurrency_safe: matches!(
                effect,
                Effect::Read | Effect::Internal | Effect::NetworkRead
            ),
        }
    }
    async fn prepare(&self, mut args: Value, ctx: &ToolContext) -> Result<Value> {
        if matches!(
            self.name.as_str(),
            "files.read"
                | "files.list"
                | "files.search"
                | "files.write"
                | "files.edit"
                | "files.insert"
                | "files.find"
                | "files.read_image"
        ) {
            args["path"] = json!(crate::policy::workspace_path(field(&args, "path")?)?);
        }
        match self.name.as_str() {
            "browser.check" => return crate::browser::prepare(args, ctx).await,
            "files.insert" => {
                let bytes = ctx.workspace.read(field(&args, "path")?, 1_048_576).await?;
                if hash(&bytes) != field(&args, "expected_hash")? {
                    return Err(WorkerError::new(
                        "write_conflict",
                        "File hash changed before insertion; read the affected section again",
                    ));
                }
                let original = String::from_utf8(bytes)
                    .map_err(|_| WorkerError::new("binary_file", "Insert requires UTF-8 text"))?;
                let line = args["after_line"].as_u64().ok_or_else(|| {
                    WorkerError::new("validation_error", "Missing insertion line")
                })?;
                if line > original.lines().count() as u64 {
                    return Err(WorkerError::new(
                        "write_conflict",
                        "Insertion line is past the end of the observed file",
                    ));
                }
                let offset: usize = original
                    .split_inclusive('\n')
                    .take(line as usize)
                    .map(str::len)
                    .sum();
                let mut content = original.clone();
                let section = field(&args, "content")?;
                content.insert_str(offset, section);
                let next_line = (section.ends_with('\n') || offset == original.len())
                    .then(|| content[..offset + section.len()].lines().count());
                args["next_insert_after_line"] = json!(next_line);
                args["diff"] = json!({"path":args["path"],"before":original,"after":content});
                args["content"] = json!(content);
            }
            "files.edit" => {
                let bytes = ctx.workspace.read(field(&args, "path")?, 1_048_576).await?;
                if hash(&bytes) != field(&args, "expected_hash")? {
                    return Err(WorkerError::new(
                        "write_conflict",
                        "File hash changed before preparation",
                    ));
                }
                let original = String::from_utf8(bytes)
                    .map_err(|_| WorkerError::new("binary_file", "Edit requires UTF-8 text"))?;
                let old = field(&args, "old")?;
                if old.is_empty() || original.matches(old).count() != 1 {
                    return Err(WorkerError::new(
                        "write_conflict",
                        "Edit requires one unique nonempty match",
                    ));
                }
                let content = original.replacen(old, field(&args, "new")?, 1);
                args["content"] = json!(content);
                args["diff"] = json!({"path":args["path"],"before":original,"after":content});
            }
            "files.write" => {
                let original = if args.get("expected_hash").is_some() {
                    let bytes = ctx.workspace.read(field(&args, "path")?, 1_048_576).await?;
                    if hash(&bytes) != field(&args, "expected_hash")? {
                        return Err(WorkerError::new(
                            "write_conflict",
                            "File hash changed before preparation",
                        ));
                    }
                    String::from_utf8(bytes).map_err(|_| {
                        WorkerError::new("binary_file", "Write replacement requires UTF-8")
                    })?
                } else {
                    String::new()
                };
                args["diff"] =
                    json!({"path":args["path"],"before":original,"after":args["content"]});
            }
            "process.run" | "process.start" => {
                if let Some(issue) = crate::process_input::invocation_issue(&args) {
                    return Err(WorkerError::new("invalid_process_invocation", issue));
                }
                if args["stdin"]
                    .as_str()
                    .is_some_and(|input| input.len() > crate::process_input::STDIN_LIMIT)
                {
                    return Err(WorkerError::new(
                        "invalid_stdin",
                        "Standard input exceeds 32768 bytes; use a workspace script for larger programs",
                    ));
                }
                let cwd = process_cwd(args["cwd"].as_str().unwrap_or("."))?;
                args["cwd"] = json!(cwd);
                args["sandbox"] = json!({"backend":"bubblewrap","workspace_writable":ctx.writable,"network":false,"host_home":false,"limits":ctx.sandbox.limits});
            }
            _ => {}
        }
        Ok(args)
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let value=match self.name.as_str(){
            "files.read"=>{
                let path=field(&args,"path")?;let bytes=ctx.workspace.read(path,1_048_576).await?;
                if bytes.contains(&0){return Err(WorkerError::new("binary_file","Binary file is not text"));}
                let text=String::from_utf8(bytes.clone()).map_err(|_|WorkerError::new("encoding_error","File is not UTF-8"))?;
                if let Some(start) = args["start_byte"].as_u64() {
                    if args.get("start_line").is_some() || args.get("end_line").is_some() || start > text.len() as u64 || !text.is_char_boundary(start as usize) {
                        return Err(WorkerError::new("validation_error", "Use a UTF-8 byte boundary within the file, without line bounds"));
                    }
                    return Ok(ToolResult::ok(json!({"path":path,"sha256":hash(bytes),"content":&text[start as usize..],"start_byte":start,"end_byte":text.len(),"total_bytes":text.len(),"mode":"bytes"})));
                }
                let start=args["start_line"].as_u64().unwrap_or(1) as usize;let end=args["end_line"].as_u64().unwrap_or(u64::MAX) as usize;
                if end<start{return Err(WorkerError::new("validation_error","Reversed line range"));}
                let total_lines=text.lines().count();
                let lines:Vec<_>=text.lines().skip(start-1).take(end.saturating_sub(start).saturating_add(1)).collect();
                let start_byte:usize=text.split_inclusive('\n').take(start-1).map(str::len).sum();
                json!({"path":path,"sha256":hash(bytes),"start_line":start,"start_byte":start_byte,"total_bytes":text.len(),
                    "end_line":start-1+lines.len(),"total_lines":total_lines,"content":lines.join("\n")})
            },
            "files.list"=>json!({"entries":ctx.workspace.list(field(&args,"path")?,1000).await?,"hidden":false,"recursive":false}),
            "files.search" | "files.find" => crate::discovery::execute(&self.name, &args, ctx).await?,
            "files.read_image" => {
                let bytes = ctx.workspace.read(field(&args,"path")?,crate::attachments::CLIPBOARD_IMAGE_LIMIT).await?;
                let image = crate::attachments::ImageAttachment::from_bytes(&bytes)?;
                let mut result = ToolResult::ok(json!({"path":args["path"],"sha256":hash(bytes),"width":image.width,"height":image.height}));
                result.images.push(image); return Ok(result);
            },
            "browser.check" => return crate::browser::execute(args, ctx).await,
            "files.write"|"files.edit"|"files.insert"=>{
                let content=field(&args,"content")?;ctx.workspace.write(field(&args,"path")?,content.as_bytes().to_vec(),args["expected_hash"].as_str().map(str::to_owned)).await?;
                // The full diff remains in the prepared action/approval record.
                // Echoing growing files here exhausts context and can truncate
                // away the hash needed for the next incremental edit.
                json!({"path":args["path"],"sha256":hash(content),"bytes_written":content.len(),"total_lines":content.lines().count(),"next_insert_after_line":if self.name=="files.insert"{args["next_insert_after_line"].clone()}else{json!(content.lines().count())}})
            },
            "process.run"|"process.start"=>{
                let argv:Vec<String>=serde_json::from_value(args["argv"].clone())?;let cwd=args["cwd"].as_str().unwrap_or(".");
                if self.name=="process.start"{json!({"job_id":ctx.jobs.start_managed(&ctx.run_id,ProcessJob { sandbox:ctx.sandbox.clone(),workspace:ctx.workspace.clone(),writable:ctx.writable,argv,cwd:cwd.into(),lease:ctx.workspace_lease.clone() }).await?})}
                else{
                    let before = crate::completion::verification_files(&args, &ctx.workspace).await?;
                    let output = if let Some(input) = args["stdin"].as_str() {
                        ctx.sandbox.run_with_stdin(&ctx.workspace,ctx.writable,&argv,cwd,ctx.cancel.clone(),input).await?
                    } else {
                        ctx.sandbox.run(&ctx.workspace,ctx.writable,&argv,cwd,ctx.cancel.clone()).await?
                    };
                    let mut result = serde_json::to_value(&output)?;
                    if output.exit_code != Some(0) || output.timed_out || output.cancelled {
                        result["failure_excerpt"] = json!(crate::completion::diagnostic_excerpt(
                            if output.stderr.is_empty() { &output.stdout } else { &output.stderr }, 1000));
                    }
                    if !before.is_empty() {
                        let after = crate::completion::verification_files(&args, &ctx.workspace).await?;
                        if let Some(reason) = crate::completion::verification_rejection(&args, &result) {
                            result["verification_rejected"] = json!(reason);
                        } else {
                            result["verification"] = json!({"kind":args["verification"]["kind"],"criteria":args["verification"]["criteria"],"files":after,"passed":output.exit_code==Some(0) && !output.timed_out && !output.cancelled && before==after});
                        }
                    }
                    result
                }
            },
            "process.status"|"process.cancel"=>ctx.jobs.status(&ctx.run_id,field(&args,"job_id")?,self.name=="process.cancel").await?,
            "process.wait" => ctx.jobs.wait(&ctx.run_id,field(&args,"job_id")?,args["timeout_ms"].as_u64().unwrap_or(30000),ctx.cancel.clone()).await?,
            "process.log" => ctx.jobs.log(&ctx.run_id,field(&args,"job_id")?,args["stream"].as_str().unwrap_or("stdout"),args["offset"].as_u64().map(|n|n as usize),args["length"].as_u64().unwrap_or(2048) as usize).await?,
            "http.fetch"=>fetch(field(&args,"url")?,args["method"].as_str().unwrap_or("GET"),None).await?,
            "skills.list"=>json!(ctx.skills.values().map(|s|json!({"name":s.name,"description":s.description,"hash":s.hash,"source":s.source})).collect::<Vec<_>>()),
            "skills.load"=>match ctx.skills.get(field(&args,"name")?) { Some(skill) => serde_json::to_value(skill)?, None => json!({"available":false,"name":field(&args,"name")?}) },
            "task.plan"=>args,
            "interaction.ask"=>return Err(WorkerError::new("interaction_required","Interaction must be handled by the engine")),
            "artifacts.read"=>{
                let store=ctx.artifacts.as_ref().ok_or_else(||WorkerError::new("artifact_unavailable","Artifact store is not configured"))?;
                let reference=ctx.artifact_refs.iter().find(|r|Some(r.id.as_str())==args["id"].as_str()).ok_or_else(||WorkerError::new("artifact_denied","Unknown artifact"))?;
                let bytes=store.read(reference,&ctx.session_id,args["offset"].as_u64().unwrap_or(0) as usize,args["length"].as_u64().unwrap_or(8192) as usize).await?;
                json!({"text":String::from_utf8_lossy(&bytes),"bytes":bytes.len()})
            },_=>return Err(WorkerError::new("unknown_tool","Unknown builtin")),
        };
        Ok(ToolResult::ok(value))
    }
}

/// The fixed sandbox mount is a logical workspace path, never a host path.
/// Normalize before approvals so equivalent cwd spellings have one receipt.
fn process_cwd(path: &str) -> Result<&str> {
    crate::policy::workspace_path(path)
}

/// Small example connector. The fixed endpoint and token are host-owned, never
/// input-schema fields. Local HTTP is allowed only when explicitly configured.
pub struct TicketConnector {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    token: Option<String>,
}
impl TicketConnector {
    pub fn new(endpoint: &str, token: Option<String>, allow_loopback: bool) -> Result<Self> {
        let endpoint = reqwest::Url::parse(endpoint)
            .map_err(|_| WorkerError::new("invalid_url", "Invalid ticket endpoint"))?;
        let local = endpoint
            .host_str()
            .is_some_and(|h| h.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback()));
        if !(endpoint.scheme() == "https"
            || (allow_loopback && local && endpoint.scheme() == "http"))
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(WorkerError::new(
                "transport_denied",
                "Connector requires HTTPS or explicit local fake-service opt-in",
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|_| WorkerError::new("http_client", "Cannot build connector"))?;
        Ok(Self {
            client,
            endpoint,
            token,
        })
    }
}
#[async_trait]
impl Tool for TicketConnector {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tickets.create".into(),
            version: "1".into(),
            description:
                "Create a draft ticket in the host-selected example API; not a Jira implementation"
                    .into(),
            input_schema: object(
                json!({"title":string(),"description":string(),"acceptance_criteria":{"type":"array","items":string(),"minItems":1},"source":string()}),
                &["title", "description", "acceptance_criteria", "source"],
            ),
            effect: Effect::ExternalWrite,
            timeout_ms: 20_000,
            output_limit: 8192,
            concurrency_safe: false,
        }
    }
    async fn prepare(&self, mut args: Value, _: &ToolContext) -> Result<Value> {
        args["origin"] = json!(self.endpoint.origin().ascii_serialization());
        Ok(args)
    }
    async fn execute(&self, mut args: Value, _: &ToolContext) -> Result<ToolResult> {
        args.as_object_mut().unwrap().remove("origin");
        let mut req = self.client.post(self.endpoint.clone()).json(&args);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let response = req.send().await.map_err(|_| {
            WorkerError::new(
                "outcome_unknown",
                "Connector write may have reached the service",
            )
        })?;
        if !response.status().is_success() {
            return Err(WorkerError::new(
                "outcome_unknown",
                "Connector write returned no successful receipt",
            ));
        }
        let bytes = bounded_body(response, 65_536).await.map_err(|_| {
            WorkerError::new(
                "outcome_unknown",
                "Connector receipt was not completely received",
            )
        })?;
        let text = String::from_utf8_lossy(&bytes);
        let text = match &self.token {
            Some(t) if !t.is_empty() => text.replace(t, "[redacted]"),
            _ => text.into_owned(),
        };
        let receipt: Value = serde_json::from_str(&text)
            .map_err(|_| WorkerError::new("outcome_unknown", "Invalid connector receipt"))?;
        Ok(ToolResult::ok(json!({"receipt":receipt})))
    }
}

pub struct WerkNativeTool {
    client: WerkClient,
    definition: ToolDefinition,
    native_name: String,
    ready_models: Vec<String>,
}
impl WerkNativeTool {
    pub async fn discover(
        client: WerkClient,
        allowlist: &[String],
        runtime: &[RuntimeCapability],
        experimental: bool,
    ) -> Result<Vec<Arc<dyn Tool>>> {
        let catalog = client.json(reqwest::Method::GET, "/v1/tools", None).await?;
        let capabilities = client
            .json(reqwest::Method::GET, "/v1/capabilities", None)
            .await?;
        if catalog["object"] != "werk.tools" {
            return Err(WorkerError::new(
                "invalid_discovery",
                "Unexpected Werk tool catalog",
            ));
        }
        let mut tools: Vec<Arc<dyn Tool>> = vec![];
        for name in allowlist {
            validate_id(name)?;
            if let Some(cap) = runtime.iter().find(|c| c.operations.contains(name))
                && !cap.status.executable(experimental)
            {
                return Err(WorkerError::new(
                    "unsupported_capability",
                    "Native operation is not executable",
                ));
            }
            let function = catalog["tools"]
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|t| t["function"]["name"].as_str() == Some(name))
                })
                .map(|t| &t["function"])
                .ok_or_else(|| {
                    WorkerError::new(
                        "unknown_native_tool",
                        "Allowlisted function absent from Werk catalog",
                    )
                })?;
            let schema = function["parameters"].clone();
            validate_schema(&schema)?;
            let ready_models = capabilities["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| {
                    m["available_tasks"]
                        .as_array()
                        .is_some_and(|tasks| tasks.contains(&json!(name)))
                })
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            if !matches!(name.as_str(), "get_job" | "cancel_job") && ready_models.is_empty() {
                return Err(WorkerError::new(
                    "unsupported_capability",
                    "No installed model has an executable runtime for the allowlisted native task",
                ));
            }
            tools.push(Arc::new(Self {
                client: client.clone(),
                native_name: name.clone(),
                ready_models,
                definition: ToolDefinition {
                    name: format!("werk.{name}"),
                    version: "1".into(),
                    description: function["description"]
                        .as_str()
                        .unwrap_or("Werk native task")
                        .into(),
                    input_schema: schema,
                    effect: if name == "get_job" {
                        Effect::NetworkRead
                    } else {
                        Effect::ExternalWrite
                    },
                    timeout_ms: 120_000,
                    output_limit: 8192,
                    concurrency_safe: false,
                },
            }));
        }
        Ok(tools)
    }
}
#[async_trait]
impl Tool for WerkNativeTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }
    async fn prepare(&self, args: Value, context: &ToolContext) -> Result<Value> {
        if matches!(self.native_name.as_str(), "get_job" | "cancel_job") {
            let job = args["id"].as_str().unwrap_or("");
            if !context.native_jobs.iter().any(|id| id == job) {
                return Err(WorkerError::new(
                    "job_denied",
                    "Native job does not belong to this session",
                ));
            }
        } else if !args["model"]
            .as_str()
            .is_some_and(|model| self.ready_models.iter().any(|id| id == model))
        {
            return Err(WorkerError::new(
                "unsupported_capability",
                "Model is not ready for this native task",
            ));
        }
        Ok(json!({"origin":self.client.origin(),"arguments":args}))
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let result=self.client.json(reqwest::Method::POST,"/v1/tools/call",Some(json!({"id":ctx.invocation_id,"type":"function","function":{"name":self.native_name,"arguments":args["arguments"].to_string()}}))).await.map_err(|_|WorkerError::new("outcome_unknown","Native task may have been accepted; reconcile its receipt"))?;
        if result["object"] != "werk.tool_result"
            || result["tool_call_id"] != ctx.invocation_id
            || result["name"] != self.native_name
        {
            return Err(WorkerError::new(
                "outcome_unknown",
                "Native receipt correlation mismatch",
            ));
        }
        Ok(ToolResult::ok(result))
    }
}

#[cfg(test)]
mod cwd_tests {
    use super::*;

    #[test]
    fn sandbox_workspace_cwd_is_normalized_without_widening_access() {
        for (input, expected) in [
            (".", "."),
            ("src", "src"),
            ("/workspace", "."),
            ("/workspace/", "."),
            ("/workspace/src", "src"),
        ] {
            assert_eq!(process_cwd(input).unwrap(), expected);
        }
        for input in [
            "/etc",
            "/workspace-other",
            "/workspace//etc",
            "/workspace/../etc",
            "../escape",
            "",
            "/workspace/a\\b",
        ] {
            assert_eq!(process_cwd(input).unwrap_err().code, "path_denied");
        }
    }
}
