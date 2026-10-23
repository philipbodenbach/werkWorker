# Context planning and Werk Protocol — 2026-10-23

This comparison concerns context maintenance, not model quality. Public docs were
read on the date above; upstream implementations and defaults can change. No
external harness source is copied. Claude Code's undocumented internals are not
inferred from its public behavior.

| Harness | Publicly documented mechanism | Useful consequence for Worker |
|---|---|---|
| [Claude Code](https://code.claude.com/docs/en/how-claude-code-works#when-context-fills-up) | Clear older tool outputs before summarizing; stop repeated ineffective compaction; subagents have their own context. | Preserve useful current work; avoid paying repeatedly for an ineffective maintenance pass. |
| [Codex](https://developers.openai.com/codex/config-reference/) | Automatic compaction threshold; the threshold scope can measure total context or growth after the carried compaction prefix. | Distinguish stable carried context from newly appended work. These docs do not establish an exact number of tokenizer calls. |
| [little-coder](https://github.com/itayinbarr/little-coder#troubleshooting) | Pi-based watchdog checks during long runs, defaults to 80%, and pauses ineffective repeated compaction. | Check at model boundaries and recognize maintenance without progress. Do not copy a large-window threshold blindly. |
| [DSH token meter](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/llm/token-meter/README.md) | Replayable local estimates, durable surface state and provider-usage anchors; estimates are explicitly distinct from exact counts. | Separate cheap candidate selection from actual request admission. |
| [Hermes](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/developer-guide/context-compression-and-caching.md) | Persisted usage anchor plus estimated appended-message delta; fingerprint validation, bounded compression recovery and local pruning. | Carry a validated working selection across turns/restarts; don't rescan the entire transcript through the remote tokenizer. |

## Observed failure

The supplied Werk server log contains 630 successful token-count requests taking
911.71 seconds in aggregate, versus six generations taking 75.56 seconds. Complete
planning intervals contain 115–117 counts before one generation. These are serial
planning costs, not context unloading. The default builder reconstructed every
candidate from raw history on every turn. It retained only optional model-summary
compactions, ignoring its own preceding deterministic selection. Large historical
reads each triggered another remote count, often followed by a summary-free count.

## Protocol boundary

The current protocol already provides the required foundation:

- `/werk/v1/chat/limits`: context per request, output ceiling and configured
  concurrency. Concurrency is not a token budget or a count of idle slots.
- `/werk/v1/chat/reasoning`: model/runtime tiers and effective budgets.
- `/werk/v1/chat/count_tokens`: complete chat-template-aware input counts,
  including tools/options. Unsupported image counting retains Worker's explicit
  native-text-plus-image-reserve fallback, never a falsely exact count.

Worker should reuse a **selection**, not trust a previous token total for a changed
request. The final composed request still goes through the existing provider
count/fallback and admission path, with output reserve and safety margin. No new
endpoint or server-side session ownership is needed for this fix.

A future optional batched count endpoint could amortize transport/template work,
but it would need request-size bounds, capability discovery and per-candidate
errors. It should follow measurements after removing the redundant client work.
Cross-request token caching additionally needs model/template/runtime identity;
adding counts of separately tokenized messages is not exact at template boundaries.

Keeping the system/tool prefix stable also helps backend prefix-cache reuse.
Changing old history can still invalidate the affected KV suffix; a faster
planning algorithm does not promise a KV-cache hit or faster token generation.
