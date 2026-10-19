# Interactive agent terminal

`ww` (or `werk-worker`) opens the terminal UI. `ww --werk-config config.json`
connects to Werk using the same explicit provider setup as the SDK. Without a
provider, the UI shows setup instructions and never silently uses a demo model.
`ww tui --demo "Hello"` explicitly uses one scripted response. `ww run ...` is
the headless path; `ww rpc` continues to emit JSONL only.

Wide terminals show the five-row ASCII startup letterforms from werk1112,
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
| Ctrl-C | Cancel the active run; press again consecutively to quit (any other key resets this) |
| Ctrl-Q | Cancel/clean up managed work, close the session and restore the terminal |
| Page Up / Page Down | Scroll conversation history |
| Alt-Up / Alt-Down | Scroll full approval details, question or session information |

Slash commands: `/help` shows commands and shortcuts, `/clear` clears only the
visible conversation, `/cancel` cancels the active run, and `/quit` closes the
session with the same cleanup as Ctrl-Q. Commands work without a connected model.
While answering a pending question, slash-prefixed input is treated as an answer.

The conversation streams attempt text and tool states. Failed attempts remain
visibly interrupted. The side panel shows the concrete prepared approval details
including file differences and actual process rights; the UI adds no grants.
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
