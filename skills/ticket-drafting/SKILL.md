---
name: ticket-drafting
description: Turn meeting notes or requirements into structured ticket drafts with acceptance criteria and traceable sources. Use for planning work and ticket previews.
metadata:
  werk-worker-version: "1"
---
Read approved notes with files.read. Separate facts, proposed work and missing
information. Produce title, context, description, acceptance_criteria and source
for every draft. Use interaction.ask if a required destination is unknown.
Write a preview with files.write when permitted. Submit through an explicitly
registered connector only after approval. Never invent user or project IDs.
Example: two meeting decisions become two drafts with checkable criteria.
