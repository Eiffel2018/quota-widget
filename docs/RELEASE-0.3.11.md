# Quota Widget 0.3.11

New quota drops previously showed an unknown estimate after an observer restart
or a token import gap, even with a usable historical calibration. A read-only
instance also stayed read-only after the previous writer exited.

- Keep the existing contiguous ratio-of-sums calibration. Missing or zero-event
  intervals do not contribute artificial zero costs to it.
- Carry an existing calibration forward for a known matching account, source
  and window when quota endpoints agree. Reset boundaries, changed epochs in
  the same observer, ambiguous identity, broken endpoints and a calibration
  older than 24 hours prevent reuse. The detail view identifies historical reuse.
- A read-only database worker periodically releases its own shared reader lock
  before trying exclusive ownership. Once acquired, reload the authoritative
  database, merge observations made while read-only and resume local imports.
  Other live writers retain exclusive ownership.
- A zero-event, zero-subtotal interval now says it has no Token records rather
  than presenting the subtotal as measured zero usage.
- Repeated fixed-path deployments preserve a backup still mapped by a running
  older instance and use a fresh rollback file for the next update.
- The debit table has five columns: quota change, start, end, duration and USD
  estimate. Click a row for calibration, raw Token subtotals and model details.
  Per-point pricing appears once above the table.

Original observations, token costs and exports remain available. No database
schema changes, history resets or private data are part of this release.
Restart the fixed installed executable once to load this version.
