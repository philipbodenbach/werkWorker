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
Use process.run to check syntax AND behavior with assertions; fix failures.
For interactive applications, exercise the real startup/restart and input
handlers. Do not substitute direct internal-state changes for broken entry
points. Check relevant state/data invariants and representative boundary cases.
For browser work, use an available browser or DOM/canvas harness that dispatches
the actual handlers; report when that check cannot be performed.
Report the diff, actual exit codes and any unverified behavior.
Do not commit, push, install packages or broaden access without host authority.
Example: fix a subtraction accidentally used in add, then run its unit test.
