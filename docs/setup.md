# Interactive connection setup

Start `ww`, or `cargo run --bin ww --` from this checkout. The setup uses the
same colors and banner as the chat. No API request is made until you choose
**Discover models**, and no inference is made until you submit a chat message.

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

Directly entered keys exist only in the process for the connection/session.
They are masked on screen and are absent from settings, prompts, tool arguments
and session exports. You must enter them again after reconnect/restart. There is
no OS keychain integration. Environment authentication is the supported reusable
credential reference. Do not put keys in a project file, profile or chat prompt.

**Project model/profile/budgets** writes `.werkworker/config.json` in the selected
workspace, retaining its other project preferences. It does not save the server
URL or authentication. **Do not save** changes neither file. An existing project
config selects project saving by default; otherwise global saving is selected.
Concurrent project-config edits cause a conflict instead of an overwrite.
See [project configuration and profiles](project-configuration.md).

The default interactive workspace access is read-only with an approval for each
operation. You can explicitly choose no access, read/write, or read/write plus
sandboxed processes. These selections are **session-only** and never remembered
as global grants. Each operation still needs approval; native sandbox support
is still required. An explicit `--grants` file takes precedence over this field.
Project access requests are displayed but never select a broader policy for you.

## Existing CLI paths

`ww --werk-config config.json` seeds the dialog with that WerkConfig. Supplying
both `--werk-config` and `--secret-env` retains the direct connection path; add
`--setup` to review/change it interactively. Explicit scripted/demo paths remain
offline. `run`, `diagnose` and `rpc` do not read global connection settings or ask
for credentials: provide their existing explicit transport and environment flags.
Headless runs retain their default 2,048-token output reserve; the wizard's budget
applies to runs started in its connected TUI session.

For the currently discussed Qwen server, use the [local example](local-werk-example.md).
