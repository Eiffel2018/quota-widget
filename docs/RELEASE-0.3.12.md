# Quota Widget 0.3.12

Includes the 0.3.11 estimate recovery, automatic writer-lock recovery, five-column
history table and repeated live-deployment backup fix.

A restarted observer may have missed quota drops while the previous window was
read-only. It can now reuse a valid historical calibration when its remaining
quota has decreased since that observer. The missing drops are not calibration
samples and missing Token costs are never added as zero usage. Matching known
account/source/window, reset/epoch guards and the 24-hour calibration limit still
apply. Historical reuse is identified in the details.

Restart the fixed installed executable once. No history reset is needed.
