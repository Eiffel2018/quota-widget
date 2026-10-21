# Quota Widget 0.3.8 — relevant subagent tree

- The live Task tree now starts with recorded usage within the past two hours or the current quota interval, plus only the ancestors needed to connect those Tasks.
- Historical siblings and descendants no longer reappear merely because their parent is visible. Expanded and collapsed groups use the same relevant descendant count and subtotal.
- Older Tasks that contributed to the current quota interval remain, including recorded zero and unknown costs. Historical usage and parent relationships are retained for accounting and detail views.
- The child count is a display count, not a count of running Codex agent slots. This patch does not close, archive, delete or modify any Codex session.

Close the old application, extract the ZIP, and run quota-widget.exe. Preserve existing settings/history. No data reset or new migration is required.

Windows x64 is cross-built and unsigned; native Windows runtime is untested. Native Linux validation uses synthetic records. No Mac binary is included.
