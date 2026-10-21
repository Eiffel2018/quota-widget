# Codex usage schema and grouping

Verified against official OpenAI sources on 2026-10-17:

- https://github.com/openai/codex/blob/main/codex-rs/protocol/src/protocol.rs
- https://github.com/openai/codex/blob/main/codex-rs/core/src/state/session.rs

The current `token_usage_record` payload contains `thread_id`, `turn_id`, `session_id`, `root_turn_id`, `response_id`, `usage`, `turn_token_usage` and `thread_token_usage`. The app treats `usage` as the response contribution. `turn_token_usage` and `thread_token_usage` are cumulative snapshots, useful for reconciliation and detecting missing coverage, not additional billable events.

A Response is identified by `(thread_id, response_id)`. Replayed records count once; updated records replace the response contribution. Different response IDs remain distinct even when their token counts match. A Turn is `(thread_id, turn_id)`. A Task is the logical `thread_id`; the root `session_id` never merges separate child tasks. Missing turn IDs stay visibly unknown.

Legacy `token_count` cumulative-delta import remains supported. With mixed formats, exact response identity or comparable monotonic cumulative ranges can remove overlap. If overlap cannot be established, response records take precedence and the result is marked incomplete. No discarded legacy portion is claimed to be zero. Per-response price can remain known while aggregate task/turn coverage is incomplete.

Input means the full request context, including cached input, not only newly typed text. Cached input is a subset of input. Reasoning is a subset of output. Pricing uses uncached input, cached input and total output once. Long-context adjustments apply to individual response input, never to a grouped daily total.

The widget's Task rows show the latest event time and cost, plus that Task's observed cost after the selected quota window's latest real decrease/reset. The interval is `(boundary, now]`. Recent tasks remain visible with zero new observed cost after a boundary. Without a boundary, the known subtotal starts at zero at application startup and accumulates new recorded usage, explicitly marked partial. The overall footer uses the same boundary. Unknown coverage or price is never shown as a complete total.

Daily Response/Turn/Task views and JSON summaries use only normalized events in the selected local day. They do not claim whole-turn or lifetime totals when a turn/task crosses days. CSV retains IDs and source kind for independent analysis.

Store v1/v2 upgrades to v3 by retaining history and replaying explicitly selected import roots once; old byte offsets cannot hide previously ignored response records. Future unsupported versions are rejected without overwriting them.

## Names, parents and tooltip counters

Official names are read from explicit session/thread metadata and the optional adjacent `session_index.jsonl` (`id`, `thread_name`, `updated_at`). Arbitrary message text is never used as a title. Immediate parent relationships are read only from `parent_thread_id` on SessionMeta/SessionConfigured, or `source.subagent.thread_spawn.parent_thread_id`. Root/session IDs, `forked_from_id`, paths and matching names are not parenting evidence. Conflicting parent evidence remains unknown. Cycles are detached for display, while evidence remains stored.

Tree summaries count every known descendant of a visible root, including metadata-only descendants, once. Parent self usage is separate. Metadata-only nodes have no invented response time/count/cost; known subtotal can be zero with a partial flag. Unrelated historical components are not shown. Expansion only changes presentation, never accounting.

Tooltip Response counters retain missing-field flags. Missing input/cache/output prevents exact pricing; missing reasoning alone does not invalidate the inclusive output price. Turn input/output use the latest matching recorded turn snapshot, not a sum of snapshots. When only retained responses can be aggregated, the tooltip explicitly labels it partial; unavailable fields show unknown.

## Historical quota reconciliation

Raw observation events remain persisted separately from their normalized presentation. Source/window/account identity, log milliseconds, session origin, and before/after reset metadata are retained. Replayed stale decreases are not repeated accounting boundaries. Unsupported historical increases are uncertainty markers, not assumed refills; live confirmed increases and reset-deadline rollover evidence remain real boundaries.

## Automatic review policy (0.3.2)

A derived `excluded_recorded_codex_auto_review` marker records exact original model identity before any override. The raw normalized store keeps these events. Default snapshots, range indexes, grouping, task trees and exports omit them; explicitly selected audit exports retain them and declare `excluded_from_default_totals`. CLI JSON retains a separate `excluded_auto_review_events` list. Mixed-model tasks retain ordinary events and their recorded prices. A pure review legacy initial aggregate is excluded with a scope-uncertain flag; mixed-model aggregate gaps retain an unpriced residual rather than misassigning historic tokens. Matching Turn tooltips use included responses when a cumulative snapshot mixes included/excluded events, marked partial. No inferred free billing or quota adjustment is made.
