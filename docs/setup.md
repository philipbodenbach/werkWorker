# Interactive connection setup

Start `ww`, or `cargo run --bin ww --` from this checkout. The setup uses the
same colors and banner as the chat. On first use, no API request is made until
you choose **Discover models**. On subsequent starts, **Continue previous**
checks and reconnects to the saved service, while **Connect new** opens the wizard.
No inference is made until you submit a chat message or explicitly resume an
interrupted run.

Connecting checks WerkProtocol identity, runtime capabilities and `/v1/models`.
Runtime capabilities and the model list are fetched concurrently. The worker
does not request the general `/v1/capabilities` catalogue or `/v1/parameters`
during chat setup: the catalogue probes installed models/backends and can be
slow on a cold server. Explicit media tools discover their own task support.
Bounded deployment/observability checks still establish context and concurrency.

1. Choose a local Werk server, a remote Werk server, or the explicit offline demo.
2. Enter the server origin (local default `http://127.0.0.1:11434`). Remote
   servers require HTTPS. URL credentials, queries and non-root paths are rejected.
3. Choose authentication: masked API-key input for this session, the **name** of
   an environment variable, or explicitly no authentication. Paste is supported.
   Use a key already accepted by Werk; this does not create a server-side key.
4. Discover models. The client checks WerkProtocol 1.0 and lists `/v1/models`.
   Authentication/network failures stay in the dialog so you can correct them.
5. Select the model, context limit, output budget, agent profile and workspace
   access. Keep context empty for discovery, or supply the actual **per-request**
   server limit. Changing models clears a manually entered context limit.
   Output defaults to 4,096 tokens **including reasoning** and must fit within
   one quarter of the effective context. The server's smaller discovered limit
   always wins. Unknown context requires an explicit value before continuing.
6. Select streaming and tool support for your model. Profiles exposing tools
   require tool calling. These capability choices are host assertions, not a
   benchmark proving that a model can use tools correctly.
7. Choose where to save, then **Connect and start**. The dialog checks the selected
   model's capacity again. Save errors remain visible; choosing **Do not save**
   allows a session without changing configuration files.

CUDA, CPU, Metal and other inference backends are controlled by the running
Werk server. The dialog reports Werk's active backend; it does not install,
launch or reconfigure an inference server and is not a generic OpenAI adapter.

## Controls and reconnecting

Tab/Shift-Tab or Up/Down selects a field; Left/Right changes a choice. Enter moves
to the next field or performs the selected action. Ctrl-U clears a text field,
Backspace removes its last character. Esc goes back or cancels a connection
check. Ctrl-Q exits; two consecutive Ctrl-C presses also exit. The first Ctrl-C
cancels an outstanding check. Each check has a 20-second overall timeout, and
exiting aborts its task and restores the terminal.

`/setup` reopens configuration from the chat after the current run has finished
or been cancelled. It closes the old engine and starts a **new session**; it does
not silently change the provider of an existing run. The earlier session remains
in SQLite. An initial CLI prompt is not resubmitted when reconnecting.
`ww --setup` explicitly opens the wizard even when configuration flags are given.
With a saved connection, ordinary startup instead offers **Continue previous**
and **Connect new**. Continue checks the live server and its current context
capacity with a cancellable 20-second timeout. Failure leaves the choices visible
for retry or new setup; it does not erase previous chats or silently use a demo.
It reuses the effective model and capability choices from the successful setup,
even when the original project transport file contains incomplete defaults.
After connecting, choose a new or previous session. `/sessions` returns to that
selection without reconnecting.

## Global configuration and credentials

| Platform | Global connection file |
|---|---|
| Linux/macOS | `$XDG_CONFIG_HOME/werkworker/config.json`, or `~/.config/werkworker/config.json` |
| Windows | `%APPDATA%\werkworker\config.json` |

The global file is versioned JSON containing `version`, `werk` (the existing
WerkConfig object), `secret_env`, `no_auth`, `profile` and `limits`. The wizard
reloads it on the next launch. **Global connection** saves URL, model,
capabilities, streaming, profile, token budgets and authentication method.
A named environment reference is saved; the variable's value is not.
The file is written using a temporary file and replacement; Unix mode is 0600.
Windows replacement removes the old file first and is not atomic.

Successful saved setups also remember the effective connection, budgets, profile
and explicitly selected access in `connections/<workspace-hash>.json` beside the
global config. These files are scoped to the canonical workspace path and written
with mode 0600 on Unix. They contain credential references, never API-key values.
This also remembers connections when the wizard saves project preferences only.
**Do not save** leaves the remembered connection unchanged.

Directly entered keys exist only in the process for the connection/session.
They are masked on screen and are absent from settings, prompts, tool arguments
and session exports. You must enter them again after reconnect/restart unless
you configure a reusable credential. There is no OS keychain integration.
Environment authentication supports reusable credential references.

For a local project connection, place a WerkConfig in
`.werkworker/werk-config.json`. Bare `ww` automatically reads it from the current
directory (or from `--workspace`). Optionally add `.werkworker/secrets.json` with
`{"api_key":"your-token"}` to preload the masked API-key field. Keep this secret
file gitignored and mode `0600` on Unix; it is stored as plaintext. The wizard
never writes or copies this token to global/project settings. Local credentials
are used only with the automatically discovered local connection; `--werk-config`
bypasses both files, and `--secret-env` overrides the local token. A different
local server URL clears inherited global authentication. The connection is still
shown in the connection selection before any reconnect request. Do not put keys in rule files, profiles or
chat prompts.

**Project model/profile/budgets** writes `.werkworker/config.json` in the selected
workspace, retaining its other project preferences. It does not save the server
URL or authentication. **Do not save** changes neither file. An existing project
config selects project saving by default; otherwise global saving is selected.
Concurrent project-config edits cause a conflict instead of an overwrite.
See [project configuration and profiles](project-configuration.md).

The default interactive workspace access is read-only with an approval for each
operation. You can explicitly choose no access, read/write, or read/write plus
sandboxed processes. Saved setups remember this explicit selection for the current
workspace and show it next to **Continue previous**. Choosing Continue reuses it
for new chats; **Connect new** defaults to read-only. Each operation still needs
approval; native sandbox support is still required. An explicit `--grants` file
takes precedence for new sessions. Existing sessions retain their stored policy.
Project access requests are displayed but never select a broader policy for you.

For code changes and `cargo test`, choose both **Agent profile: coding-basic**
and **Workspace access: Read/write + sandbox processes** using Left/Right on each
field. The profile supplies tools; workspace access grants host permission.
Choosing only one is insufficient. In chat, F2 or `/permissions` shows both
settings and how to start a new session with different access. Replying "yes"
to an agent question cannot change either setting.

## Existing CLI paths

`ww --werk-config config.json` seeds the dialog with that WerkConfig. Supplying
both `--werk-config` and `--secret-env` retains the direct connection path; add
`--setup` to review/change it interactively. Explicit scripted/demo paths remain
offline. `run`, `diagnose` and `rpc` do not read global connection settings or ask
for credentials: provide their existing explicit transport and environment flags.
Headless runs retain their default 2,048-token output reserve; the wizard's budget
applies to runs started in its connected TUI session.

For the currently discussed Qwen server, use the [local example](local-werk-example.md).
