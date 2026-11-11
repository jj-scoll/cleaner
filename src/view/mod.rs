//! What the window shows, as plain data (spec §7.2). No GTK here, so
//! ordinary tests check it; `src/gui` only renders these models.

pub mod dashboard;
pub mod history;
pub mod review;
pub mod usage;

use crate::config::Config;
use crate::engine::registry;
use crate::plan::Plan;

/// The display name of a category id; unknown ids are shown as-is.
pub fn category_label(id: &str) -> String {
    match id {
        "artifacts" => "Project artifacts",
        "caches" => "Tool caches",
        "user_rules" => "Folder rules",
        "trash" => "Trash",
        "idle_folders" => "Large and old",
        "worktrees" => "Finished worktrees",
        "app_caches" => "Browser and app caches",
        "docker" => "Docker",
        "nix" => "Nix",
        "scan_roots" => "Scan roots",
        "disk_map" => "Disk map",
        other => other,
    }
    .to_string()
}

/// Category ids in display order, followed by any others the plan mentions.
pub fn category_order(plan: &Plan) -> Vec<String> {
    let mut order: Vec<String> =
        registry(&Config::default()).iter().map(|c| c.id().to_string()).collect();
    let ids = plan
        .entries
        .iter()
        .chain(&plan.report_only)
        .map(|e| &e.category)
        .chain(plan.skipped.iter().map(|s| &s.category));
    for id in ids {
        if !order.contains(id) {
            order.push(id.clone());
        }
    }
    order
}

/// How full a filesystem is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    Ok,
    High,
    Critical,
}

/// < 75% used is `Ok`, < 90% `High`, otherwise `Critical`.
pub fn level_for(used_fraction: f64) -> Level {
    if used_fraction < 0.75 {
        Level::Ok
    } else if used_fraction < 0.90 {
        Level::High
    } else {
        Level::Critical
    }
}

impl Level {
    /// The libadwaita style class for this level.
    pub fn css_class(self) -> &'static str {
        match self {
            Level::Ok => "success",
            Level::High => "warning",
            Level::Critical => "error",
        }
    }
}
