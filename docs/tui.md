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

| Key | Behavior |
|---|---|
| Enter | Submit a message, steer the active run, or answer the pending question |
| Backspace | Edit the input |
| / | Preview slash commands, filtered as you type |
| Up / Down, Tab, Enter, Esc | Select a suggestion, complete it, execute it, or dismiss the preview |
| Ctrl-Y / Ctrl-N | Allow the displayed prepared action once / deny it |
| Up / Down, Enter (empty input, pending request) | Select and send a suggested answer or approval decision |
| Ctrl-U | Clear a draft answer to use the selection again |
| F4 / `/view` | Choose and remember Compact or Cockpit |
| `/sessions` | Choose a new chat or a previous conversation in this workspace |
| F2 / `/permissions` | Inspect tools/skills/grants; Enter opens confirmation to enable approval requests |
| F3 / `/agents` | Open the subagent inspector; Up/Down selects a child, Page Up/Down scrolls its details, Esc returns |
| Ctrl-C | Cancel the active run; press again consecutively to quit (any other key resets this) |
| Ctrl-Q | Cancel/clean up managed work, close the session and restore the terminal |
| Page Up / Page Down | Scroll conversation history |
| Mouse wheel | Scroll chat or inspector under the pointer (Shift selects terminal text in most terminals) |
| Home / End | Jump to oldest visible history / latest output and resume following |
| Alt-Up / Alt-Down | Scroll full approval details, question or session information |

Slash commands: `/help` shows commands and shortcuts, `/clear` clears only the
visible conversation, `/cancel` cancels the active run, `/agents` opens live child details, `/permissions` explains current tools and rights, `/setup` reconnects through the configuration wizard when no run is active, and `/quit` closes the
session with the same cleanup as Ctrl-Q. Commands work without a connected model.
Known commands also work while answering a pending question. Other slash-prefixed
input is treated as an answer. F2/F3 preserve any draft answer.

The conversation streams attempt text and tool states. Scrolling up anchors the
reading position while new output arrives; End or scrolling to the bottom resumes
following. A scrollbar and position indicator show where you are. Wrapping and
scroll offsets use the same terminal-width calculation, including wide characters.
Failed attempts remain visibly interrupted. Terminal run errors show their code,
message and next action in the transcript, including when reopening a failed chat.
Completed and failed runs leave the session open for follow-up messages.
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
exposing all registered tools and skills, including the bundled coding skill.
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
saved setup for this workspace when creating a new session. **Connect new** starts
with all tools and explicit approvals. An explicit `--grants` file controls newly created sessions;
opening an old session retains that session's stored grants.
Questions are answered through their durable request IDs. Typing another message
after completion starts a new run in the same session. Display history is bounded;
full committed events remain in SQLite. Untrusted text cannot emit terminal control
characters. Redraws happen only after changes, at most 20 times per second.

## Previous sessions

After connecting, the startup list offers **New session** and previous chats
from the current workspace, most recently used first. Titles use the first
prompt, with age, status, profile and session ID. Empty chats and internal
subagent sessions are omitted. With no previous chats, the first chat opens
directly. An initial CLI prompt starts a new session unless `--session` is given.
`/sessions` opens the list after the active run finishes or is cancelled;
Esc returns to the current conversation. On startup, Esc exits.

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
