//! `cleaner review`: the review page in a terminal, for machines without a
//! display. The state and rendering are plain functions (see `state` and
//! `render`); `run` only owns the terminal and the event loop. Deleting is
//! the caller's job, through the same `engine::apply_selected` as the GUI.

mod render;
pub(crate) mod state;

pub use render::render;
pub use state::{Action, App, Key, LineKind, ListLine, Screen};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::path::Path;

/// The reducer's key for a terminal key press; `None` for releases and
/// keys it doesn't use.
pub fn key_from_event(ev: &KeyEvent) -> Option<Key> {
    if ev.kind != KeyEventKind::Press {
        return None;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        return (ev.code == KeyCode::Char('c')).then_some(Key::CtrlC);
    }
    Some(match ev.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Char(c) => Key::Char(c),
        _ => return None,
    })
}

/// Shows `app` full-screen until the user quits (`None`) or confirms
/// (`Some(ids)` to apply). A sort change is saved in `state_dir`, as the
/// GUI saves it. The terminal is restored before this returns.
pub fn run(app: &mut App, state_dir: &Path) -> anyhow::Result<Option<Vec<String>>> {
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, app, state_dir);
    ratatui::try_restore()?;
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    state_dir: &Path,
) -> anyhow::Result<Option<Vec<String>>> {
    loop {
        terminal.draw(|f| render(f, app))?;
        // Anything but a key (a resize, say) just redraws.
        let Event::Key(ev) = event::read()? else {
            continue;
        };
        let Some(key) = key_from_event(&ev) else {
            continue;
        };
        match app.handle(key) {
            Action::None => {}
            Action::Quit => return Ok(None),
            Action::Apply(ids) => return Ok(Some(ids)),
            Action::SortChanged(sort) => {
                if let Err(e) = sort.save(state_dir) {
                    app.set_message(format!("can't remember the sort order: {e}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

    fn ev(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn keys_map_to_the_reducer() {
        let none = KeyModifiers::NONE;
        assert_eq!(key_from_event(&ev(KeyCode::Up, none)), Some(Key::Up));
        assert_eq!(key_from_event(&ev(KeyCode::Down, none)), Some(Key::Down));
        assert_eq!(key_from_event(&ev(KeyCode::Enter, none)), Some(Key::Enter));
        assert_eq!(key_from_event(&ev(KeyCode::Esc, none)), Some(Key::Esc));
        assert_eq!(
            key_from_event(&ev(KeyCode::Backspace, none)),
            Some(Key::Backspace)
        );
        assert_eq!(
            key_from_event(&ev(KeyCode::Char('j'), none)),
            Some(Key::Char('j'))
        );
        assert_eq!(
            key_from_event(&ev(KeyCode::Char('Y'), KeyModifiers::SHIFT)),
            Some(Key::Char('Y'))
        );
        assert_eq!(
            key_from_event(&ev(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Key::CtrlC)
        );
        assert_eq!(
            key_from_event(&ev(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None,
            "other Ctrl keys are not letters"
        );
        assert_eq!(key_from_event(&ev(KeyCode::F(1), none)), None);
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
            ..ev(KeyCode::Char('y'), none)
        };
        assert_eq!(key_from_event(&release), None, "only presses count");
    }
}
