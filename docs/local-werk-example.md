# Try the local Qwen agent

This example targets a running Werk server with `qwen3.8-27b-ud-q4_k_xl`,
`--ctx-size 32768` and `--parallel 2`. It is a runnable example, not a recorded
live-model quality evaluation. It contains no credential.

From the werkWorker checkout, in a second terminal:

```sh
cargo run --bin ww -- --workspace .
```

In the setup dialog:

1. Choose **Local Werk server**, URL `http://127.0.0.1:11434`.
2. Choose **Enter API key (session only)** and enter/paste the key already accepted
   by the server. Input is masked. Select **Discover models**.
3. Select `qwen3.8-27b-ud-q4_k_xl`. Keep context on auto if the server reports a
   usable per-request limit; otherwise enter the verified per-slot value.
   `context_per_request_tokens` is the native minimum; an aggregate allocation
   is a different measurement. Do not halve a per-sequence deployment context.
4. Keep output at **4096** (including reasoning), profile `general-readonly`,
   and **Read only; ask each time**. Enable tool calling/streaming for this model.
5. Select **Global connection**, then **Connect and start**.

Tab changes fields, arrows change choices, Ctrl-U clears text, Enter performs
an action. Global settings are restored next time; typed keys must be entered
again. Use an environment-variable reference to reuse credentials. `/setup`
changes the connection after a run has ended. See [full setup details](setup.md).

Enter this task in the chat:

```text
Lies README.md und Cargo.toml. Erkläre die Architektur und nenne drei konkrete
Prüfpunkte mit Dateibelegen. Ändere keine Dateien.
```

Ctrl-Y permits the displayed read once; Ctrl-N denies it. After completion:

```text
Delegiere zwei unabhängige Prüfungen parallel: Ein Subagent prüft docs/context.md
gegen src/context.rs, der andere docs/subagents.md gegen src/agents.rs.
Jeder soll höchstens drei belegte Abweichungen nennen. Sammle beide Ergebnisse.
Keine Dateien ändern.
```

`/agents` shows child status and inference capacity. It may conservatively remain
at one slot when Werk does not expose usable telemetry. There is no promise that
a particular model will follow the delegation request correctly.

## Environment authentication and diagnosis

In zsh, a reusable key reference can be prepared without putting the value in
shell history:

```zsh
read -rs 'WERK_API_KEY?Werk API-Key: '
export WERK_API_KEY
printf '\n'

cargo run --bin ww -- \
  --werk-config examples/werk-qwen27b.json \
  --secret-env WERK_API_KEY diagnose

cargo run --bin ww -- --setup \
  --werk-config examples/werk-qwen27b.json \
  --secret-env WERK_API_KEY --workspace .
```

The example JSON's 32,768-token host context assertion must be reviewed against
the actual per-request server allocation; a smaller discovered limit wins. An
arbitrary new client key does not update a running server. Worker sends the key
as a Bearer credential, outside prompt/tool data. `unset WERK_API_KEY` removes it
from the current shell. For a deliberately unauthenticated local server, select
**No authentication** in the wizard. There is no persistent OS keychain yet.

## Reasoning budget

Worker's **Reasoning effort** setting in `/setup` reads model-specific tiers,
the default and limits from `GET /werk/v1/chat/reasoning`. A saved preference
such as `"options":{"reasoning_tier":"low"}` becomes
`"werk":{"reasoning":{"tier":"low"}}` in chat and count requests. Werk1112
translates this ID to native controls; Worker does not assign token budgets.

For a running llama.cpp worker with budget control, Werk's tiers use 0/512/1024/
2048/4096/8192 reasoning tokens (`none`/`low`/`medium`/`high`/`max`/`ultra`).
The launch budget is a ceiling and output reserves 512 tokens for the answer.
Thus a 2048-token launch budget and 2048 total output yield a `medium` default
(1024 reasoning tokens) when thinking is enabled; 4096 output permits `high`.
An explicitly disabled launch default stays `none`. Native effort backends may
have different vocabularies and report no separate reasoning-token ceiling.
Unknown or cold runtimes report no verified tiers; discovery never loads them.

With native overrides or older servers, a 2,048-token server reasoning budget can exhaust the headless CLI's default
2,048 total output tokens. The wizard allows **4096** or another valid reserve
within the effective context. This is the initial output reservation. When the
server advertises `api.chat.limits`, a truncated generation can retry with twice
the output reservation, bounded by the model/server ceiling, the optional host
`Limits.max_output_tokens`, and actual remaining context. Older servers keep the
initial cap unless the host supplies an explicit ceiling. Recovery does not
change the selected reasoning tier. Incomplete tool calls are never executed.

If the budget cannot grow enough, the next requests use bounded file steps:
2,048 characters per file content/replacement string or process argv item,
validated before execution. Larger tasks must progress through smaller writes
and hash-bound edits. `max_retries` still limits consecutive recovery failures.
Automatic compaction handles growing input history separately. After repeated
output limits, read/list calls do not erase sizing feedback: the next small-write
suggestion shrinks until a confirmed file change. A newly failed generation
always gets a fresh notice, regardless of generic hint cooldowns. An identical
already-truncated request stops with `model_output_recovery_stalled` before more
inference is spent. Existing files remain available for a focused follow-up.

The coding profile requires executable behavioral assertions as well as syntax
checks. For browser code without a browser inside the sandbox, it can exercise
core behavior using DOM/canvas stubs and must disclose that limitation. A passing
syntax check alone is not evidence that the generated application works. Tests
must exercise the actual startup/reset and input handlers, not substitute helper
calls or state assignments that bypass a broken user entry point.

When werkProtocol advertises `api.chat.token_count`, Worker counts the exact
chat template through `/werk/v1/chat/count_tokens` before deciding whether to
compact history. This includes tool schemas and completed tool/result pairs.
Counts are cached only for an identical request. Older servers or explicitly
unsupported counters keep the conservative byte-based fallback; transient
protocol failures receive bounded retries, and invalid/model-mismatched counts
are rejected. Fresh `context_per_request_tokens` telemetry supplies the safe
minimum native slot context, clamped by the host setting. Deployment `context`
is already per sequence and is not divided by parallel slot count.

For streaming chat, `timeout_ms` bounds inactivity between received chunks.
A healthy stream may take longer overall; the run's active-time budget still
applies (15 minutes, 48 steps and 60 model requests by default). Explicit
host budgets remain authoritative. Pending tool approvals pause this clock,
including across restart; model/tool work still consumes it. Non-streaming requests and control calls keep
their total HTTP timeout. This lets slower local models finish code generation
without repeatedly losing an active response after two minutes.

Streaming display events are grouped at most 100 ms apart (first output is
immediate), avoiding a full SQLite commit for each token. Completed responses,
tool intents, approvals and receipts remain durable before execution continues.

Run the actual one-prompt coding acceptance against an existing configured
workspace with:

```sh
cargo build --locked --bin ww
python scripts/tests/live_tetris.py --project /path/to/configured/workspace
```

The runner creates a separate temporary workspace and uses the configured model
and token budgets. It grants file/process access only in that test workspace,
does not answer clarification questions, and reports the artifact directory.
It uses `--secret-env NAME` when supplied, otherwise the existing local
`.werkworker/secrets.json`. The runner requires Node.js and checks completion,
the single HTML artifact and inline JavaScript syntax; the generated game still
needs a browser acceptance check.

Live validation on 2026-10-20 used the local Qwen model with 32,768 tokens per
request and a 4,096-token output reserve. One run completed in 533 seconds;
another recovered automatically from a real output-limit failure and continued
without user input, but reached the 900-second run limit while debugging the
generated game's top-out logic. Independent Chromium checks passed startup,
keyboard movement/rotation/drop, pause/resume, animated grain, optional music
and four-line scoring; game-over on a blocked stack still failed. This is
evidence for recovery and those behaviors, **not a fully passing coding
acceptance**. No generated game was manually repaired for these checks.

Prepared on 2026-10-19. [Fixture-based terminal and configuration experiments](benchmarks/2026-10-19-setup/README.md)
verify the connection workflow. No real server credential or live Qwen inference
was used for those experiments.
