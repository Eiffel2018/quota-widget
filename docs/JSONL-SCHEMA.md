# Codex usage schema and grouping

Verified against official OpenAI sources on 2026-10-17:

- https://github.com/openai/codex/blob/main/codex-rs/protocol/src/protocol.rs
- https://github.com/openai/codex/blob/main/codex-rs/core/src/state/session.rs

The current `token_usage_record` payload contains `thread_id`, `turn_id`, `session_id`, `root_turn_id`, `response_id`, `usage`, `turn_token_usage` and `thread_token_usage`. The app treats `usage` as the response contribution. `turn_token_usage` and `thread_token_usage` are cumulative snapshots, useful for reconciliation and detecting missing coverage, not additional billable events.

A Response is identified by `(thread_id, response_id)`. Replayed records count once; updated records replace the response contribution. Different response IDs remain distinct even when their token counts match. A Turn is `(thread_id, turn_id)`. A Task is the logical `thread_id`; the root `session_id` never merges separate child tasks. Missing turn IDs stay visibly unknown.

Legacy `token_count` cumulative-delta import remains supported. With mixed formats, exact response identity or comparable monotonic cumulative ranges can remove overlap. If overlap cannot be established, response records take precedence and the result is marked incomplete. No discarded legacy portion is claimed to be zero. Per-response price can remain known while aggregate task/turn coverage is incomplete.

Input means the full request context, including cached input, not only newly typed text. Cached input is a subset of input. Reasoning is a subset of output. Pricing uses uncached input, cached input and total output once. Long-context adjustments apply to individual response input, never to a grouped daily total.

The widget's Task rows show the latest event time and cost, plus that Task's observed cost after the selected quota window's latest real decrease/reset. The interval is `(boundary, now]`. Recent tasks remain visible with zero new observed cost after a boundary. Without a boundary, the interval cost is unknown. The overall footer uses the same boundary. Unknown coverage or price is never shown as a complete total.

Daily Response/Turn/Task views and JSON summaries use only normalized events in the selected local day. They do not claim whole-turn or lifetime totals when a turn/task crosses days. CSV retains IDs and source kind for independent analysis.

Store v1 upgrades to v2 by retaining history and replaying explicitly selected import roots once; old byte offsets cannot hide previously ignored response records. Future unsupported versions are rejected without overwriting them.
