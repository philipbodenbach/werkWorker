---
name: coding-basic
description: Read project instructions, make a small verified code change and report its diff. Use when a user requests repository work.
metadata:
  werk-worker-version: "1"
---
Read AGENTS.md; preserve user work. Follow this outcome-driven loop:
1. Extract requirements from the request AND supplied references. In task.plan,
   state outcomes, acceptance criteria and check methods. Cover behavior, content,
   appearance and delivery constraints.
2. Inspect exposed tools/capacity once. Parallelize independent reads/checks or
   reviews with available inputs/slots; keep dependent work ordered.
   Use files.find/search and bounded files.read ranges.
3. Build a small COMPLETE runnable flow with the reference composition. Preserve
   work; never restart after conflict/compaction.
4. Exercise the real entry point against the planned criteria. For visual work,
   compare an early screenshot with the reference: composition, proportions,
   typography, colors and controls. Check content meaning too.
   Browser.check has its own host runtime; missing Chromium/jsdom in process.run
   does not imply it is unavailable. Calls start fresh; use short scenarios.
5. Classify failures: application, check, invocation or environment. Use error
   locations/failed_action; make one concrete correction and rerun its check.
   Correct broken checks with verification.replaces. Avoid speculative edits.
6. Record plan evidence; keep unmet criteria open. Recheck changes or concrete
   gaps only. Deliver when satisfied; report unverified criteria.

Use process.run verification:{kind:behavior,criteria:[planned criteria],
paths:[source/test paths]}; syntax/inspection alone proves no behavior.
Edit with current SHA-256 and unique exact anchors; insert complete sections.
Obey limits, reuse hashes/cursors and reread conflicts. Use process.log/wait for
jobs. Request permission through actual tools, not interaction.ask; explain
/permissions. No commits, pushes, installs or broader access without host authority.
