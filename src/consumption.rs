//! Historical average for display only; never seeds the live next-drop predictor.
use crate::{
    history::is_confirmed_quota_change,
    model::{HistoryEvent, HistoryKind, WindowView},
};

pub fn historical_rate(
    history: &[HistoryEvent],
    view: &WindowView,
    account: Option<&str>,
) -> Option<f64> {
    let mut rows: Vec<_> = history
        .iter()
        .filter(|e| {
            e.source_key == view.source_key
                && e.window_key.as_deref() == Some(view.window_key.as_str())
                && e.observed_at_unix <= view.observed_at
                && is_confirmed_quota_change(e)
                && e.evidence.as_ref().and_then(|p| p.account_id.as_deref()) == account
        })
        .collect();
    rows.sort_by_key(|e| e.observed_at_unix);
    let reset = rows.iter().rposition(|e| e.kind == HistoryKind::QuotaReset);
    let rows = &rows[reset.map_or(0, |i| i + 1)..];
    let mut recent = Vec::new();
    for e in rows.iter().rev().take(11) {
        if e.evidence
            .as_ref()
            .and_then(|p| p.reset_at)
            .zip(view.reset_at)
            .is_some_and(|(a, b)| a.abs_diff(b) > 90)
        {
            break;
        }
        recent.push(*e);
    }
    let (latest, earliest) = (*recent.first()?, *recent.last()?);
    if latest.observed_at_unix <= earliest.observed_at_unix {
        return None;
    }
    let mut points = 0.;
    for pair in recent.windows(2) {
        let newer_before: f64 = pair[0].before.parse().ok()?;
        let older_after: f64 = pair[1].after.parse().ok()?;
        let newer_after: f64 = pair[0].after.parse().ok()?;
        // Never join overlapping, rebounding or missing percentage boundaries.
        if (newer_before - older_after).abs() > 0.000001 {
            return None;
        }
        points += newer_before - newer_after;
    }
    let rate = points * 3600. / (latest.observed_at_unix - earliest.observed_at_unix) as f64;
    (rate.is_finite() && rate > 0.).then_some(rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::QuotaModel;
    fn fixtures() -> (Vec<HistoryEvent>, WindowView) {
        let mut model = QuotaModel::default();
        for (t, used) in [(100, 20), (700, 21), (1300, 22), (1900, 23)] {
            model.ingest_json(&serde_json::json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":used,"windowDurationMins":10080,"resetsAt":99999}}}), t).unwrap();
        }
        let view = model.views(1900).remove(0);
        (model.history, view)
    }
    #[test]
    fn sparse_history_still_provides_display_average() {
        let (history, view) = fixtures();
        assert!(view.rate_per_hour.is_none());
        assert_eq!(historical_rate(&history, &view, None), Some(6.));
    }
    #[test]
    fn repeated_polls_refresh_liveness_without_adding_history() {
        let mut model = QuotaModel::default();
        let snapshot = serde_json::json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":99999}}});
        for t in (100..=400).step_by(10) {
            model.ingest_json(&snapshot, t).unwrap();
        }
        assert!(model.history.is_empty());
        assert_eq!(model.views(400)[0].observed_at, 400);
        let changed = serde_json::json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":21,"windowDurationMins":10080,"resetsAt":99999}}});
        model.ingest_json(&changed, 410).unwrap();
        model.ingest_json(&changed, 420).unwrap();
        assert_eq!(model.history.len(), 1);
        assert_eq!(model.history[0].observed_at_unix, 410);
    }
    #[test]
    fn no_crossing_reset_account_window_or_missing_boundary() {
        let (mut history, mut view) = fixtures();
        assert_eq!(historical_rate(&history, &view, Some("other")), None);
        view.window_key = "other".into();
        assert_eq!(historical_rate(&history, &view, None), None);
        view.window_key = history[0].window_key.clone().unwrap();
        history[1].after = "70".into();
        assert_eq!(historical_rate(&history, &view, None), None);
        let (mut history, view) = fixtures();
        history[1].kind = HistoryKind::QuotaReset;
        history[1].before = "1".into();
        history[1].after = "100".into();
        assert_eq!(historical_rate(&history, &view, None), None);
    }
}
