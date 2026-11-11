//! The review page: a plan's entries grouped by category, with the user's
//! selection. Only plan entries can be selected; report-only rows never.

use std::collections::BTreeSet;

use super::{category_label, category_order};
use crate::fmt::human_bytes;
use crate::object::Object;
use crate::plan::{Entry, EntryKind, Plan, Skip};

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub bytes: u64,
    pub reason: String,
    pub note: Option<String>,
    pub selected: bool,
    pub permanent: bool,
    /// A store rule, tool operation or Nix closure: the size is a plan-time estimate.
    pub estimate: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub category: String,
    pub label: String,
    /// Some row in the group deletes permanently.
    pub permanent: bool,
    pub rows: Vec<Row>,
    pub skipped: Vec<Skip>,
    /// Never selected.
    pub report_only: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Confirm {
    pub heading: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct ReviewModel {
    plan: Plan,
    selected: BTreeSet<String>,
}

fn row(e: &Entry, selected: bool) -> Row {
    Row {
        id: e.id.clone(),
        title: e.title.clone(),
        bytes: e.bytes,
        reason: e.reason.clone(),
        note: e.note.clone(),
        selected,
        permanent: e.permanent,
        estimate: e.is_estimate(),
    }
}

fn in_filter(category: &str, filter: Option<&str>) -> bool {
    filter.is_none_or(|f| f == category)
}

impl ReviewModel {
    /// Starts with the plan's own selection.
    pub fn new(plan: Plan) -> ReviewModel {
        let selected = plan.entries.iter().filter(|e| e.selected).map(|e| e.id.clone()).collect();
        ReviewModel { plan, selected }
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Non-empty groups in category order (skip-only categories last);
    /// `filter` keeps one category.
    pub fn groups(&self, filter: Option<&str>) -> Vec<Group> {
        let mut order = category_order(&self.plan);
        for s in &self.plan.skipped {
            if !order.contains(&s.category) {
                order.push(s.category.clone());
            }
        }
        order
            .into_iter()
            .filter(|c| in_filter(c, filter))
            .filter_map(|category| {
                let rows: Vec<Row> = self
                    .plan
                    .entries
                    .iter()
                    .filter(|e| e.category == category)
                    .map(|e| row(e, self.selected.contains(&e.id)))
                    .collect();
                let skipped: Vec<Skip> =
                    self.plan.skipped.iter().filter(|s| s.category == category).cloned().collect();
                let report_only: Vec<Row> = self
                    .plan
                    .report_only
                    .iter()
                    .filter(|e| e.category == category)
                    .map(|e| row(e, false))
                    .collect();
                if rows.is_empty() && skipped.is_empty() && report_only.is_empty() {
                    return None;
                }
                Some(Group {
                    label: category_label(&category),
                    permanent: rows.iter().any(|r| r.permanent),
                    category,
                    rows,
                    skipped,
                    report_only,
                })
            })
            .collect()
    }

    /// Selects or deselects one plan entry; other ids are ignored.
    pub fn set(&mut self, id: &str, on: bool) {
        if !self.plan.entries.iter().any(|e| e.id == id) {
            return;
        }
        if on {
            self.selected.insert(id.to_string());
        } else {
            self.selected.remove(id);
        }
    }

    /// Selects or deselects every plan entry of `category`.
    pub fn set_category(&mut self, category: &str, on: bool) {
        let ids: Vec<String> =
            self.plan.entries.iter().filter(|e| e.category == category).map(|e| e.id.clone()).collect();
        for id in ids {
            self.set(&id, on);
        }
    }

    fn chosen(&self, filter: Option<&str>) -> impl Iterator<Item = &Entry> {
        self.plan
            .entries
            .iter()
            .filter(move |e| self.selected.contains(&e.id) && in_filter(&e.category, filter))
    }

    /// Selected entry ids in plan order.
    pub fn selected_ids(&self, filter: Option<&str>) -> Vec<String> {
        self.chosen(filter).map(|e| e.id.clone()).collect()
    }

    pub fn selected_bytes(&self, filter: Option<&str>) -> u64 {
        self.chosen(filter).map(|e| e.bytes).sum()
    }

    /// "Clean selected ({bytes})"
    pub fn clean_label(&self, filter: Option<&str>) -> String {
        format!("Clean selected ({})", human_bytes(self.selected_bytes(filter)))
    }

    /// The confirmation dialog's text for the current selection.
    pub fn confirm(&self, filter: Option<&str>) -> Confirm {
        let n = self.chosen(filter).count();
        let rule = self.chosen(filter).any(|e| matches!(e.kind, EntryKind::StoreRule { .. }));
        let op = self.chosen(filter).any(|e| matches!(e.kind, EntryKind::Operation { .. }));
        let closure = self.chosen(filter).any(Entry::is_closure_estimate);
        let root = self.chosen(filter).any(|e| {
            matches!(e.kind, EntryKind::Object { obj: Object::NixGenerations { root: true, .. } })
        });
        // A closure estimate is never stated as an exact size (D5).
        let mut body = format!(
            "{}{} will be deleted now. This cannot be undone.",
            if closure { "About " } else { "" },
            human_bytes(self.selected_bytes(filter))
        );
        if rule {
            body.push_str(" Cache rule sizes are estimates.");
        }
        match (op, closure) {
            (true, false) => body.push_str(" Tool operation sizes are estimates."),
            (true, true) => body.push_str(" Tool operation and Nix closure sizes are estimates."),
            (false, true) => body.push_str(" Nix closure sizes are estimates."),
            (false, false) => {}
        }
        if root {
            body.push_str(" You'll be asked for your password for the system generations.");
        }
        Confirm { heading: format!("Delete {n} items?"), body }
    }
}
