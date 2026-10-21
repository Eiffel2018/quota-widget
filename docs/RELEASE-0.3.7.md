# Quota Widget 0.3.7 — readable, compact UI

- All text uses at least the main quota-rate label's size (18 logical points at the minimum window width), including tooltips, tables, settings, buttons and disabled controls.
- Dark and light palettes use opaque high-contrast text. Automated checks require at least 7:1 contrast for semantic foreground colors against every UI fill.
- Removed explanatory paragraphs and “不可比較” from the UI. Retained concise labels, numbers, actions and necessary error states. Partial/unknown data remain visibly labelled; the underlying coverage reasons and exports are unchanged.
- Tooltips wrap within the native window and scroll vertically when needed. Task tooltips retain Input, Cached, Uncached, Output, Reasoning, Turn Input and Turn Output, plus task/model identifiers.
- Increased row/control spacing and minimum width. The task table grows to the configured visible-row threshold (default 10) and scrolls sooner if the display cannot fit it. Detail tables support horizontal scrolling rather than overlapping numbers.

No accounting, import, quota normalization, pricing or storage migration changes are included in this patch. The screenshot alone cannot identify the exact reasons behind five partial historical intervals; fresh interval export would be required for that account-specific diagnosis.

## Install

Close the old app, then extract and run quota-widget.exe. Preserve your existing settings and history. No reset/re-import is required by this UI patch.

## Verification limits

Windows x64 is cross-built and unsigned; native Windows runtime has not been tested. Linux native UI was checked using synthetic data. No Mac binary is included. Existing 0.3.6 CLI initial-failure cause remains unverified.
