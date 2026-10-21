//! Read-only presentation of canonical completed quota intervals.
//!
//! These helpers never select token events, recompute prices, split observed
//! decrements, or change exported accounting. The table and latest-five tooltip
//! can share the same strings and ordering without maintaining another ledger.
use crate::{
    display_time,
    intervals::{CompletedInterval, StartBoundaryKind},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntervalPresentation {
    pub change: String,
    pub start: String,
    pub end: String,
    pub duration: String,
    pub total: String,
    pub per_point: String,
    pub status: String,
    pub tooltip: String,
}

fn number(value: f64) -> String {
    if value.is_finite() {
        value.to_string()
    } else {
        "未知".into()
    }
}

fn money(value: f64) -> String {
    if !value.is_finite() || value < 0.0 {
        return "$未知".into();
    }
    // Known zero remains zero, including a possible negative floating-point zero.
    let value = if value == 0.0 { 0.0 } else { value };
    if value > 0.0 && value < 0.0001 {
        format!("${value:.6}")
    } else {
        format!("${value:.4}")
    }
}

fn elapsed(seconds: i64) -> String {
    if seconds >= 86_400 {
        format!(
            "{}d {:02}:{:02}:{:02}",
            seconds / 86_400,
            seconds / 3_600 % 24,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3_600,
            seconds / 60 % 60,
            seconds % 60
        )
    }
}

/// Format one real completed interval without modifying its canonical data.
/// Per-point cost is only displayed from the supplied complete, comparable value.
pub fn present(interval: &CompletedInterval) -> IntervalPresentation {
    let start_time = interval.start.and_then(display_time::datetime);
    let end_time = display_time::datetime(interval.end);
    let seconds = interval
        .start
        .and_then(|start| interval.end.checked_sub(start))
        .filter(|seconds| *seconds > 0 && start_time.is_some() && end_time.is_some());
    let start = start_time.unwrap_or_else(|| "未知".into());
    let end = end_time.unwrap_or_else(|| "未知".into());
    let duration = seconds.map(elapsed).unwrap_or_else(|| "未知".into());
    let change = format!("{}% → {}%", number(interval.before), number(interval.after));
    let valid_decrement = interval.before.is_finite()
        && interval.after.is_finite()
        && interval.before > interval.after
        && interval.percentage_points.is_finite()
        && interval.percentage_points > 0.0;
    let complete = interval.comparable
        && !interval.partial
        && interval.start_boundary_kind != StartBoundaryKind::QuotaReset
        && seconds.is_some()
        && valid_decrement;
    let ratio = interval
        .cost_per_point
        .filter(|value| complete && value.is_finite() && *value >= 0.0);
    let total = format!(
        "{}{}",
        money(interval.known_usd),
        if complete { "" } else { " *" }
    );
    let averaged = interval.percentage_points > 1.0;
    let per_point = match ratio {
        Some(value) if averaged => format!("平均 {}/百分點", money(value)),
        Some(value) => format!("{}/百分點", money(value)),
        None => "未知".into(),
    };
    let status = if !complete {
        "部分"
    } else if ratio.is_none() {
        "未知"
    } else if averaged {
        "平均"
    } else {
        "完整"
    }
    .to_owned();
    let tooltip = format!(
        "額度視窗：{}\n額度變化：{}\n下降：{} 百分點\n起點：{}\n終點：{}\n歷時：{}\n已知小計：{}\n每百分點：{}\n狀態：{}\n記錄來源：{}\n觀察工作階段：{}",
        interval.window_label,
        change,
        number(interval.percentage_points),
        start,
        end,
        duration,
        total,
        per_point,
        status,
        interval.source_key,
        interval.session
    );
    IntervalPresentation {
        change,
        start,
        end,
        duration,
        total,
        per_point,
        status,
        tooltip,
    }
}

/// Indices of every interval, newest end first, then stable event key ascending.
/// Duplicate keys retain source order. Callers may take five for a preview; this
/// helper never truncates, deduplicates, splits, or reorders the source records.
pub fn newest_indices(intervals: &[CompletedInterval]) -> Vec<usize> {
    let mut indices: Vec<_> = (0..intervals.len()).collect();
    indices.sort_by(|&a, &b| {
        intervals[b]
            .end
            .cmp(&intervals[a].end)
            .then_with(|| {
                intervals[a]
                    .history_event_key
                    .cmp(&intervals[b].history_event_key)
            })
            .then_with(|| a.cmp(&b))
    });
    indices
}
