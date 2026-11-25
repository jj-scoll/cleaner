//! The review TUI's state and its reducer: plain data, no terminal, so unit
//! tests drive it key by key. The selection lives in a `ReviewModel`, the
//! same one the GUI's review page uses, so the tickable rules are its rules.

use crate::fmt::human_bytes;
use crate::view::category_label;
use crate::view::review::{ReviewModel, Sort, row_line};

/// A key press, as the reducer sees it (see `super::key_from_event`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
    CtrlC,
    Char(char),
}

/// What the event loop should do after a key.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    None,
    /// Leave without deleting anything.
    Quit,
    /// The user confirmed: apply these plan entry ids.
    Apply(Vec<String>),
    /// The sort order changed; remember it as the GUI does.
    SortChanged(Sort),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    List,
    /// Typing a `/` filter.
    Filter,
    /// "Delete N items?"
    Confirm,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LineKind {
    Header,
    /// A plan entry: the only kind that can be ticked.
    Item {
        id: String,
        selected: bool,
    },
    /// Shown, never deleted.
    ReportOnly,
    /// Shown with its reason, never deleted.
    Skipped,
}

/// One line of the list.
#[derive(Clone, Debug, PartialEq)]
pub struct ListLine {
    /// Stable across sorting and filtering, so the cursor can follow a line.
    pub key: String,
    pub category: String,
    pub kind: LineKind,
    pub text: String,
    pub note: Option<String>,
}

pub struct App {
    model: ReviewModel,
    cursor: usize,
    filter: String,
    screen: Screen,
    message: Option<String>,
    /// The list's scroll offset, kept between frames by `render`.
    pub offset: usize,
}

impl App {
    pub fn new(model: ReviewModel) -> App {
        App {
            model,
            cursor: 0,
            filter: String::new(),
            screen: Screen::List,
            message: None,
            offset: 0,
        }
    }

    pub fn model(&self) -> &ReviewModel {
        &self.model
    }

    pub fn sort(&self) -> Sort {
        self.model.sort()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// A one-line note for the status bar (e.g. why a row can't be ticked).
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Shows `text` in the status line until the next key.
    pub fn set_message(&mut self, text: String) {
        self.message = Some(text);
    }

    /// The visible lines: each category's header, its plan entries, then
    /// its report-only and skipped rows, in the model's sort order.
    /// A category with no visible line is left out; the filter matches
    /// titles, ignoring case.
    pub fn lines(&self) -> Vec<ListLine> {
        let needle = self.filter.to_lowercase();
        let shown = |title: &str| needle.is_empty() || title.to_lowercase().contains(&needle);
        let mut out = Vec::new();
        for g in self.model.groups(None) {
            let line = |key: String, kind: LineKind, text: String, note: Option<String>| ListLine {
                key,
                category: g.category.clone(),
                kind,
                text,
                note,
            };
            let mut body = Vec::new();
            for r in g.rows.iter().filter(|r| shown(&r.title)) {
                let mark = if r.selected { "[x]" } else { "[ ]" };
                body.push(line(
                    format!("i:{}", r.id),
                    LineKind::Item {
                        id: r.id.clone(),
                        selected: r.selected,
                    },
                    row_line(mark, r),
                    r.note.clone(),
                ));
            }
            for r in g.report_only.iter().filter(|r| shown(&r.title)) {
                body.push(line(
                    format!("r:{}", r.id),
                    LineKind::ReportOnly,
                    format!("{} [report only]", row_line("   ", r)),
                    r.note.clone(),
                ));
            }
            for s in g.skipped.iter().filter(|s| shown(&s.title)) {
                body.push(line(
                    format!("s:{}:{}", g.category, s.title),
                    LineKind::Skipped,
                    format!("    ⊘ {}: {}", s.title, s.reason),
                    None,
                ));
            }
            if body.is_empty() {
                continue;
            }
            let on: Vec<_> = g.rows.iter().filter(|r| r.selected).collect();
            let perm = if g.permanent { " [permanent]" } else { "" };
            let header = if g.rows.is_empty() {
                format!("    {}{perm}", g.label)
            } else {
                let mark = match on.len() {
                    0 => "[ ]",
                    n if n == g.rows.len() => "[x]",
                    _ => "[~]",
                };
                let bytes: u64 = on.iter().map(|r| r.bytes).sum();
                format!("{mark} {} ({} selected){perm}", g.label, human_bytes(bytes))
            };
            out.push(line(
                format!("h:{}", g.category),
                LineKind::Header,
                header,
                None,
            ));
            out.append(&mut body);
        }
        out
    }

    /// The plan entry ids `Apply` would carry, in plan order.
    pub fn selected_ids(&self) -> Vec<String> {
        self.model.selected_ids(None)
    }

    /// How many selected entries the filter hides.
    pub fn hidden_selected(&self) -> usize {
        let visible = self
            .lines()
            .iter()
            .filter(|l| matches!(l.kind, LineKind::Item { selected: true, .. }))
            .count();
        self.selected_ids().len() - visible
    }

    pub fn handle(&mut self, key: Key) -> Action {
        self.message = None;
        if key == Key::CtrlC {
            return Action::Quit;
        }
        match self.screen {
            Screen::List => self.handle_list(key),
            Screen::Filter => {
                self.handle_filter(key);
                Action::None
            }
            Screen::Confirm => self.handle_confirm(key),
        }
    }

    fn handle_list(&mut self, key: Key) -> Action {
        let lines = self.lines();
        let current = lines.get(self.cursor).cloned();
        match key {
            Key::Up | Key::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                if self.cursor + 1 < lines.len() {
                    self.cursor += 1;
                }
            }
            Key::Char(' ') => {
                if let Some(l) = current {
                    self.toggle(&l, &lines);
                }
            }
            Key::Char('a') => {
                if let Some(l) = current {
                    self.toggle_category(&l.category, &lines);
                }
            }
            Key::Char('s') => {
                let sort = self.sort().next();
                self.model.set_sort(sort);
                self.follow(current.map(|l| l.key));
                return Action::SortChanged(sort);
            }
            Key::Char('/') => self.screen = Screen::Filter,
            Key::Enter => {
                if self.selected_ids().is_empty() {
                    self.message = Some("Nothing selected.".into());
                } else {
                    self.screen = Screen::Confirm;
                }
            }
            Key::Char('q') | Key::Esc => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    fn handle_filter(&mut self, key: Key) {
        let current = self.lines().get(self.cursor).map(|l| l.key.clone());
        match key {
            Key::Char(c) => self.filter.push(c),
            Key::Backspace => {
                self.filter.pop();
            }
            Key::Enter => self.screen = Screen::List,
            Key::Esc => {
                self.filter.clear();
                self.screen = Screen::List;
            }
            _ => {}
        }
        self.follow(current);
    }

    fn handle_confirm(&mut self, key: Key) -> Action {
        match key {
            Key::Char('y' | 'Y') => return Action::Apply(self.selected_ids()),
            Key::Char('n' | 'N' | 'q') | Key::Esc => self.screen = Screen::List,
            _ => {}
        }
        Action::None
    }

    /// Space: an entry flips, a header is `a`; the rest only say why not.
    fn toggle(&mut self, line: &ListLine, lines: &[ListLine]) {
        match &line.kind {
            LineKind::Item { id, selected } => self.model.set(id, !selected),
            LineKind::Header => self.toggle_category(&line.category, lines),
            LineKind::ReportOnly => {
                self.message = Some("Report only: cleaner never deletes this.".into());
            }
            LineKind::Skipped => {
                self.message = Some("Skipped: there is nothing here to delete.".into());
            }
        }
    }

    /// Selects every visible entry of `category`, or clears them all when
    /// they are all selected already (the GUI's group check button).
    fn toggle_category(&mut self, category: &str, lines: &[ListLine]) {
        let items: Vec<(&str, bool)> = lines
            .iter()
            .filter(|l| l.category == category)
            .filter_map(|l| match &l.kind {
                LineKind::Item { id, selected } => Some((id.as_str(), *selected)),
                _ => None,
            })
            .collect();
        if items.is_empty() {
            self.message = Some(format!(
                "Nothing to select in {}.",
                category_label(category)
            ));
            return;
        }
        let on = !items.iter().all(|&(_, selected)| selected);
        for (id, _) in items {
            self.model.set(id, on);
        }
    }

    /// Puts the cursor back on the line `key` names, or keeps it in range
    /// when that line is gone.
    fn follow(&mut self, key: Option<String>) {
        let lines = self.lines();
        match key.and_then(|k| lines.iter().position(|l| l.key == k)) {
            Some(i) => self.cursor = i,
            None => self.cursor = self.cursor.min(lines.len().saturating_sub(1)),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::plan::{Entry, EntryKind, Plan, Skip};
    use crate::view::review::entry_line;

    fn entry(id: &str, category: &str, title: &str, bytes: u64, selected: bool) -> Entry {
        Entry {
            id: id.into(),
            category: category.into(),
            title: title.into(),
            kind: EntryKind::Item { targets: vec![] },
            bytes,
            reason: "because".into(),
            selected,
            permanent: false,
            note: None,
            last_active: None,
        }
    }

    /// caches: pip (selected); trash: old.zip (selected), b (not);
    /// idle_folders: one report-only row; docker: one skip.
    pub(crate) fn sample_plan() -> Plan {
        Plan {
            id: "20261111T030000Z".into(),
            created: 1_794_366_000,
            entries: vec![
                entry("trash:0", "trash", "old.zip", 4096, true),
                entry("trash:1", "trash", "b", 1024, false),
                entry("caches:0", "caches", "pip", 2048, true),
            ],
            report_only: vec![entry(
                "idle_folders:0",
                "idle_folders",
                "Videos",
                9999,
                false,
            )],
            skipped: vec![Skip {
                category: "docker".into(),
                title: "docker".into(),
                reason: "daemon not running".into(),
            }],
        }
    }

    fn app() -> App {
        App::new(ReviewModel::new(sample_plan()))
    }

    fn keys(app: &App) -> Vec<String> {
        app.lines().into_iter().map(|l| l.key).collect()
    }

    fn at(app: &App) -> Option<String> {
        app.lines().get(app.cursor()).map(|l| l.key.clone())
    }

    /// Moves the cursor down from the top until it is on `key`.
    fn goto(app: &mut App, key: &str) {
        while app.cursor() > 0 {
            app.handle(Key::Up);
        }
        for _ in 0..50 {
            if at(app).as_deref() == Some(key) {
                return;
            }
            app.handle(Key::Down);
        }
        panic!("no line {key}; lines are {:?}", keys(app));
    }

    fn press(app: &mut App, text: &str) -> Action {
        let mut last = Action::None;
        for c in text.chars() {
            last = app.handle(Key::Char(c));
        }
        last
    }

    #[test]
    fn lines_group_like_show() {
        let a = app();
        assert_eq!(
            keys(&a),
            [
                "h:caches",
                "i:caches:0",
                "h:trash",
                "i:trash:0",
                "i:trash:1",
                "h:idle_folders",
                "r:idle_folders:0",
                "h:docker",
                "s:docker:docker",
            ]
        );
        let lines = a.lines();
        let plan = sample_plan();
        assert_eq!(lines[3].text, entry_line(&plan.entries[0], true));
        assert_eq!(lines[4].text, entry_line(&plan.entries[1], false));
        assert!(lines[2].text.contains("Trash"), "{}", lines[2].text);
        assert!(lines[6].text.contains("[report only]"), "{}", lines[6].text);
        assert!(lines[8].text.contains("daemon not running"));
    }

    #[test]
    fn arrows_and_jk_move_within_bounds() {
        let mut a = app();
        a.handle(Key::Up);
        assert_eq!(a.cursor(), 0);
        a.handle(Key::Down);
        press(&mut a, "j");
        assert_eq!(a.cursor(), 2);
        press(&mut a, "k");
        assert_eq!(a.cursor(), 1);
        for _ in 0..20 {
            a.handle(Key::Down);
        }
        assert_eq!(a.cursor(), 8, "stops on the last line");
    }

    #[test]
    fn space_toggles_an_item() {
        let mut a = app();
        goto(&mut a, "i:trash:1");
        press(&mut a, " ");
        assert_eq!(a.selected_ids(), ["trash:0", "trash:1", "caches:0"]);
        assert_eq!(
            a.lines()[a.cursor()].kind,
            LineKind::Item {
                id: "trash:1".into(),
                selected: true
            }
        );
        press(&mut a, " ");
        assert_eq!(a.selected_ids(), ["trash:0", "caches:0"]);
    }

    #[test]
    fn report_only_and_skipped_rows_are_untickable() {
        let mut a = app();
        let before = a.selected_ids();
        for key in ["r:idle_folders:0", "s:docker:docker", "h:idle_folders"] {
            goto(&mut a, key);
            press(&mut a, " ");
            assert_eq!(a.selected_ids(), before, "space on {key}");
            assert!(a.message().is_some(), "space on {key} explains why");
            press(&mut a, "a");
            assert_eq!(a.selected_ids(), before, "a on {key}");
        }
    }

    #[test]
    fn a_toggles_the_whole_category() {
        let mut a = app();
        goto(&mut a, "i:trash:0");
        press(&mut a, "a");
        assert_eq!(a.selected_ids(), ["trash:0", "trash:1", "caches:0"]);
        press(&mut a, "a");
        assert_eq!(a.selected_ids(), ["caches:0"], "all on, so a clears");
        goto(&mut a, "h:trash");
        press(&mut a, " ");
        assert_eq!(
            a.selected_ids(),
            ["trash:0", "trash:1", "caches:0"],
            "space on a header is a"
        );
    }

    #[test]
    fn s_cycles_the_sort_and_the_cursor_follows() {
        let mut a = app();
        goto(&mut a, "i:trash:1");
        assert_eq!(press(&mut a, "s"), Action::SortChanged(Sort::Name));
        assert_eq!(a.sort(), Sort::Name);
        assert_eq!(at(&a).as_deref(), Some("i:trash:1"));
        assert_eq!(
            &keys(&a)[2..5],
            ["h:trash", "i:trash:1", "i:trash:0"],
            "b before old.zip"
        );
        assert_eq!(press(&mut a, "s"), Action::SortChanged(Sort::Age));
        assert_eq!(press(&mut a, "s"), Action::SortChanged(Sort::Size));
    }

    #[test]
    fn slash_filters_by_title() {
        let mut a = app();
        press(&mut a, "/");
        assert_eq!(a.screen(), Screen::Filter);
        press(&mut a, "PIx");
        a.handle(Key::Backspace);
        a.handle(Key::Enter);
        assert_eq!(a.screen(), Screen::List);
        assert_eq!(a.filter(), "PI");
        assert_eq!(keys(&a), ["h:caches", "i:caches:0"]);
        assert_eq!(a.hidden_selected(), 1, "old.zip is selected but hidden");
        assert_eq!(a.cursor(), 0);
        // `a` only touches what the filter shows.
        goto(&mut a, "i:caches:0");
        press(&mut a, "a");
        assert_eq!(a.selected_ids(), ["trash:0"]);
        press(&mut a, "/");
        a.handle(Key::Esc);
        assert_eq!(a.filter(), "", "Esc clears the filter");
        assert_eq!(keys(&a).len(), 9);
    }

    #[test]
    fn enter_confirms_and_y_applies_the_selection() {
        let mut a = app();
        goto(&mut a, "i:trash:1");
        press(&mut a, " ");
        a.handle(Key::Enter);
        assert_eq!(a.screen(), Screen::Confirm);
        press(&mut a, "n");
        assert_eq!(a.screen(), Screen::List, "n goes back");
        a.handle(Key::Enter);
        a.handle(Key::Esc);
        assert_eq!(a.screen(), Screen::List, "Esc goes back, not out");
        a.handle(Key::Enter);
        assert_eq!(
            a.handle(Key::Enter),
            Action::None,
            "Enter alone never deletes"
        );
        assert_eq!(
            press(&mut a, "y"),
            Action::Apply(vec!["trash:0".into(), "trash:1".into(), "caches:0".into()])
        );
    }

    #[test]
    fn enter_with_nothing_selected_explains() {
        let mut a = app();
        goto(&mut a, "i:caches:0");
        press(&mut a, " ");
        goto(&mut a, "i:trash:0");
        press(&mut a, " ");
        assert!(a.selected_ids().is_empty());
        a.handle(Key::Enter);
        assert_eq!(a.screen(), Screen::List);
        assert_eq!(a.message(), Some("Nothing selected."));
    }

    #[test]
    fn q_esc_and_ctrl_c_quit() {
        assert_eq!(press(&mut app(), "q"), Action::Quit);
        assert_eq!(app().handle(Key::Esc), Action::Quit);
        assert_eq!(app().handle(Key::CtrlC), Action::Quit);
        let mut a = app();
        a.handle(Key::Enter);
        assert_eq!(a.handle(Key::CtrlC), Action::Quit);
    }

    #[test]
    fn empty_plan_is_not_a_crash() {
        let mut a = App::new(ReviewModel::new(Plan {
            entries: vec![],
            report_only: vec![],
            skipped: vec![],
            ..sample_plan()
        }));
        for k in [
            Key::Down,
            Key::Up,
            Key::Char(' '),
            Key::Char('a'),
            Key::Char('s'),
        ] {
            a.handle(k);
        }
        a.handle(Key::Enter);
        assert_eq!(a.screen(), Screen::List);
    }
}
