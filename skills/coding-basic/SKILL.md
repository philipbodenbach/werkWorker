---
name: coding-basic
description: Read project instructions, make a small verified code change and report its diff. Use when a user requests repository work.
metadata:
  werk-worker-version: "1"
---
Read relevant AGENTS.md and files using files.read. Preserve existing user
changes. Use files.edit with the observed SHA-256 and one exact match; do not
guess patch bases. Ask for required approvals. Run the relevant test using
process.run inside the assigned sandbox. Report the diff and actual exit code.
Do not commit, push, install packages or broaden access without host authority.
Example: fix a subtraction accidentally used in add, then run its unit test.
