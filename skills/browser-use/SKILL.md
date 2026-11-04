---
name: browser-use
description: Complete browser tasks by observing the current page, choosing actions and verifying their effects; use vision for visual surfaces.
---

Derive a goal and observable completion criteria from the user's request.
Use task.plan for milestones/checkpoints. This session state survives compaction;
reuse it directly, never via invented runtime paths. Receipts establish observations.
Open with exactly one target, for example browser.open {"url":"https://example.com"}
or {"path":"index.html"}. Only a successful receipt supplies a live browser_id;
validation errors and cancelled_before_start do not create a browser.
Reuse the live browser_id. Observe, choose one useful action, then inspect its
effect. Tool success alone is not task success.
Use named DOM/accessibility controls and fresh target.ref values. References
expire on the next observation or state change; never reuse them across actions.
Click an observed control with target:{"ref":"e123"}; an optional name is explanatory
metadata and never replaces the ref. Reported color and selected fields describe
the current controls. Choose palette colors from those observations or the screenshot;
do not invent CSS selectors or repeatedly select an already selected drawing tool.
action_target records the element actually acted on before references expired.
Selecting a color/tool does not paint the drawing surface; verify the visible effect.
For drawings, boards, charts or other visual work request visual:true in Auto
mode, then use browser.action with action:click_at, move or drag. These are action
values, not separate tools; include the live browser_id and coordinates or a drag
path chosen from the latest screenshot. Vision and detected Auto visual surfaces
always supply frames when image input is available; screenshot:false cannot turn
off this feedback. screenshot:true also requests a frame in DOM mode.
Viewport coordinates cover the whole page, not just a drawing canvas. Locate the
intended surface in the screenshot and its controls[].bounds; keep drawing points
inside that surface, excluding palettes, toolbars and gray workspace margins.
drag holds one mouse button from the first path point until release at the last.
Rectangle, ellipse and selection tools normally use two opposite corners; do not
trace a closed outline with those tools, since returning to the start collapses
the shape. Freehand strokes and separate line segments use different gestures.
Keep screenshots enabled for visual actions and inspect the returned frame.
An action already observes its result; request another observation only if the
frame is missing, stale, still loading or an uncertain effect needs checking.
Screenshots are already attached as images to browser tool results. Artifact IDs
are not workspace file paths; do not pass them to files.read_image. For a fresh
or still-loading page, use browser.observe with the live browser_id and screenshot:true.
Use press for keys/chords and type for text into the current focus after clicking
or tabbing there. These mouse/keyboard actions work without DOM targets.
Multiply image coordinates by screenshot.scale_x/scale_y for viewport coordinates.
If image input is unsupported or a frame is missing, name the specific visual
limitation; do not pretend to see it or switch backend on your own.
Wait in bounded intervals for animations, opponent turns and delayed UI changes.
Read readiness and observation_errors; readiness cannot predict future updates.
After compaction reuse the retained current observation; compaction itself does
not reload or clear the page. After navigation, Continue, stale references or
browser_changed, obtain a current observation before acting. After restart historical IDs are not
live sessions; open a new browser. If action_executed is true but observation
fails, observe again instead of repeating the action. Inspect unknown effects.
If browser_state is crashed or browser_page_crashed is reported, close that
browser session and open a new one; observing or navigating the crashed tab
cannot recover it. Do not repeat actions whose remote effects are unknown.
Keep a healthy browser open for follow-up work. Close it explicitly when the
user requests it or when the task calls for cleanup, rather than after every task.
Honor site permissions, Ask/Deny, protected inputs and manual takeover. Do not
replace visual interaction with scripts, internal game state or solution APIs.
Report completion against observed criteria and state remaining uncertainty.
For browser-only tasks stay in the browser UI; do not switch to files/process or
browser.check. If code changed, retain its behavioral checks.
