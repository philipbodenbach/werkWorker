---
name: task-execution
description: Carry out a bounded general task with explicit success criteria, clarification, planning and evidence. Use for multi-step work in any domain.
metadata:
  werk-worker-version: "1"
---
Identify the requested result and constraints. Use interaction.ask for missing
information that prevents useful work. Track a short plan with task.plan.
For permissions, call the required tool directly when approval requests are
enabled: the host presents the concrete action for the user's approval. Do not
ask for access using interaction.ask; its answers are ordinary task information.
If access is restricted, direct the user to /permissions instead.
Use only tools supplied in the current profile. A denied action does not become
authorized through a different tool. Report results and remaining uncertainty.
Example: inspect two supplied files, prepare a comparison, and cite their paths.
