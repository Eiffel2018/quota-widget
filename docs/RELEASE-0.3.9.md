# Quota Widget 0.3.9

Every one-point deduction within the same uninterrupted observation chain now
uses one shared historical USD estimate. Previously the table displayed the sum
of response costs completed between two detected quota changes, which varied
from row to row and was easy to mistake for the price of a percentage point.

The shared rate is `sum(known token USD) / sum(observed percentage-point drops)`.
For example, synthetic one-point samples costing $12, $17, $13, $14 and $14
produce the same $14 estimate for every one-point row. A three-point deduction
uses three times the shared rate. Estimates update together when records change;
changing the selected date does not recalibrate a chain from that day's subset.

At least three contiguous intervals spanning five percentage points are needed.
Chains do not cross account, source, window, observer session, epoch, reset,
missing-link or invalid-boundary changes. Unpriced or assumed-model events are
excluded and break the chain. Token-coverage-only partial samples can provide a
clearly marked lower-bound estimate (`≥`), rather than keeping every estimate
blank forever. This lower bound describes the recorded historical sample under
the configured token prices; it does not guarantee future cost or full coverage.

The original observed subtotal is retained separately as “區間 Token 小計 USD”.
No token events, model prices, quota observations, interval subtotals or original
export fields are rewritten. Table rows, history labels, model details and the
latest-five tooltip use the same calibrated presentation. Daily CSV/JSON exports
add `quota_estimated_usd`, `quota_usd_per_point`, `quota_estimate` (sample size,
period and lower-bound flag), and `quota_estimate_method`. Their original ledger
fields retain their previous meanings. Estimates are historical proxies, not an
official fixed conversion or a subscription invoice.

Official quota documentation reports window percentages and reset times; it
does not provide a fixed USD-per-percent field:
[App Server](https://learn.chatgpt.com/docs/app-server),
[pricing](https://learn.chatgpt.com/docs/pricing).

The Windows importer also supports ImDisk RAM drives whose DOS canonical-path
query returns `ERROR_INVALID_FUNCTION`. It resolves the existing file through
its NT device handle and a Win32 GLOBALROOT path, preserving real-path identity
and existing symlink exclusions. Missing or inaccessible paths still fail. No
RAM-drive remount, filesystem change or application-data migration is needed.
[Microsoft documents the Mount Manager limitation and NT path option](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfinalpathnamebyhandlew).

Close the old application before starting the new executable. Existing settings,
history and SQLite state are retained. Keep useful source/releases in durable,
named folders; disposable build and test files can live on a RAM drive.
