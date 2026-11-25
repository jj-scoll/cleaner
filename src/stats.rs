//! Cumulative space freed, from the run log. `aggregate` is a pure fold over
//! parsed records; `from_log` streams the log file into it.
//!
//! A run's freed bytes are *measured* when the log has its `RunLine` (free
//! space after minus before, summed over filesystems, floored at 0), else
//! *estimated*: the sum of its deleted entries' `bytes`. Both are kept per
//! run and in the totals, since a measurement can miss filesystems it does
//! not watch. Per-category figures only exist per entry, so they are always
//! estimates.

use anyhow::Context;
use jiff::tz::TimeZone;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use crate::log::Record;

/// How a run's freed bytes were obtained.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Basis {
    /// statvfs: free space after minus before the run.
    Measured,
    /// The sum of the sizes reported for the run's deleted items.
    Estimated,
}

/// Freed bytes over a period: `bytes = measured + estimate_only`.
#[derive(Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Total {
    /// The headline: each run's measurement, or its estimate without one.
    pub bytes: u64,
    /// The measurements of the runs that have one.
    pub measured: u64,
    /// Every run's estimate, measured or not.
    pub estimated: u64,
    /// The estimates of the runs without a measurement.
    pub estimate_only: u64,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct RunFreed {
    pub run_id: String,
    /// The run's earliest log time (Unix seconds).
    pub time: i64,
    /// `measured` if the run has a measurement, else `estimated`.
    pub bytes: u64,
    pub basis: Basis,
    /// Free space after minus before, floored at 0 (`None`: not measured).
    pub measured: Option<u64>,
    /// The sum of the run's deleted entries' sizes.
    pub estimated: u64,
}

/// Deleted entries' bytes for one category (always an estimate).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct CategoryFreed {
    pub category: String,
    pub bytes: u64,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Records before this time (Unix seconds) were ignored.
    pub since: Option<i64>,
    /// Distinct runs with at least one record at or after `since`.
    pub runs: usize,
    pub all_time: Total,
    /// Runs since January 1st, 00:00 local time.
    pub this_year: Total,
    /// Runs in the 30 days up to `now`.
    pub last_30_days: Total,
    pub biggest_run: Option<RunFreed>,
    /// Every run in the window, oldest first (then by id).
    pub per_run: Vec<RunFreed>,
    /// Largest first, then by id; categories that freed 0 bytes are left out.
    pub categories: Vec<CategoryFreed>,
}

/// Folds `records` into totals as of `now` in `tz`, ignoring records before
/// `since`.
pub fn aggregate(
    records: impl IntoIterator<Item = Record>,
    now: i64,
    tz: &TimeZone,
    since: Option<i64>,
) -> Stats {
    // Memory is per run and per category, not per line.
    let mut runs: HashMap<String, RunAcc> = HashMap::new();
    let mut categories: HashMap<String, u64> = HashMap::new();
    for rec in records {
        let (run_id, time) = match &rec {
            Record::Entry(l) => (&l.run_id, l.time),
            Record::Run(r) => (&r.run_id, r.time),
        };
        if since.is_some_and(|s| time < s) {
            continue;
        }
        // Not `entry(run_id.clone())`: that allocates on every line.
        if !runs.contains_key(run_id) {
            runs.insert(
                run_id.clone(),
                RunAcc {
                    time,
                    estimated: 0,
                    measured: None,
                },
            );
        }
        let Some(acc) = runs.get_mut(run_id) else {
            unreachable!("inserted above")
        };
        acc.time = acc.time.min(time);
        match &rec {
            Record::Entry(l) if l.outcome == "deleted" => {
                acc.estimated = acc.estimated.saturating_add(l.bytes);
                match categories.get_mut(&l.category) {
                    Some(b) => *b = b.saturating_add(l.bytes),
                    None => {
                        categories.insert(l.category.clone(), l.bytes);
                    }
                }
            }
            Record::Entry(_) => {}
            Record::Run(r) => {
                let delta = r
                    .freed_by_fs
                    .iter()
                    .fold(0i64, |s, f| s.saturating_add(f.bytes));
                acc.measured = Some(acc.measured.unwrap_or(0).saturating_add(delta));
            }
        }
    }

    let year_start = year_start(now, tz);
    let month_start = now.saturating_sub(30 * 86_400);
    let mut stats = Stats {
        since,
        runs: runs.len(),
        ..Stats::default()
    };
    for (run_id, acc) in runs {
        let measured = acc.measured.map(|m| m.max(0) as u64);
        let (bytes, basis) = match measured {
            Some(m) => (m, Basis::Measured),
            None => (acc.estimated, Basis::Estimated),
        };
        let run = RunFreed {
            run_id,
            time: acc.time,
            bytes,
            basis,
            measured,
            estimated: acc.estimated,
        };
        stats.all_time.add(&run);
        if acc.time >= year_start {
            stats.this_year.add(&run);
        }
        if acc.time >= month_start {
            stats.last_30_days.add(&run);
        }
        // Largest, then earliest, then by id: independent of HashMap order.
        let beats = |b: &RunFreed| {
            (std::cmp::Reverse(bytes), acc.time, &run.run_id)
                < (std::cmp::Reverse(b.bytes), b.time, &b.run_id)
        };
        if bytes > 0 && stats.biggest_run.as_ref().is_none_or(beats) {
            stats.biggest_run = Some(run.clone());
        }
        stats.per_run.push(run);
    }
    stats
        .per_run
        .sort_by(|a, b| (a.time, &a.run_id).cmp(&(b.time, &b.run_id)));
    stats.categories = categories
        .into_iter()
        .filter(|(_, b)| *b > 0)
        .map(|(category, bytes)| CategoryFreed { category, bytes })
        .collect();
    stats.categories.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.category.cmp(&b.category))
    });
    stats
}

struct RunAcc {
    /// Earliest record time.
    time: i64,
    /// Sum of deleted entries' bytes.
    estimated: u64,
    /// Sum of the run line's deltas, if the log has one.
    measured: Option<i64>,
}

impl Total {
    fn add(&mut self, run: &RunFreed) {
        self.bytes = self.bytes.saturating_add(run.bytes);
        self.estimated = self.estimated.saturating_add(run.estimated);
        match run.measured {
            Some(m) => self.measured = self.measured.saturating_add(m),
            None => self.estimate_only = self.estimate_only.saturating_add(run.estimated),
        }
    }

    /// Whether the measured runs' measurements and estimates are more than
    /// 20% apart (of the measurement), so the estimate is worth showing.
    pub fn estimate_differs(&self) -> bool {
        let measured_runs_estimate = self.estimated.saturating_sub(self.estimate_only);
        differs(self.measured, measured_runs_estimate)
    }
}

impl RunFreed {
    /// Whether the run's measurement and estimate are more than 20% apart.
    pub fn estimate_differs(&self) -> bool {
        self.measured.is_some_and(|m| differs(m, self.estimated))
    }
}

/// `estimated` is more than 20% of `measured` away from it.
fn differs(measured: u64, estimated: u64) -> bool {
    u128::from(measured.abs_diff(estimated)) * 5 > u128::from(measured)
}

/// January 1st, 00:00 of `now`'s year in `tz`, as Unix seconds.
fn year_start(now: i64, tz: &TimeZone) -> i64 {
    jiff::Timestamp::from_second(now)
        .ok()
        .and_then(|t| {
            let first = t.to_zoned(tz.clone()).date().first_of_year();
            first.to_zoned(tz.clone()).ok()
        })
        .map_or(i64::MIN, |z| z.timestamp().as_second())
}

/// `aggregate` over the log at `path`, streamed. A missing log gives empty stats.
pub fn from_log(path: &Path, now: i64, tz: &TimeZone, since: Option<i64>) -> anyhow::Result<Stats> {
    let mut err = None;
    let records = crate::log::records(path)?.map_while(|r| r.map_err(|e| err = Some(e)).ok());
    let stats = aggregate(records, now, tz, since);
    match err {
        Some(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        None => Ok(stats),
    }
}

/// "YYYY-MM-DD" as the Unix time of that day's local midnight in `tz`.
pub fn parse_since(s: &str, tz: &TimeZone) -> anyhow::Result<i64> {
    let date = jiff::civil::Date::strptime("%Y-%m-%d", s)
        .with_context(|| format!("expected a date as YYYY-MM-DD, got {s:?}"))?;
    let midnight = date
        .to_zoned(tz.clone())
        .with_context(|| format!("{s} has no midnight in this time zone"))?;
    Ok(midnight.timestamp().as_second())
}
