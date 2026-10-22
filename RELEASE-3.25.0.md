# v3.25

- Per-model usage counts now count distinct Codex turns instead of collapsing an entire conversation to one.
- The daily percentage table fits the available window width so Total remains visible.
- Daily results are calculated once while the history window is open; background polling no longer restarts the calculation. Closing and reopening refreshes it.
