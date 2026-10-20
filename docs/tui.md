# Interactive agent terminal

`ww` (or `werk-worker`) opens the terminal UI. `ww --werk-config config.json`
seeds the [interactive connection wizard](setup.md). Without configuration, the
UI asks for the server, credentials, model and agent settings. It never silently
uses a demo model. Saved global settings and [project rules/preferences](project-configuration.md)
are loaded for review on subsequent starts.
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
| F2 / `/permissions` | Inspect the current profile, grants and instructions for changing access |
| F3 / `/agents` | Open the subagent inspector; Up/Down selects a child, Page Up/Down scrolls its details, Esc returns |
| Ctrl-C | Cancel the active run; press again consecutively to quit (any other key resets this) |
| Ctrl-Q | Cancel/clean up managed work, close the session and restore the terminal |
| Page Up / Page Down | Scroll conversation history |
| Alt-Up / Alt-Down | Scroll full approval details, question or session information |

Slash commands: `/help` shows commands and shortcuts, `/clear` clears only the
visible conversation, `/cancel` cancels the active run, `/agents` opens live child details, `/permissions` explains current tools and rights, `/setup` reconnects through the configuration wizard when no run is active, and `/quit` closes the
session with the same cleanup as Ctrl-Q. Commands work without a connected model.
Known commands also work while answering a pending question. Other slash-prefixed
input is treated as an answer. F2/F3 preserve any draft answer.

The conversation streams attempt text and tool states. Failed attempts remain
visibly interrupted. Pending approvals and questions appear below the conversation
and identify their owning agent. Questions show the agent's suggested answers plus
**Write my own answer**. Use Up/Down and Enter, or type a custom answer directly.
When the input contains text, Enter sends that text instead of the highlighted
choice. Questions and submitted answers remain in the visible chat history.
Approval cards offer **Allow once** and **Deny** (initial selection); use Up/Down
and Enter or Ctrl-Y/Ctrl-N. Approval details include file differences and actual
process rights; Alt-Up/Down scrolls them independently of the choice list.
Typing a chat message does not approve a prepared action. Inspectors do not accept approval keystrokes
for hidden requests: return with Esc to review and answer the displayed request.
The Cockpit sidebar summarizes the profile and host access; F2 exposes these
details in either layout.

The setup explicitly selects session grants; a reply such as "yes" to an agent
question cannot enlarge them. To create files and run `cargo test`, select both
the `coding-basic` profile and `Read/write + sandbox processes` workspace access.
If the current run has insufficient rights, use `/cancel`, then `/setup`, choose
the profile/access, and resubmit the task in the new session. Access is not
remembered as an automatic grant on the next launch. An explicit `--grants` file
continues to control host access when supplied.
Questions are answered through their durable request IDs. Typing another message
after completion starts a new run in the same session. Display history is bounded;
full committed events remain in SQLite. Untrusted text cannot emit terminal control
characters. Redraws happen only after changes, at most 20 times per second.

Explicit recovery:

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
