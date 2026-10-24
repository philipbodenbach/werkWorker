---
name: coding-basic
description: Read project instructions, make a small verified code change and report its diff. Use when a user requests repository work.
metadata:
  werk-worker-version: "1"
---
Read relevant AGENTS.md. Find paths with files.find, symbols with files.search,
then read matching ranges with files.read. Preserve existing user work.
Edit with the current SHA-256 and one exact unique anchor. Add complete sections
with files.insert when available. Reuse returned hashes and pagination cursors.
Follow current size guidance; never restart after conflict or compaction.
On conflict, reread the relevant range and fix the exact anchor.
Request permissions by calling the actual tool: the host presents its approval.
Do not ask for rights via interaction.ask or substitute chat code for execution.
For restricted access explain /permissions.
Check syntax AND behavior using process.run or browser.check; fix failures.
Include verification:{criteria:[requirements exercised],paths:[source and test
files relative to workspace]}. The host records exit status and file hashes;
changes invalidate earlier checks. Listings, echo and syntax alone do not prove
functional behavior. Keep unfinished task.plan steps open and continue until
implementation and verification satisfy the request.
Exercise real startup/restart and input handlers plus relevant boundary cases;
do not bypass broken entry points by modifying internal state. Prefer
browser.check with clicks/keys and assertions; inspect screenshots when vision
is available. Otherwise use a DOM/canvas harness dispatching the actual handlers,
and report checks that could not run.
For background jobs read process.log and use process.wait instead of polling.
Delegate independent work with precise context and narrow tools; a reviewer's
opinion does not replace executable checks.
Report actual results and unverified behavior. Do not commit, push, install
packages or broaden access without host authority.
Example: fix a subtraction accidentally used in add, then run its unit test.
