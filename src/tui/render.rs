//! Draws an `App`: a header, the list, a status line and the key help, or
//! the confirmation over the list.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{
    Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};

use super::state::{App, LineKind, Screen};
use crate::fmt::{human_bytes, plural};
use crate::view::category_label;

pub fn render(f: &mut Frame, app: &mut App) {
    let [top, body, status, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    f.render_widget(Paragraph::new(header(app)), top);
    list(f, app, body);
    f.render_widget(Paragraph::new(status_line(app)), status);
    f.render_widget(
        Paragraph::new(help_line(app.screen())).style(Style::new().add_modifier(Modifier::DIM)),
        help,
    );
    if app.screen() == Screen::Confirm {
        confirm(f, app, body);
    }
}

/// `Plan <id> · sort: size`, then the filter when there is one.
fn header(app: &App) -> String {
    let mut text = format!(
        "Plan {} · sort: {}",
        app.model().plan().id,
        app.sort().label().to_lowercase()
    );
    if app.screen() == Screen::Filter {
        text.push_str(&format!(" · filter: {}_", app.filter()));
    } else if !app.filter().is_empty() {
        text.push_str(&format!(" · filter: {}", app.filter()));
    }
    text
}

fn list(f: &mut Frame, app: &mut App, area: Rect) {
    let lines = app.lines();
    if lines.is_empty() {
        let text = if app.filter().is_empty() {
            "Nothing in this plan."
        } else {
            "Nothing matches the filter."
        };
        f.render_widget(Paragraph::new(text), area);
        return;
    }
    let items: Vec<ListItem> = lines
        .iter()
        .map(|l| {
            let mut text = Text::from(l.text.as_str());
            if l.kind == LineKind::Header {
                text = text.style(Style::new().add_modifier(Modifier::BOLD));
            }
            if let Some(note) = &l.note {
                text.push_line(Line::from(format!("    note: {note}")));
            }
            ListItem::new(text)
        })
        .collect();
    let widget = List::new(items)
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default()
        .with_offset(app.offset)
        .with_selected(Some(app.cursor()));
    f.render_stateful_widget(widget, area, &mut state);
    app.offset = state.offset();
}

/// The current note, or what is selected and what the filter hides.
fn status_line(app: &App) -> String {
    if let Some(m) = app.message() {
        return m.to_string();
    }
    let n = app.selected_ids().len();
    let mut text = format!(
        "Selected: {}, {}",
        plural(n, "item"),
        human_bytes(app.selected_bytes())
    );
    let hidden = app.hidden_selected();
    if hidden > 0 {
        text.push_str(&format!(
            " ({hidden} selected hidden by the filter, not deleted)"
        ));
    }
    text
}

fn help_line(screen: Screen) -> &'static str {
    match screen {
        Screen::List => {
            "↑↓/jk move  space toggle  a category  s sort  / filter  enter clean  q quit"
        }
        Screen::Filter => "type to filter by title  enter keep  esc clear",
        Screen::Confirm => "y delete  n/esc back",
    }
}

/// The GUI's confirmation text over the list, then what will be deleted.
fn confirm(f: &mut Frame, app: &App, area: Rect) {
    let c = app.confirm();
    let area = area.inner(Margin::new(2, 1));
    let ids = app.selected_ids();
    let chosen: Vec<_> = app
        .model()
        .plan()
        .entries
        .iter()
        .filter(|e| ids.contains(&e.id))
        .collect();
    // Two borders, up to two lines of body text and a blank line.
    let room = usize::from(area.height).saturating_sub(5);
    let shown = if chosen.len() > room {
        room.saturating_sub(1)
    } else {
        chosen.len()
    };
    let mut text = Text::from(c.body);
    text.push_line(Line::default());
    for e in &chosen[..shown] {
        let size = if e.is_estimate() {
            format!("~{}", human_bytes(e.bytes))
        } else {
            human_bytes(e.bytes)
        };
        text.push_line(Line::from(format!(
            "{size:>10}  {}  {}",
            category_label(&e.category),
            e.title
        )));
    }
    if shown < chosen.len() {
        text.push_line(Line::from(format!(
            "      …and {} more",
            chosen.len() - shown
        )));
    }
    let block = Block::new().borders(Borders::ALL).title(c.heading);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::Key;
    use crate::view::review::ReviewModel;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        App::new(ReviewModel::new(crate::tui::state::tests::sample_plan()))
    }

    /// The screen as text, one string per row, trailing spaces trimmed.
    fn draw(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| render(f, app)).unwrap();
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn find<'a>(screen: &'a [String], text: &str) -> &'a str {
        screen
            .iter()
            .find(|l| l.contains(text))
            .unwrap_or_else(|| panic!("no {text:?} on screen:\n{}", screen.join("\n")))
    }

    #[test]
    fn list_screen_shows_plan_rows_and_totals() {
        let screen = draw(&mut app(), 100, 16);
        assert!(screen[0].contains("Plan 20261111T030000Z"), "{}", screen[0]);
        assert!(screen[0].contains("sort: size"), "{}", screen[0]);
        assert!(find(&screen, "Tool caches").contains("[x]"));
        assert!(find(&screen, "Trash").contains("[~]"), "partly selected");
        assert!(find(&screen, "old.zip").contains("[x]    4.0 KiB  old.zip  (because)"));
        assert!(find(&screen, "  b  ").contains("[ ]"));
        assert!(find(&screen, "Videos").contains("[report only]"));
        assert!(!find(&screen, "Videos").contains("[ ]"), "no box to tick");
        find(&screen, "⊘ docker: daemon not running");
        find(&screen, "Selected: 2 items, 6.0 KiB");
        let help = &screen[screen.len() - 1];
        for k in ["space", "a ", "s ", "/", "enter", "q "] {
            assert!(help.contains(k), "help {help:?} lacks {k:?}");
        }
    }

    #[test]
    fn cursor_line_is_marked() {
        let mut a = app();
        a.handle(Key::Down);
        let screen = draw(&mut a, 100, 16);
        assert!(find(&screen, "pip").starts_with("> "), "{screen:?}");
        assert!(find(&screen, "old.zip").starts_with("  "));
    }

    #[test]
    fn long_lists_scroll_to_the_cursor() {
        let mut a = app();
        for _ in 0..8 {
            a.handle(Key::Down);
        }
        // 3 list rows: header, status and help take the other 3.
        let screen = draw(&mut a, 100, 6);
        assert!(find(&screen, "⊘ docker").starts_with("> "), "{screen:?}");
    }

    #[test]
    fn filter_is_shown_in_the_header() {
        let mut a = app();
        a.handle(Key::Char('/'));
        a.handle(Key::Char('p'));
        a.handle(Key::Char('i'));
        let screen = draw(&mut a, 100, 16);
        assert!(screen[0].contains("filter: pi_"), "{}", screen[0]);
        find(&screen, "1 selected hidden by the filter");
        assert!(!screen.iter().any(|l| l.contains("old.zip")));
    }

    #[test]
    fn filtered_counts_and_confirm_cover_only_visible_ticks() {
        let mut a = app();
        for c in "/pi".chars() {
            a.handle(Key::Char(c));
        }
        a.handle(Key::Enter);
        let screen = draw(&mut a, 100, 16);
        find(&screen, "Selected: 1 item, 2.0 KiB");
        find(&screen, "1 selected hidden by the filter, not deleted");
        a.handle(Key::Enter);
        let screen = draw(&mut a, 100, 16);
        find(&screen, "Delete 1 item?");
        find(
            &screen,
            "2.0 KiB will be deleted now. This cannot be undone.",
        );
        assert!(!screen.iter().any(|l| l.contains("old.zip")), "{screen:?}");
    }

    #[test]
    fn confirm_screen_shows_the_total_and_keys() {
        let mut a = app();
        a.handle(Key::Enter);
        let screen = draw(&mut a, 100, 16);
        find(&screen, "Delete 2 items?");
        find(
            &screen,
            "6.0 KiB will be deleted now. This cannot be undone.",
        );
        find(&screen, "old.zip");
        find(&screen, "y delete");
    }
}
