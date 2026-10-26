# Interactive agent terminal

`ww` (or `werk-worker`) opens the terminal UI. `ww --werk-config config.json`
seeds the [interactive connection wizard](setup.md). Without configuration, the
UI asks for the server, credentials, model and agent settings. It never silently
uses a demo model. Saved global settings and [project rules/preferences](project-configuration.md)
are loaded on subsequent starts. **Continue previous** reconnects to the remembered
service; **Connect new** opens the full wizard. No inference runs during connection.
`ww tui --demo "Hello"` explicitly uses one scripted response. `ww run ...` is
the headless path; `ww rpc` continues to emit JSONL only.

The default **Compact** layout has one chat column, a short status header and
inline magenta cards for questions and approvals. The conversation stays visible
above each card. Subagent selection and details are stacked vertically as well.
Use **F4** or **`/view`** to choose Compact or **Cockpit** with Up/Down and Enter;
Esc cancels. Cockpit keeps the conversation/sidebar layout. The choice is saved
in `ui.json` beside the global connection config and survives restarts.
Changing layouts preserves the session, pending request and any draft answer.

In Cockpit, wide terminals show the five-row ASCII startup letterforms from werk1112,
spelling WERKWORKER with the `werkWorker · Agent Engine` caption. Narrow or
short terminals use a compact wordmark. No banner is emitted into JSONL.

The UI uses ratatui/crossterm. Its background/panel/text colors and five brand
accents are the exact RGB values from werk1112 `top/ui.rs` and `terminal.rs`:
background `(9,13,23)`, panel `(15,21,35)`, text `(224,230,248)`, muted
`(129,143,174)`, cyan `(0,220,255)`, blue `(59,130,246)`, indigo `(99,102,241)`,
violet `(139,92,246)` and pink `(255,79,195)`. A true-color terminal displays
these directly. See NOTICE for attribution.

Fenced source code has a separate charcoal/olive background, a language label,
and muted sage strings, ochre numbers, mauve keywords and gray-green comments.
Indentation is retained; an unfinished code fence is styled while streaming.
Blocks longer than eight display rows start collapsed, showing up to three
preview lines. Long preview lines are shortened to the pane width. Click the
**▶ / ▼ header** to expand or collapse that block; **Ctrl-O** toggles all blocks.
`/expand` and `/collapse` explicitly open or close all blocks, including subsequent
output in the current chat. Individual choices survive streaming and layout
changes. Folding only changes the display: the full available transcript remains
intact, and expanding restores the content. Questions and approvals stay visible.

| Key | Behavior |
|---|---|
| Enter | Submit a message, steer the active run, or answer the pending question |
| Left / Right, Backspace / Delete | Move the cursor or edit at its position |
| Ctrl-Left / Ctrl-Right | Move by word |
| Up / Down (nonempty draft) | Move between draft lines |
| Alt-Enter / Ctrl-J | Insert a newline without sending |
| / | Preview slash commands, filtered as you type |
| Up / Down, Tab, Enter, Esc | Select a suggestion, complete it, execute it, or dismiss the preview |
| Ctrl-Y / Ctrl-N | Allow the displayed prepared action once / deny it |
| Up / Down, Enter (empty input, pending request) | Select and send a suggested answer or approval decision |
| Ctrl-U | Clear a draft answer to use the selection again |
| Ctrl-O / `/expand` / `/collapse` | Toggle, expand or collapse all code blocks |
| Click a code block header | Expand or collapse that block |
| F4 / `/view` | Choose and remember Compact or Cockpit |
| F5 / `/settings` | Edit the current profile, permissions, enabled tools and skills |
| `/sessions` | Choose a new chat or a previous conversation in this workspace |
| F2 / `/permissions` | Inspect tools/skills/grants; E opens settings, Enter opens the all-tools approval shortcut |
| F3 / `/agents` | Open the subagent inspector; Up/Down selects a child, Page Up/Down scrolls its details, Esc returns |
| Left-button drag, Ctrl-C | Select visible text within one pane and copy it; Esc clears the selection |
| Ctrl-V / Shift-Insert | Paste clipboard text into the draft; Enter sends it |
| Right-click | Copy selected text, otherwise paste (one click) |
| F6 | Toggle native terminal selection for terminal-provided copy/paste shortcuts |
| Ctrl-C (without selection) | Cancel the active run; press again consecutively to quit (any other key resets this) |
| Ctrl-Q | Cancel/clean up managed work, close the session and restore the terminal |
| Page Up / Page Down | Scroll conversation history |
| Mouse wheel | Independently scroll the pane under the pointer: chat, request details, session sidebar, agent list/details, permissions or input |
| Home / End | Move to the start/end of the draft line; with an empty draft, scroll to oldest/latest history |
| Ctrl-Home / Ctrl-End | Scroll to oldest/latest history |
| Alt-Up / Alt-Down | Scroll pending approval/question details, or the session sidebar when no request is pending |

Clipboard paste preserves Unicode and line breaks in the draft. Bracketed paste
also works with the terminal's paste shortcut (usually Ctrl-Shift-V); pasted
newlines never submit a message or approval. The expanded input is limited to
64 KiB. Large pastes appear as numbered blocks and are expanded losslessly when
sent; deleting a block removes its complete pasted text. Oversized pastes are
rejected with a visible notice. Multiline drafts expand and keep the editing
cursor visible; the mouse wheel can also scroll the input.

Clipboard imports show an animated indicator immediately, including while image
decoding/resizing is running. Typing remains available; Enter waits until the
import finishes, and Ctrl-U cancels it. A completion notice reports the pasted
character count or image number. Empty, unsupported, failed and timed-out reads
show an error instead of silently doing nothing. Pasting from an inspector asks
you to return to chat first. If the terminal intercepts Ctrl-V itself, use
**Alt-V** or Shift-Insert to invoke WerkWorker's clipboard reader; an intercepted
key that produces no terminal input cannot be detected by the application.

Images appear as `Image #1`, `Image #2`, etc. with dimensions in the right sidebar:
`draft` before sending and `sent` after the session accepts the message. Sent
images remain listed during follow-ups and are restored from saved history when
reopening the session. Compact mode also shows this sidebar when images are
present and the terminal is at least 70 columns wide. Image numbering is separate
from pasted text blocks. The prompt border also animates while sending a message
and waiting for the model to process it, including long text-only prompts.

Direct clipboard access uses PowerShell on Windows/WSL, `osascript` (AppKit), `pbcopy`/`pbpaste` on
macOS, or `wl-copy`/`wl-paste` and `xclip`/`xsel` on Linux. If these helpers are
unavailable, copying falls back to the terminal's OSC 52 support; for pasting,
use the terminal's paste shortcut. F6 releases mouse capture so native terminal
selection and context menus work, then restores pane scrolling when pressed
again. Shift-drag also bypasses mouse capture in many terminals.
While dragging an application selection, only the selected pane stays still so
new output cannot change the text being copied. Status, errors and the rest of
the interface stay live. A simple click does not pause output. New approvals or
questions clear the selection and appear immediately.

Slash commands: `/help` shows commands and shortcuts, `/clear` clears only the
visible conversation, `/cancel` cancels the active run, `/agents` opens live child details, `/permissions` explains current tools and rights, `/settings` edits session/project configuration, `/setup` reconnects through the configuration wizard when no run is active, and `/quit` closes the
session with the same cleanup as Ctrl-Q. `/effort` (alias `/reasoning`) opens a
small slider above the input: Left/Right chooses a valid protocol tier, Enter
applies, and Escape cancels the preview. `/effort high` selects a tier directly;
`/effort default` restores the server default. The choice is saved with the
session and used for newly planned requests, including subagents. Reasoning
controls require a connected provider; other local commands work offline.
Known commands also work while answering a pending question. Other slash-prefixed
input is treated as an answer. F2/F3 preserve any draft answer.

The conversation streams attempt text and tool states. Scrolling up anchors the
reading position while new output arrives; End or scrolling to the bottom resumes
following. A scrollbar and position indicator show where you are. Wrapping and
scroll offsets use the same terminal-width calculation, including wide characters.
Failed attempts remain visibly interrupted. Terminal run errors show their code,
message and next action in the transcript, including when reopening a failed chat.
Completed and failed runs leave the session open for follow-up messages.
Messages sent during a running task steer that run. The chat confirms when an
update is queued and when it enters a model request. The current tool batch or
generation finishes first; context planning and subsequent retries incorporate
new updates before dispatch. User messages remain verbatim through compaction.
The model is instructed to acknowledge changes and reconcile the task plan and
verification; delivery alone is not evidence that the requested change is done.
Pending approvals and questions appear below the conversation
and identify their owning agent. Questions show the agent's suggested answers plus
**Write my own answer**. Use Up/Down and Enter, or type a custom answer directly.
When the input contains text, Enter sends that text instead of the highlighted
choice. Questions and submitted answers remain in the visible chat history.
Approval cards offer **Allow once** and **Deny** (initial selection); use Up/Down
and Enter or Ctrl-Y/Ctrl-N. Approval details include file differences and actual
process rights; Alt-Up/Down scrolls them independently of the choice list.
On an approval card, typing **ja/yes/y/allow/erlauben** and Enter allows that
prepared action once; **nein/no/n/deny/ablehnen** rejects it. Other chat text is
not an approval. Inspectors do not accept approval keystrokes
for hidden requests: return with Esc to review and answer the displayed request.
The Cockpit sidebar summarizes the profile and host access; F2 exposes these
details in either layout. The footer indicates either approval-request mode or
restricted access. The model and the bundled task/coding skills direct permission
requests through the actual tool, so the host can show a real action approval.

New interactive setups default to **All tools; explicit approval for each action**,
exposing registered tools and skills, including the bundled coding skill, unless
the project explicitly selects a subset.
To upgrade an existing restricted chat, open **F2**, press **Enter**, select
**Enable all tools; ask before actions** with Up, and confirm with Enter.
The default choice keeps current permissions. Enabling requests itself executes
nothing; the actual file change or sandbox command still requires approval.
The conversation, pending ordinary question and session ID are preserved.
Finish/cancel active subagents before changing access, then continue in the same
session; new children inherit the mode and ask for their own actions.
Access is not
changed when reopening an existing session: it retains its stored profile and
policy. **Continue previous** shows and reuses the access selected in the last
saved setup or settings editor for this workspace when creating a new session. **Connect new** starts
with all tools and explicit approvals. An explicit `--grants` file controls newly created sessions;
opening an old session retains that session's stored grants.
Questions are answered through their durable request IDs. Typing another message
after completion starts a new run in the same session. Display history is bounded;
full committed events remain in SQLite. Untrusted text cannot emit terminal control
characters. Redraws happen only after changes, at most 20 times per second.

## Edit profiles, permissions, tools and skills

Open **F5** or **`/settings`** (also **E** from F2). Use **Tab/Shift-Tab** or
**Left/Right** to switch between Profiles, Permissions, Tools and Skills.
**Up/Down** selects an entry; **Space/Enter** chooses a profile or access preset,
or toggles an individual tool/skill. Profiles include bundled and project/user
profiles. Choosing a profile restores that preset's tool and skill selection;
permissions are chosen separately. The details pane shows descriptions, profile
instructions and skill contents. Both panes scroll independently with the wheel;
Page Up/Down scrolls the details.

**Ctrl-S** applies the draft to the current session and saves project defaults.
**P** switches to saving only the current session. **Esc** closes the editor and
discards unapplied changes. Applying requires an idle root session: finish or
cancel active work first, including interrupted runs and subagents. The session
ID and conversation history are preserved.

Profile, tool and skill selections are saved in `.werkworker/config.json`,
preserving other project settings and checking for concurrent changes. Host
access is saved separately under the user's `werkworker/workspace-access`
configuration directory, bound to the workspace identity. A project file alone
cannot grant access. Defaults apply to **New session** through `/sessions` and
after restarting; previously saved chats keep their own settings. Explicit
`--grants` continues to override defaults for new sessions. Selecting a new
access level in connection setup supersedes saved access for that connection.

Disabled tools remain disabled even in approval-request mode. Disabled skills
are no longer advertised or loadable in subsequent runs; historical snapshots
remain recorded. Hard-denied tools stay denied. Permission presets replace
scoped grants with the selected scope and require approval for external actions.
If saving defaults fails after the session update, the editor reports that
partial save explicitly; resolve the error before retrying Ctrl-S.

## Previous sessions

After connecting, the startup list offers **New session** and previous chats
from the current workspace, most recently used first. Titles use a saved name or
the first prompt, with age, status, profile, turns, image count and session ID.
Empty chats can also be managed; internal subagent sessions are omitted.
With no previous chats, the first chat opens
directly. An initial CLI prompt starts a new session unless `--session` is given.
`/sessions` opens the list after the active run finishes or is cancelled;
Esc returns to the current conversation. On startup, Esc exits.

In the session list, **R** renames the selected conversation (1–80 characters),
**/** filters by title, status or ID, and **F5** refreshes. **Delete** or **D**
opens a permanent-deletion confirmation with **Cancel** selected by default.
Use arrows and Enter to confirm. Deletion removes the conversation history,
inline images and all descendant subagent sessions atomically. Active work must
finish or be cancelled first. Independent forks, project files and shared
artifact blobs remain. There is no undo. After deletion, the list updates
immediately; deleting the currently open conversation also invalidates its old
handle. Esc exits if that conversation no longer exists.

CLI management works without a model connection or an existing workspace:

```sh
ww sessions list
ww sessions rename SESSION_ID "New title"
ww sessions export SESSION_ID > session-backup.json
ww sessions delete SESSION_ID
ww sessions delete SESSION_ID --yes
```

Interactive CLI deletion asks for confirmation; scripts must supply `--yes`.
Use `--store PATH` before `sessions` for a different database. The exclusive
store lock still applies: close another `ww` process using that database before
managing it from a separate CLI process. Within the TUI, use `/sessions`.

Opening a completed conversation restores its committed messages and permits
follow-up messages in the **same session**. The model receives the stored history,
subject to normal context compaction. Opening history never replays completed
model requests or tools. Previous subagents remain available through F3.
Sessions live in the worker's SQLite store; server-side inference persistence
is not required. `--store` must point at the same database to find previous chats.

An interrupted run offers **Resume interrupted run** or **Open history without
resuming**. Failed, cancelled and completed runs are not automatically retried.
Opening interrupted history leaves it read-only until you resume through
`/sessions`; it does not start a new run over unresolved tool calls.
Explicitly open a conversation with:

```sh
ww tui --session SESSION_ID
```

Explicit run recovery:

```sh
ww --werk-config config.json tui --session SESSION_ID --run-id RUN_ID
```

An interrupted safe run resumes using SDK recovery. An unknown external outcome
still requires host reconciliation; the UI cannot authorize an automatic retry.
Both stdin and stdout must be terminals. Startup errors and panics restore raw
mode/alternate screen before returning to the shell. No model/network call is
made just to open the unconfigured setup screen.

Subagent status is refreshed from durable snapshots every 250 ms while a run is
active. The first pending request across the tree is shown with its owning goal;
approval/answer controls address that child's RunHandle. Subsequent pending
requests appear after resolution. The ordinary transcript streams parent events;
the subagent inspector shows each child's goal, status, model text, tool arguments
and results, approvals, questions and final result/error, refreshed while running.
The selected child stays selected as statuses change, and completed children remain
inspectable until the next root run. Displayed histories are bounded; full durable
records remain in child sessions. Ctrl-C and closing the terminal
cancel descendants and wait for cleanup. See [subagents](subagents.md).

Context planning also reports its conservative token estimate and compaction
notices. These are estimates, not server usage; see [context handling](context.md).

Clipboard images can be attached with Ctrl-V, Alt-V, Shift-Insert or right-click.
Alt-V is useful when the terminal consumes Ctrl-V itself. Terminal bracketed
paste (usually Ctrl-Shift-V) transports text; it does not carry clipboard bitmap
data. Native import reads PNG/JPEG on Wayland/X11, Windows/WSL and macOS, shows
an `[Image #…]` draft block, and removes the whole attachment with Backspace or
Delete. Images travel as standard OpenAI `image_url` content parts and survive
session storage/recovery. Text-only messages keep their existing string format.

Import runs in the background; typing and navigation stay responsive while a
clipboard helper starts. Enter waits for an outstanding paste, and Ctrl-U
cancels it and clears the draft. Clipboard decoding is bounded (16 MiB source,
128 MiB decoder allocation); images are normalized to JPEG, at most 1024 pixels
on the longest side and 128 KiB each, with at most four per message. This can
reduce fine detail in large screenshots. No clipboard image is uploaded until
the user submits the message.

Setup exposes **Image input**: Disabled, Auto (server validation), or Enabled
for a known vision model. Import itself does not turn a text-only model into a
vision model. An explicitly disabled connection keeps the image draft and
reports the missing capability when sending. Pending tool approvals and
questions accept text answers; image drafts are retained instead of being
silently discarded.

The regression runner `scripts/tests/clipboard_tui_smoke.py <ww-binary>` checks
32 KiB unbracketed input, continued typing during a delayed clipboard operation,
PNG import/removal, and the actual persisted image+text message.

The Unix terminal reader uses level-triggered polling on a dedicated thread.
A bounded event queue and batched text editing keep unbracketed paste responsive
as well; terminal input never waits for clipboard helper processes.
