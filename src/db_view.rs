//! Bounded database-backed display requests. No raw usage history enters the GUI.
use crate::{
    accounting::ObservedCost,
    tokens::TokenSummary,
    usage_views::{GroupBy, TaskRow, UsageGroup},
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRequest {
    /// Inclusive UTC epoch for a Hong Kong calendar day; end is exclusive.
    pub day_start: i64,
    pub day_end: i64,
    pub group: GroupBy,
    pub excluded: bool,
    pub page: usize,
    pub page_size: usize,
    pub boundary: Option<i64>,
    pub fallback_start: i64,
    pub now: i64,
}
impl ViewRequest {
    pub fn same_selection(&self, other: &Self) -> bool {
        self.day_start == other.day_start
            && self.day_end == other.day_end
            && self.group == other.group
            && self.excluded == other.excluded
            && self.page == other.page
            && self.page_size == other.page_size
            && self.boundary == other.boundary
            && self.fallback_start == other.fallback_start
            && self.now / 60 == other.now / 60
    }
}
#[derive(Debug, Clone)]
pub struct DailyView {
    pub rows: Vec<UsageGroup>,
    pub summary: TokenSummary,
    pub group_count: usize,
}
#[derive(Debug, Clone)]
pub struct DatabaseView {
    pub request: ViewRequest,
    pub database_path: std::path::PathBuf,
    pub total_included: usize,
    pub total_excluded: usize,
    /// Hong Kong day starts expressed as canonical UTC epochs.
    pub day_starts: BTreeSet<i64>,
    pub models: BTreeSet<String>,
    pub main_tasks: Vec<TaskRow>,
    pub main_cost: ObservedCost,
    pub daily: DailyView,
}
