# Browser agent review and opt-in evaluation

Implemented locally against `efc7e1603412be232f98299a17e4dfea6f872ebe`
(`feat/browser-agent`, 2026-11-01), newer than the prompt's `cbf73a7` review
basis. Existing browser isolation, permissions, image transport, panel controls,
review-report phase, WaitGraph and recovery mechanisms were reused.

## Confirmed defects and validation

| Finding | Trigger and impact | Correction | Regression |
| --- | --- | --- | --- |
| A1, `harness.rs` / `api.rs` | Repeated identical browser observations/actions hit the exact-call limit despite changed state | Scoped receipt-derived DOM progress and finite coarse visual novelty; consistent direct/indexed/feedback counts; external-write classification retained | Real main-agent Chromium loop with repeated clicks, keys and observations; idle/blinking counterpart; foreign session/tab and visual-budget unit cases |
| A2, `review_progress.rs` | Four folders with identical relative entry names looked like repeated evidence | Normalized directory identity only for `files.list`; search query churn still ignored | Four siblings, `./` and `/workspace/` aliases, real repeat and unchanged searches |
| A3, `repair_cycle.rs` | Varying waits/prefixes re-earned an already successful assertion and replenished the repair budget | Freeze original assertion goals and credit each newly passed goal once; original obligation and replacement ancestry retained | Sixteen unchanged failures with varying waits, added trivial assertions, changing diagnostics and serialization/resume; actual original-goal improvements |
| A4, Python driver | A fixed 50 ms missed delayed updates; early text extraction failure could discard a usable frame | Bounded DOM settling and independent observation parts after a complete protection scan; action outcome recorded separately | Real 250 ms UI update; real Chromium with injected text/ARIA failure and single-action assertion; protection failure emits no text/frame |

DOM stability is observational, not a guarantee that later timers will not fire.
A coarse visual change is not proof of task success. It can grant at most twelve
novelty credits per browser/tab/run; absolute, idle and action budgets remain.
Protection heuristics are not universal secret detection. A failed protection
scan now fails closed, including frame/field-count overflow.

The original repair tests now use a fixed set of original goals: inventing a new
assertion at each failed recheck no longer counts as genuine improvement. The
existing browser recovery test retains all recovery/compaction assertions, with
20k context capacity for the expanded v3 contract and skill metadata.
Tool descriptions stay compact and browser guidance is scoped to browser-capable
contexts. Existing small-context file/skill, read recovery and permission-upgrade
regressions retain their original budgets and assertions.

## New integration

- Bundled `browser-use` skill with immutable session snapshots, activation in
  browser-capable runs, profile integration and restoration into older sessions.
- Host-owned Auto/DOM/Vision settings, generic `visual:true`, explicit screenshot
  overrides and prepared-setting checks. `browser.open` is version 4 after the
  native-schema correction below; the other browser controls remain version 3.
- Observation identity and a bounded browser checkpoint in Working State;
  at most two browser frames in planned requests, with user images retained.
- Panel/TUI settings, observed activity, task status, vision problems and expiry.
  Continue and compaction of the latest observation require a fresh observation.
  An independent refresh marker also blocks reused `browser.open` when Continue
  occurs while an executing action owns the browser outside the session map.
- Absolute lifetime, idle timeout and action budgets; bounded defaults and
  host-only changes. Existing run limits remain independent.
- Structured transport policy and block reasons, including WebSockets,
  background requests and server-sent event streams. No network isolation was
  relaxed and no additional inference server was added.

## Live-site probe, 2026-11-01

The ignored `live_browser_compatibility_probe` was explicitly run in Chromium
through the real isolated Rust broker, allowing only each starting origin.
This is a compatibility probe, not an autonomous model run or proof of playability.

| Target | Observed result | Remaining limit |
| --- | --- | --- |
| `https://jspaint.app/` | Title “JS Paint”, screenshot, navigation timeout at 15 s, no extracted controls | Additional GitHub, flipanim.com and i.postimg.cc requests were denied; drawing workflow not verified |
| NYT Wordle | Page loaded, one extracted control | Additional NYT asset/API origins (including static01.nyt.com / static01.nytimes.com) denied; puzzle play not verified |
| `https://lichess.org/` | Context/page closed during navigation; image.lichess.org denied | Cause of context closure not isolated; no legal-move/game compatibility claim |
| CrazyGames pool page | Page shell, 28 controls and a frame | builds.crazygames.com and privacy CDN denied; embedded game not verified |

Denied origins are observations, not an automatically approved allowlist, and
are not asserted to be the sole cause of these failures. WebSocket/service-worker
and background-network requirements remain unsupported. No blanket streaming
support was built for unverified games. Original demo game URLs were not all
recoverable; Lichess and CrazyGames are explicit additional compatibility targets.

## Reproduce a real model evaluation

Install the browser runtime using the existing setup instructions and start your
already configured Werk server. Supply a **real** `WerkConfig` JSON, with tool
calling enabled and an appropriate image-capable backend for visual cases.
Use a sufficiently large context: each selected image conservatively reserves
8192 tokens unless the backend supplies better counts. Model names are not
whitelisted; Qwen3.8-27B is a comparison candidate only.

```sh
cargo run --locked --example browser_live -- \
  --config /path/to/werk-config.json --secret-env WERK_API_KEY \
  --case visual-puzzle --perception vision --output /tmp/ww-visual-eval

cargo run --locked --example browser_live -- \
  --config /path/to/text-model-config.json --secret-env WERK_API_KEY \
  --case form --perception dom --output /tmp/ww-dom-eval

cargo run --locked --example browser_live -- \
  --config /path/to/werk-config.json --secret-env WERK_API_KEY \
  --case jspaint --perception vision --seconds 1800 --steps 100 \
  --output /tmp/ww-jspaint-eval
```

Omit `--secret-env` for a server that does not require authentication. Other
cases are `wordle`, `chess` and `pool`. `--origin https://approved.example` adds an
explicit host-approved exact origin; unapproved scope expansion is denied and
reported. `--seconds` and `--steps` are host budgets, never model tools.

The executable uses `WerkProvider` and the ordinary engine. It supplies only a
natural-language goal, without scripted tool decisions. Its profile excludes
file/process tools, `browser.evaluate`, `browser.check`, direct canvas/DOM
mutation, internal game-state access and solution APIs. The visual puzzle draws
shapes only on canvas, changes their positions after each step and exposes no
solution through accessibility text. The host independently tests the final
frame's pixels; the form case checks the actual confirmation. Remote games and
JSPaint require human assessment of saved frames against the listed criteria;
engine completion is explicitly not recorded as success for those cases.

`report.json` records commit/dirty state, configured model (quantization only if
known from its ID), backend, perception, budgets, target, duration, model/tool/
image counts, transport blocks and measured criteria. `final.jpg` is the last
available image for independent review. Logs omit image payloads and secrets.
A successful single run is only a smoke test.

The configured local endpoint `http://127.0.0.1:11434` refused connections on
2026-11-01. The executable was invoked with the existing Qwen configuration and
reported `backend_unavailable` / `network_error` before any model request. No
model or hardware was downloaded or changed. **A genuine multi-step visual
model run and a text-model DOM run remain open.**

## JSPaint follow-up, 2026-11-01

A real user run on `bbb515b` ended after 987.8 seconds with `repetition_limit`:
three `browser.open {}` calls failed validation and the fourth was stopped
before execution. No JSPaint observation or drawing was produced.
The active backend was configured with Qwen3.8-Flash-Next Q4_K_M and a vision
projector; this failure preceded visual inference.

The running llama.cpp build's schema parser reads `oneOf` alternatives without
merging their siblings. The previous required-only branches lost the target
properties. A reduced reproduction linked against that build's native library
produced `anyOf / any / any`; complete branches produced
`anyOf / object required=url / object required=path`. This confirms a schema
incompatibility; it does not measure the model's drawing ability.

`browser.open` v4 now keeps complete, mutually exclusive object alternatives,
including optional field constraints, with root properties retained for chat
templates. Descriptions stay compact to preserve existing context budgets.
Missing targets and repetition limits are still enforced before effects.
Validation failures no longer fabricate empty browser checkpoints or replace
the last real observation, and exact echoes of emitted checkpoints enter bounded
response recovery. Browser workflows no longer trigger an automatic coding
review at plan creation in Max mode; actual file changes still trigger reviews.

Regression coverage includes independent alternative validation, the serialized
WerkProvider request, the engine's repeated invalid calls, checkpoint provenance
and echo handling, and browser-only plans followed by real implementation work.
The authenticated server returned HTTP 401 to a new client, so no additional
real-model drawing run was claimed during the correction.

Correction validation on `bbb515b` plus local changes: format/check/Clippy passed;
the full Rust suite reported 408 passed, 0 failed and 20 ignored. Explicit
Chromium runs passed 12 `browser_control` tests (2 opt-in tests ignored) and all
8 `browser_mode` tests, including the actual WerkProvider image request path.
Restart the rebuilt worker and choose a fresh session for the next model test;
an already running worker keeps its previous tool definitions.

## Initial feature checks

Completed on 2026-11-01 at `186463b` plus local final corrections:

- `cargo fmt --check`, `cargo check --locked --all-targets` and
  `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `cargo test --locked --all-targets --no-fail-fast`: 405 reported passed,
  0 failed, 20 ignored, including the existing review/partial-evidence,
  WaitGraph, recovery and mixed coding completion regressions. Tests gated
  off without `REQUIRE_BROWSER` are not counted as real-browser evidence.
- Explicit Chromium suites: `browser_control` 12 passed / 2 opt-in tests ignored;
  `browser_mode` 8 passed. A concurrent run initially hit an OS resource error
  while starting Chromium; the separate complete rerun passed.
- Python observation regressions: 2 passed using real Chromium.
- Panel JavaScript syntax checked with `node --check`; `git diff --check` passed.
- The live-site probe was explicitly executed separately; outcomes are above.
  The real-model executable produced a `backend_unavailable` report, not a pass.

Reproduction commands:

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
REQUIRE_BROWSER=1 WW_BROWSER_ENGINES=chromium cargo test --locked \
  --test browser_control --test browser_mode -- --test-threads=1
PYTHONPATH="$HOME/.local/share/werk-worker/tools/browser/python" \
PLAYWRIGHT_BROWSERS_PATH="$HOME/.local/share/werk-worker/tools/browser/browsers" \
  /usr/bin/python3 scripts/tests/test_browser_observation.py
WW_COMPAT_REPORT=/tmp/browser-compatibility.json cargo test --locked \
  --test browser_control live_browser_compatibility_probe -- --ignored --nocapture
```

Only Chromium was exercised for this task. Ignored/offline saved-session replays
and browser tests without `REQUIRE_BROWSER=1` are not evidence of real browsers.
