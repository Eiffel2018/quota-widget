# Quota Widget 0.3.13

After restarting, recovered log observations could disappear from quota history.
Uncertain observations from different reset deadlines shared a duplicate-pair
set, hiding later logged decreases. The preview and debit table also skipped
uncertain observations entirely.

Uncertain anonymous observations are now deduplicated within their recorded
reset period, with up to 90 seconds of deadline jitter grouped together. The
conservative accounting anchor and confirmed-reset rules are unchanged.
Recovered observations appear in the recent-five preview and debit table as
unconfirmed log records, without invented USD costs or accounting boundaries.
Expired old-period snapshots remain excluded after a fresh period is observed.

The recent-five preview and debit table prefer a nearby direct observation over
an anonymous log echo only when matching window, endpoints and reset deadline
identify one recorded account. Ambiguous account matches remain visible. Raw
observations, accounting intervals and the all-observations view are preserved.

Synthetic regressions cover exhausted prior periods, newly visible uncertain
observations, expired snapshots and ambiguous account matches. Timestamps remain
observation times; missing percentages, dates or Token costs are not fabricated.

Restart the fixed installed executable. No data reset or database migration is
needed. Source and executable packages exclude private usage and settings.
