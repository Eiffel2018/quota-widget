# 0.3.0 rebuild

This is newly reconstructed, maintainable source based on recoverable 0.2.1 assets, schema, screenshots and selected reverse-engineered behavior. It is not the lost original source tree.

Fixed: metadata-only resets, 0→0 false history entries, fragile all-1pp/±25% prediction suppression, seven-day automatic history deletion, duplicate cumulative token notifications, lost per-session offsets, ambiguous costs treated as zero, mixed-window cost boundaries, same-second double assignment, stale/corrupt-state overwrites and topmost reassertion/focus behavior.

Added: a scrollable per-Task widget table with latest event time/cost and task-only current quota-interval subtotal, official response-scoped TokenUsageRecord import, explicit Response/Turn/Task daily grouping, conservative mixed-schema overlap handling, 30-day daily navigation with older available history retained, local JSONL importer, recorded/assumed model attribution, editable official Standard/Fast price presets, per-request long-context adjustment, separate daily quota and Token views, CSV/JSON export, cost-since-real-boundary, weighted cost-per-percentage-point, guarded persistence and structured read-only CLI.

Current platform status: Windows x64 cross-built; Linux native-tested; macOS arm64 code-checked but no linked app. The full source, tests and Mac build instructions are included.
