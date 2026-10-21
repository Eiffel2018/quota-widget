//! Read-only presentation of completed and explicitly partial quota intervals.
//!
//! Partial recovered rows are presentation-only; they do not change canonical
//! accounting or exported data.
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
    pub observed_total: String,
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
        observed_total: total.clone(),
        total,
        per_point,
        status,
        tooltip,
    }
}

/// Shared quota estimate for display; keep the observed token subtotal separate.
pub fn present_with_estimate(
    interval: &CompletedInterval,
    estimate: Option<&crate::quota_estimate::QuotaEstimate>,
) -> IntervalPresentation {
    let mut view = present(interval);
    let marker = if estimate.is_some_and(|value| value.lower_bound) {
        " ≥"
    } else {
        " ≈"
    };
    view.total = estimate
        .map(|value| format!("{marker} {}", money(value.total(interval))))
        .unwrap_or_else(|| "未知".into());
    view.per_point = estimate
        .map(|value| format!("{marker} {}/百分點", money(value.usd_per_point)))
        .unwrap_or_else(|| "未知".into());
    view.status = match estimate {
        Some(value) if value.lower_bound => "本段估算 · 下限",
        Some(_) => "本段估算",
        None => "未有足夠區間 Token 資料",
    }
    .into();
    if interval.events == 0 && interval.known_usd == 0.0 {
        view.observed_total = "未有 Token 記錄".into();
    }
    if interval.partial {
        view.status.push_str(" · 部分");
    }
    view.tooltip.push_str(&format!(
        "\n本段 Token 費用估算：{}\n本段 USD / 百分點：{}\n估算狀態：{}",
        view.total, view.per_point, view.status
    ));
    if let Some(value) = estimate {
        view.tooltip.push_str(&format!(
            "\n本段有價 Token 成本：${:.4} ÷ {} 百分點\n算法：本段有價 Token 成本 ÷ 本段百分點\n估值期間：{} 至 {}\n估值用途：根據本段記錄估算，並非官方固定美元換算率",
            value.known_usd,
            value.percentage_points,
            display_time::datetime(value.start).unwrap_or_default(),
            display_time::datetime(value.end).unwrap_or_default()
        ));
    }
    view
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

/// Show an actually logged decrease without claiming a confirmed cost boundary.
pub fn unconfirmed_observation(event: &crate::model::HistoryEvent) -> IntervalPresentation {
    IntervalPresentation {
        change: format!("{}% → {}%", event.before, event.after),
        start: "—".into(), end: display_time::datetime(event.observed_at_unix).unwrap_or_else(|| "未知".into()),
        duration: "—".into(), total: "待確認".into(), observed_total: "未能確認歸屬".into(),
        per_point: "未知".into(), status: "日誌補回 · 未確認".into(),
        tooltip: "日誌記錄的額度下降；帳戶或週期邊界未能確認。時間為日誌觀察時間，未計入扣額或單價校準，原始 Token 記錄保留。".into(),
    }
}

/// Present a consecutive historical observation with the token subtotal in its
/// bounded time window. Imported quota evidence remains explicitly approximate.
pub fn recovered_observation(
    event: &crate::model::HistoryEvent,
    start: i64,
    cost: crate::accounting::ObservedCost,
) -> IntervalPresentation {
    let seconds = event.observed_at_unix.saturating_sub(start);
    let points = event
        .before
        .parse::<f64>()
        .ok()
        .zip(event.after.parse::<f64>().ok())
        .map(|(before, after)| (before - after).abs());
    let has_tokens = cost.events > 0;
    let lower_bound = cost.unknown_events > 0 || cost.partial;
    let amount = format!(
        "{}{}",
        if lower_bound { "≥ " } else { "≈ " },
        money(cost.known_usd)
    );
    let per_point = has_tokens
        .then(|| {
            points
                .filter(|points| *points > 0.0)
                .map(|points| {
                    format!(
                        "{}{} / 百分點",
                        if lower_bound { "≥ " } else { "≈ " },
                        money(cost.known_usd / points)
                    )
                })
                .unwrap_or_else(|| "未知".into())
        })
        .unwrap_or_else(|| "未知".into());
    let (total, observed_total, status, tooltip) = if !has_tokens {
        (
            "未知".into(),
            "無 Token 記錄".into(),
            "區間沒有 Token 記錄".into(),
            "已按相鄰百分比變化重建時間區間，但本機沒有該區間的 Token 記錄，因此無法估算費用。"
                .into(),
        )
    } else {
        (
            amount.clone(),
            amount,
            "歷史 Token 估算 · 額度部分未確認".into(),
            "按相鄰歷史百分比下降時間統計本機 Token 紀錄；額度快照的帳戶／連續性未完全確認，金額只作估算。".into(),
        )
    };
    IntervalPresentation {
        change: format!("{}% → {}%", event.before, event.after),
        start: display_time::datetime(start).unwrap_or_else(|| "未知".into()),
        end: display_time::datetime(event.observed_at_unix).unwrap_or_else(|| "未知".into()),
        duration: elapsed(seconds),
        total,
        observed_total,
        per_point,
        status,
        tooltip,
    }
}
