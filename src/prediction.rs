//! Robust, observation-based estimates. All rates are percentage points per hour.
//!
//! A drop is a measured change, not a token request. The first observed drop is
//! only an anchor: the unknown startup-to-first-drop interval is never fitted.
use serde::{Deserialize, Serialize};

pub const STALE_AFTER_SECONDS: i64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DropSample {
    pub observed_at: i64,
    pub delta_percent: f64,
    /// Maximum timing uncertainty of this observed step, from the last poll.
    pub poll_interval_seconds: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PredictionState {
    Estimate,
    Overdue,
    Insufficient,
    Stale,
    Unavailable,
    LowRemaining,
    ResetDue,
    ResetBeforeEstimate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    None,
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "無",
            Self::Low => "低",
            Self::Medium => "中",
            Self::High => "高",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Prediction {
    pub state: PredictionState,
    pub interval_seconds: Option<f64>,
    pub earliest_at: Option<i64>,
    pub latest_at: Option<i64>,
    pub estimated_at: Option<i64>,
    pub confidence: Confidence,
    /// Stable machine-readable reason codes, never fabricated measurements.
    pub reasons: Vec<String>,
    pub explanation: String,
    pub observed_drops: usize,
    pub measured_intervals: usize,
    pub rate_per_hour: Option<f64>,
}

impl Prediction {
    pub fn unavailable(
        state: PredictionState,
        reason: &str,
        explanation: &str,
        drops: usize,
    ) -> Self {
        Self {
            state,
            interval_seconds: None,
            earliest_at: None,
            latest_at: None,
            estimated_at: None,
            confidence: Confidence::None,
            reasons: vec![reason.into()],
            explanation: explanation.into(),
            observed_drops: drops,
            measured_intervals: 0,
            rate_per_hour: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PredictionContext {
    pub now: i64,
    pub observed_at: i64,
    pub remaining_percent: Option<f64>,
    pub reset_at: Option<i64>,
    pub eligible: bool,
}

/// Predict the next one percentage point of consumption. The range is an
/// empirical uncertainty range, NOT a calibrated statistical confidence interval.
/// Passing the point estimate keeps evidence visible as `overdue`; it never
/// silently advances the estimate into the future while the user is idle.
pub fn predict(drops: &[DropSample], context: &PredictionContext) -> Prediction {
    let unavailable = |state, reason, explanation| {
        Prediction::unavailable(state, reason, explanation, drops.len())
    };
    if !context.eligible {
        return unavailable(
            PredictionState::Unavailable,
            "unavailable_window",
            "目前來源或額度視窗不可用。",
        );
    }
    let age = context.now.checked_sub(context.observed_at);
    if context.now < 0 || context.observed_at < 0 || age.is_none_or(|age| age < 0) {
        return unavailable(
            PredictionState::Unavailable,
            "invalid_observation_time",
            "觀測時間異常，等待有效資料。",
        );
    }
    if age.unwrap() > STALE_AFTER_SECONDS {
        return unavailable(
            PredictionState::Stale,
            "stale_observation",
            "資料已超過 90 秒未更新，暫停預估。",
        );
    }
    if context.reset_at.is_some_and(|reset| reset <= context.now) {
        return unavailable(
            PredictionState::ResetDue,
            "reset_due",
            "已到伺服器提供的重設時間，等待實際更新。",
        );
    }
    let remaining = match context.remaining_percent {
        Some(value) if value.is_finite() && (0.0..=100.0).contains(&value) => value,
        _ => {
            return unavailable(
                PredictionState::Unavailable,
                "unknown_remaining",
                "尚無有效剩餘額度。",
            )
        }
    };
    if remaining < 1.0 {
        return unavailable(
            PredictionState::LowRemaining,
            "less_than_one_point",
            "剩餘額度不足 1 百分點。",
        );
    }
    if drops.len() < 2 {
        return unavailable(
            PredictionState::Insufficient,
            "need_two_drops",
            "需觀測至少 2 次下降；首次下降只建立基準。",
        );
    }
    let recent = &drops[drops.len().saturating_sub(10)..];
    if recent.iter().any(|drop| {
        drop.observed_at < 0
            || drop.observed_at > context.observed_at
            || !drop.delta_percent.is_finite()
            || drop.delta_percent <= 0.0
            || drop.poll_interval_seconds < 0
    }) {
        return unavailable(
            PredictionState::Unavailable,
            "invalid_drop_sample",
            "下降資料無效，等待新的連續觀測。",
        );
    }
    let mut intervals = Vec::with_capacity(recent.len() - 1);
    for (index, pair) in recent.windows(2).enumerate() {
        let Some(elapsed) = pair[1].observed_at.checked_sub(pair[0].observed_at) else {
            return unavailable(
                PredictionState::Unavailable,
                "invalid_drop_interval",
                "下降時間區間無效。",
            );
        };
        if elapsed <= 0 {
            return unavailable(
                PredictionState::Unavailable,
                "invalid_drop_interval",
                "下降時間必須嚴格遞增。",
            );
        }
        let seconds_per_point = elapsed as f64 / pair[1].delta_percent;
        if !seconds_per_point.is_finite() || seconds_per_point <= 0.0 {
            return unavailable(
                PredictionState::Unavailable,
                "invalid_drop_interval",
                "下降時間區間無效。",
            );
        }
        // New intervals receive more weight, while larger measured changes
        // modestly reduce percentage quantization noise. No single sample can
        // dominate solely because its drop is large.
        let recency = 2f64.powf(-((recent.len() - 2 - index) as f64) / 4.0);
        let weight = recency * pair[1].delta_percent.sqrt().clamp(0.5, 2.0);
        intervals.push((seconds_per_point.ln(), weight));
    }
    let center = weighted_quantile(&intervals, 0.5);
    let deviations: Vec<_> = intervals
        .iter()
        .map(|(value, weight)| ((value - center).abs(), *weight))
        .collect();
    let mad = weighted_quantile(&deviations, 0.5);
    let clip = (mad * 2.5).max(2f64.ln());
    let weight_sum: f64 = intervals.iter().map(|entry| entry.1).sum();
    let log_mean: f64 = intervals
        .iter()
        .map(|(value, weight)| value.clamp(center - clip, center + clip) * weight)
        .sum::<f64>()
        / weight_sum;
    let interval = log_mean.exp().max(0.001);
    let n = intervals.len();
    let base_uncertainty: f64 = match n {
        1 => 0.65,
        2 => 0.45,
        3..=4 => 0.30,
        _ => 0.20,
    };
    let last = recent.last().unwrap();
    let poll_uncertainty = last.poll_interval_seconds.max(1) as f64;
    let lower = (weighted_quantile(&intervals, 0.15)
        .exp()
        .min(interval * (1.0 - base_uncertainty))
        - poll_uncertainty)
        .max(1.0);
    let upper = weighted_quantile(&intervals, 0.85)
        .exp()
        .max(interval * (1.0 + base_uncertainty))
        + poll_uncertainty;
    let (Some(estimated_at), Some(earliest_at), Some(latest_at)) = (
        add_duration(last.observed_at, interval),
        add_duration(last.observed_at, lower),
        add_duration(last.observed_at, upper),
    ) else {
        return unavailable(
            PredictionState::Unavailable,
            "time_overflow",
            "預估時間超出可表示範圍。",
        );
    };
    let dispersion =
        (weighted_quantile(&intervals, 0.85) - weighted_quantile(&intervals, 0.15)).abs();
    let confidence = if n >= 6 && dispersion <= 0.4 && poll_uncertainty <= interval * 0.35 {
        Confidence::High
    } else if n >= 3 && dispersion <= 1.1 {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    let mut reasons = vec![
        "observed_usage_only".into(),
        "empirical_range_not_guarantee".into(),
    ];
    if n < 3 {
        reasons.push("few_measured_intervals".into());
    }
    if dispersion > 0.7 {
        reasons.push("variable_recent_usage".into());
    }
    if recent
        .iter()
        .any(|drop| (drop.delta_percent - 1.0).abs() > 0.000001)
    {
        reasons.push("multi_point_drops_normalized".into());
    }
    let mut result = Prediction {
        state: PredictionState::Estimate,
        interval_seconds: Some(interval),
        earliest_at: Some(earliest_at),
        latest_at: Some(latest_at),
        estimated_at: Some(estimated_at),
        confidence,
        reasons,
        explanation: "依近期實際下降估算下一個百分點；使用節奏變化時會偏離。".into(),
        observed_drops: drops.len(),
        measured_intervals: n,
        rate_per_hour: Some(3600.0 / interval),
    };
    if context.reset_at.is_some_and(|reset| estimated_at >= reset) {
        result.state = PredictionState::ResetBeforeEstimate;
        result.reasons.push("reset_precedes_estimate".into());
        result.explanation = "依目前速度，重設時間可能早於下一次 1 百分點下降。".into();
    } else if context.now >= estimated_at {
        result.state = PredictionState::Overdue;
        result.reasons.push("usage_may_have_paused".into());
        result.explanation = "已超過上次估算時間；可能暫停使用，保留區間並等待新下降。".into();
    }
    if context.reset_at.is_some_and(|reset| latest_at >= reset) {
        result.reasons.push("range_overlaps_reset".into());
    }
    result
}

fn weighted_quantile(values: &[(f64, f64)], quantile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.0.total_cmp(&right.0));
    let total: f64 = sorted.iter().map(|value| value.1).sum();
    let mut cumulative = 0.0;
    for (value, weight) in &sorted {
        cumulative += weight;
        if cumulative >= total * quantile {
            return *value;
        }
    }
    sorted.last().map(|value| value.0).unwrap_or_default()
}

fn add_duration(timestamp: i64, seconds: f64) -> Option<i64> {
    if !seconds.is_finite() || seconds < 0.0 || seconds.ceil() >= i64::MAX as f64 {
        return None;
    }
    // exp(ln(60)) may be 60.00000000000001. Avoid converting floating
    // roundoff into a spurious extra whole second, while rounding real
    // fractional-second estimates conservatively upward.
    let nearest = seconds.round();
    let rounded = if (seconds - nearest).abs() <= seconds.abs().max(1.0) * 1e-12 {
        nearest
    } else {
        seconds.ceil()
    };
    timestamp.checked_add(rounded as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn samples(times: &[i64], amount: f64) -> Vec<DropSample> {
        times
            .iter()
            .map(|&observed_at| DropSample {
                observed_at,
                delta_percent: amount,
                poll_interval_seconds: 2,
            })
            .collect()
    }
    fn context(now: i64) -> PredictionContext {
        PredictionContext {
            now,
            observed_at: now,
            remaining_percent: Some(90.0),
            reset_at: None,
            eligible: true,
        }
    }
    #[test]
    fn two_drops_are_enough_with_low_confidence() {
        let prediction = predict(&samples(&[100, 160], 1.0), &context(161));
        assert_eq!(prediction.state, PredictionState::Estimate);
        assert_eq!(prediction.confidence, Confidence::Low);
        assert!((prediction.interval_seconds.unwrap() - 60.0).abs() < 0.001);
        assert_eq!(prediction.estimated_at, Some(220));
        assert!(prediction.earliest_at.unwrap() < prediction.estimated_at.unwrap());
        assert!(prediction.latest_at.unwrap() > prediction.estimated_at.unwrap());
    }
    #[test]
    fn first_drop_never_fits_startup_interval() {
        assert_eq!(
            predict(&samples(&[160], 1.0), &context(161)).state,
            PredictionState::Insufficient
        );
    }
    #[test]
    fn multi_point_drops_are_normalized() {
        let prediction = predict(&samples(&[100, 160, 220], 2.0), &context(220));
        assert!((prediction.interval_seconds.unwrap() - 30.0).abs() < 0.001);
        assert!(prediction
            .reasons
            .iter()
            .any(|reason| reason == "multi_point_drops_normalized"));
    }
    #[test]
    fn uneven_usage_still_has_an_empirical_range() {
        let prediction = predict(&samples(&[100, 160, 300, 330, 450], 1.0), &context(450));
        assert_eq!(prediction.state, PredictionState::Estimate);
        assert!(prediction.latest_at.unwrap() - prediction.earliest_at.unwrap() > 60);
    }
    #[test]
    fn outlier_does_not_dominate_estimate() {
        let prediction = predict(
            &samples(&[100, 160, 220, 280, 340, 400, 10000, 10060], 1.0),
            &context(10060),
        );
        assert!(prediction.interval_seconds.unwrap() < 120.0);
    }
    #[test]
    fn retains_past_estimate_instead_of_rolling_it_forward() {
        let prediction = predict(&samples(&[100, 160], 1.0), &context(500));
        assert_eq!(prediction.state, PredictionState::Overdue);
        assert!(prediction.estimated_at.unwrap() <= 221);
        assert!(prediction.interval_seconds.is_some());
    }
    #[test]
    fn raw_observation_age_is_inclusive_at_ninety_seconds() {
        let mut context = context(250);
        context.observed_at = 160;
        assert_ne!(
            predict(&samples(&[100, 160], 1.0), &context).state,
            PredictionState::Stale
        );
        context.now = 251;
        assert_eq!(
            predict(&samples(&[100, 160], 1.0), &context).state,
            PredictionState::Stale
        );
    }
    #[test]
    fn low_remaining_unavailable_and_reset_reasons_are_distinct() {
        let drops = samples(&[100, 160], 1.0);
        let mut input = context(160);
        input.remaining_percent = Some(0.9);
        assert_eq!(predict(&drops, &input).state, PredictionState::LowRemaining);
        input.remaining_percent = None;
        assert_eq!(predict(&drops, &input).state, PredictionState::Unavailable);
        input.remaining_percent = Some(10.0);
        input.reset_at = Some(160);
        assert_eq!(predict(&drops, &input).state, PredictionState::ResetDue);
        input.reset_at = Some(200);
        assert_eq!(
            predict(&drops, &input).state,
            PredictionState::ResetBeforeEstimate
        );
    }
    #[test]
    fn short_intervals_work_and_bad_timestamps_do_not() {
        assert_eq!(
            predict(&samples(&[100, 103], 1.0), &context(103)).state,
            PredictionState::Estimate
        );
        for times in [
            &[100, 100][..],
            &[100, 99][..],
            &[-1, 100][..],
            &[100, 200][..],
        ] {
            assert_eq!(
                predict(&samples(times, 1.0), &context(160)).state,
                PredictionState::Unavailable
            );
        }
    }
    #[test]
    fn invalid_float_samples_cannot_produce_json_nan() {
        for amount in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let output = predict(&samples(&[100, 160], amount), &context(160));
            assert_eq!(output.state, PredictionState::Unavailable);
            assert!(serde_json::to_string(&output).is_ok());
        }
    }
    #[test]
    fn higher_evidence_yields_higher_confidence() {
        let prediction = predict(
            &samples(&[100, 160, 220, 280, 340, 400, 460, 520], 1.0),
            &context(520),
        );
        assert_eq!(prediction.confidence, Confidence::High);
    }
    #[test]
    fn time_overflow_is_reported() {
        let prediction = predict(
            &samples(&[i64::MAX - 20, i64::MAX - 10], 0.1),
            &context(i64::MAX - 10),
        );
        assert_eq!(prediction.state, PredictionState::Unavailable);
        assert_eq!(prediction.reasons, ["time_overflow"]);
    }
}
