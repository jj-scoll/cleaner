//! The dashboard: filesystem bars, the latest-plan banner, categories and
//! the last run.

use jiff::tz::TimeZone;

use super::{Level, category_label, category_order, level_for};
use crate::disk::FsUsage;
use crate::fmt::{human_bytes, local_time};
use crate::log::LogLine;
use crate::plan::Plan;

#[derive(Clone, Debug, PartialEq)]
pub struct FsBar {
    pub label: String,
    pub used_fraction: f64,
    pub level: Level,
    /// "{avail} free of {total}"
    pub free_text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Banner {
    /// No plan has been built yet.
    NoPlan,
    /// The latest plan selects nothing, or it has been applied.
    Nothing,
    /// The latest plan has selected bytes and hasn't been applied.
    Ready { plan_id: String, title: String, subtitle: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CategoryRow {
    pub id: String,
    pub label: String,
    pub selected_bytes: u64,
    pub selected_count: usize,
    pub report_only_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dashboard {
    pub filesystems: Vec<FsBar>,
    pub banner: Banner,
    /// Categories of the latest plan with at least one entry or report-only row.
    pub categories: Vec<CategoryRow>,
    /// "Last run {when} · deleted {bytes}" for the newest run.
    pub last_run: Option<String>,
}

pub fn dashboard(fs: &[FsUsage], latest: Option<&Plan>, log: &[LogLine], tz: &TimeZone) -> Dashboard {
    let filesystems = fs
        .iter()
        .map(|f| {
            let used = f.total.saturating_sub(f.avail);
            let used_fraction = if f.total == 0 { 0.0 } else { used as f64 / f.total as f64 };
            FsBar {
                label: f.label.clone(),
                used_fraction,
                level: level_for(used_fraction),
                free_text: format!("{} free of {}", human_bytes(f.avail), human_bytes(f.total)),
            }
        })
        .collect();

    let banner = match latest {
        None => Banner::NoPlan,
        Some(p) if p.selected_bytes() > 0 && !log.iter().any(|l| l.plan_id == p.id) => Banner::Ready {
            plan_id: p.id.clone(),
            title: format!("New plan ready: {} can be freed", human_bytes(p.selected_bytes())),
            subtitle: format!("Built {} · nothing has been touched yet", local_time(p.created, tz)),
        },
        Some(_) => Banner::Nothing,
    };

    let categories = match latest {
        None => Vec::new(),
        Some(p) => category_order(p)
            .into_iter()
            .filter_map(|id| {
                let entries: Vec<_> = p.entries.iter().filter(|e| e.category == id).collect();
                let report_only_count = p.report_only.iter().filter(|e| e.category == id).count();
                if entries.is_empty() && report_only_count == 0 {
                    return None;
                }
                let selected: Vec<_> = entries.iter().filter(|e| e.selected).collect();
                Some(CategoryRow {
                    label: category_label(&id),
                    selected_bytes: selected.iter().map(|e| e.bytes).sum(),
                    selected_count: selected.len(),
                    report_only_count,
                    id,
                })
            })
            .collect(),
    };

    let last_run = super::history::history(log, tz)
        .first()
        .map(|r| format!("Last run {} · deleted {}", r.when, human_bytes(r.deleted_bytes)));

    Dashboard { filesystems, banner, categories, last_run }
}
