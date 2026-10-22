---
name: coding-basic
description: Read project instructions, make a small verified code change and report its diff. Use when a user requests repository work.
metadata:
  werk-worker-version: "1"
---
Read relevant AGENTS.md and files using files.read. Preserve existing user
changes. Use files.edit with the observed SHA-256 and one exact match; do not
guess patch bases. Request approval by calling the actual files.write,
files.edit or process.run tool; the engine pauses it for host confirmation.
Do not replace an implementation request with code in chat or ask for rights
using interaction.ask. If tools are restricted, explain the /permissions option.
Create new files in small working increments. Use files.edit for existing files;
never restart a whole implementation after a conflict or compaction. On an edit
conflict, read the relevant lines and choose an exact, unique anchor.
Use process.run to check syntax AND behavior with assertions; fix failures.
Supply verification:{criteria:[requirements exercised],paths:[relevant source
and test files relative to workspace]} on the checking call. The host records
the exit status and file hashes; edits invalidate old evidence. Do not use an
echo, file listing, or syntax-only check as proof of functional behavior.
Keep task.plan accurate: unfinished implementation and verification stay open.
Continue implementing, checking, and repairing until the requirements pass.
For independent work use delegate_task with precise context and narrow tools;
a reviewer's opinion does not replace the executable checks.
For interactive applications, exercise the real startup/restart and input
handlers. Do not substitute direct internal-state changes for broken entry
points. Check relevant state/data invariants and representative boundary cases.
For browser work, use an available browser or DOM/canvas harness that dispatches
the actual handlers; report when that check cannot be performed.
Report the diff, actual exit codes and any unverified behavior.
Do not commit, push, install packages or broaden access without host authority.
Example: fix a subtraction accidentally used in add, then run its unit test.
