---
name: coding-basic
description: Read project instructions, make a small verified code change and report its diff. Use when a user requests repository work.
metadata:
  werk-worker-version: "1"
---
Read AGENTS.md; preserve user work. Use files.find/search, then files.read ranges.
Edit with current SHA-256 and a unique exact anchor; insert complete sections.
Reuse hashes/cursors, obey size limits and reread conflicts. Never restart after
conflict/compaction.
Request permission via actual tools, not interaction.ask; explain /permissions.
Execute work rather than return code to paste.
Check syntax AND behavior via process.run/browser.check with
verification:{criteria:[requirements exercised],paths:[relative source/test paths]}.
Changes invalidate checks; listings, echo and syntax alone prove no behavior.
Exercise real startup/restart, input handlers and boundaries, not state shortcuts.
Prefer exposed browser.check: its host runtime is independent of Chromium/jsdom
in process.run. Calls start fresh; use short click/key scenarios within limits.
Inspect failed_action, target state and controls; repair wrong tests through
verification.replaces, not working application behavior. Without browser support,
use a DOM/canvas harness exercising actual handlers.
Before building, identify reference composition, sizes, colors and controls.
Compare with the first check's screenshot when vision is available; fix layout
defects. Text assertions do not prove appearance.
Batch independent reads/checks; order dependencies. Recheck relevant changes or
concrete gaps. Keep plan steps open until behavior and visuals are verified,
then deliver. Use process.log/wait for background jobs. Delegate narrow independent
work with precise context; reviewer opinions cannot replace executable checks.
Report results and unverified behavior. No commits, pushes, package installs or
broader access without host authority.
Example: fix a subtraction accidentally used in add, then run its unit test.
