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
   usable per-request limit; otherwise enter the verified per-slot value. With
   a total allocation of 32,768 shared over two slots, that may be 16,384 rather
   than 32,768: do not infer per-slot context from the CLI flag alone.
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

A 2,048-token server reasoning budget can exhaust the headless CLI's default
2,048 total output tokens. The wizard allows **4096** or another valid reserve
within the effective context. If the model still reaches `model_output_limit`,
reduce reasoning on the server or adjust the reserve; incomplete tool JSON is
never executed. Automatic compaction handles growing input history separately.

Prepared on 2026-10-19. [Fixture-based terminal and configuration experiments](benchmarks/2026-10-19-setup/README.md)
verify the connection workflow. No real server credential or live Qwen inference
was used for those experiments.
