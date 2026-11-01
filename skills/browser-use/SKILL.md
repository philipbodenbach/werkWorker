---
name: browser-use
description: Complete browser tasks by observing the current page, choosing actions and verifying their effects; use vision for visual surfaces.
---

Derive a goal and observable completion criteria from the user's request.
For multiple steps record the subgoal, next step and uncertainties in task.plan;
plans describe intent, while browser receipts establish observations.
Reuse the live browser_id. Observe, choose one useful action, then inspect its
effect. Tool success alone is not task success.
Use named DOM/accessibility controls and fresh target.ref values. References
expire on the next observation or state change; never reuse them across actions.
For drawings, boards, charts or other visual work request visual:true in Auto
mode, then use click_at, move and drag. Vision supplies frames automatically;
screenshot:true requests a frame even in DOM mode, screenshot:false suppresses it.
Multiply image coordinates by screenshot.scale_x/scale_y for viewport coordinates.
If image input is unsupported or a frame is missing, name the specific visual
limitation; do not pretend to see it or switch backend on your own.
Wait in bounded intervals for animations, opponent turns and delayed UI changes.
Read readiness and observation_errors; readiness cannot predict future updates.
After navigation, Continue, compaction, stale references or browser_changed,
observe again before choosing an action. After restart historical IDs are not
live sessions; open a new browser. If action_executed is true but observation
fails, observe again instead of repeating the action. Inspect unknown effects.
Honor site permissions, Ask/Deny, protected inputs and manual takeover. Do not
replace visual interaction with scripts, internal game state or solution APIs.
Report completion against observed criteria and state remaining uncertainty.
Pure browser work needs no invented file edits or browser.check evidence; when
code was changed retain the existing behavioral checks on those files.
