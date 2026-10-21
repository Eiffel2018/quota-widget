//! Read an interval export locally; print only the newest shared estimate.
use quota_widget::{intervals::CompletedInterval, quota_estimate::interval_estimates};
use serde::Deserialize;

#[derive(Deserialize)]
struct IntervalFile {
    intervals: Vec<CompletedInterval>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("expected quota-intervals.json path")?;
    let file: IntervalFile = serde_json::from_reader(std::fs::File::open(path)?)?;
    let estimates = interval_estimates(&file.intervals);
    let newest = file
        .intervals
        .iter()
        .enumerate()
        .max_by_key(|(_, row)| row.end)
        .and_then(|(i, _)| estimates[i].as_ref());
    println!("{}", serde_json::to_string_pretty(&newest)?);
    Ok(())
}
