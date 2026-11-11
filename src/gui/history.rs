//! The history page: past runs from the log, newest first.

use adw::prelude::*;

use crate::fmt::human_bytes;
use crate::view::category_label;
use crate::view::history::RunSummary;

pub struct HistoryPage {
    page: adw::NavigationPage,
}

impl HistoryPage {
    /// One expandable row per run with its items, or "No runs yet".
    pub fn new(runs: &[RunSummary]) -> HistoryPage {
        let content: gtk::Widget = if runs.is_empty() {
            adw::StatusPage::builder()
                .title("No runs yet")
                .description("Runs appear here after something is cleaned.")
                .icon_name("document-open-recent-symbolic")
                .vexpand(true)
                .build()
                .upcast()
        } else {
            let group = adw::PreferencesGroup::builder().margin_start(12).margin_end(12).build();
            for run in runs {
                let row = adw::ExpanderRow::builder()
                    .title(&run.when)
                    .subtitle(format!(
                        "deleted {} · {} deleted, {} skipped, {} failed",
                        human_bytes(run.deleted_bytes),
                        run.deleted,
                        run.skipped,
                        run.failed
                    ))
                    .use_markup(false)
                    .build();
                for item in &run.items {
                    let mut title = format!(
                        "{}  {}  [{}] {}",
                        item.outcome,
                        human_bytes(item.bytes),
                        category_label(&item.category),
                        item.title
                    );
                    if !item.detail.is_empty() {
                        title.push_str(&format!(" ({})", item.detail));
                    }
                    row.add_row(&adw::ActionRow::builder().title(title).use_markup(false).build());
                }
                group.add(&row);
            }
            let clamp = adw::Clamp::builder()
                .maximum_size(860)
                .margin_top(24)
                .margin_bottom(24)
                .child(&group)
                .build();
            gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&clamp)
                .vexpand(true)
                .build()
                .upcast()
        };
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));
        let page = adw::NavigationPage::builder().title("History").tag("history").child(&toolbar).build();
        HistoryPage { page }
    }

    pub fn page(&self) -> adw::NavigationPage {
        self.page.clone()
    }
}
