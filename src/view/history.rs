//! The history page: the run log grouped by run.

use jiff::tz::TimeZone;

use crate::fmt::local_time;
use crate::log::LogLine;

#[derive(Clone, Debug, PartialEq)]
pub struct RunSummary {
    pub run_id: String,
    /// Local time of the run's first line.
    pub when: String,
    pub plan_id: String,
    pub deleted_bytes: u64,
    pub deleted: usize,
    pub skipped: usize,
    pub failed: usize,
    /// The run's lines, in log order.
    pub items: Vec<LogLine>,
}

/// One summary per run, newest run first.
pub fn history(log: &[LogLine], tz: &TimeZone) -> Vec<RunSummary> {
    let mut runs: Vec<RunSummary> = Vec::new();
    for line in log {
        let i = match runs.iter().position(|r| r.run_id == line.run_id) {
            Some(i) => i,
            None => {
                runs.push(RunSummary {
                    run_id: line.run_id.clone(),
                    when: local_time(line.time, tz),
                    plan_id: line.plan_id.clone(),
                    deleted_bytes: 0,
                    deleted: 0,
                    skipped: 0,
                    failed: 0,
                    items: Vec::new(),
                });
                runs.len() - 1
            }
        };
        let run = &mut runs[i];
        run.deleted_bytes += line.bytes;
        match line.outcome.as_str() {
            "deleted" => run.deleted += 1,
            "skipped" => run.skipped += 1,
            "failed" => run.failed += 1,
            _ => {}
        }
        run.items.push(line.clone());
    }
    runs.reverse();
    runs
}
