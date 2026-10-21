# Quota Widget 0.3.14

Each quota decrease now gets its own estimate: the priced Token cost recorded
inside that interval divided by the observed percentage-point decrease. A
multi-point decrease uses the same interval formula, so its USD total remains
the observed subtotal and its per-point value is that subtotal divided by the
number of points. This retains real variation between intervals instead of
repeating one pooled average across every row.

Intervals with Token coverage gaps display the known subtotal as a lower bound.
Missing Token events, unknown model prices, assumed model prices or unreliable
quota boundaries remain unknown. The history header separately shows the
Token-cost-weighted average of estimable intervals in the selected list.

The estimate is based on locally recorded Token events and configured model
prices. It is not an official conversion rate for subscription quota; quota
polling can also occur after the actual server-side deduction time. Raw Token
subtotals and quota observations remain unchanged.

No data migration or reset is required. Restart the fixed-path executable to
load this version. Source and executable packages exclude private usage and
settings.
