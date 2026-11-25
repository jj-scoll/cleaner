//! What `cleaner apply` and `cleaner review` print on stderr while applying.
//!
//! On a terminal the running entry's line is redrawn in place with its
//! elapsed time, then finished as "· done 1m12s"; otherwise each entry gets
//! one plain line as it starts. Time is passed in, so tests need no clock.

use std::io::Write;
use std::time::Instant;

use crate::engine::Progress;
use crate::fmt::elapsed;

/// Carriage return plus erase-line: redraws the current terminal line.
const CLEAR: &str = "\r\x1b[2K";

pub struct ApplyProgress<W: Write> {
    out: W,
    tty: bool,
    width: Option<usize>,
    /// "[n/total]" and the title of the running entry, and when it started.
    current: Option<(String, String, Instant)>,
    paused: bool,
}

impl<W: Write> ApplyProgress<W> {
    /// `width` (terminal columns) only matters when `tty`; 0 (a pty with no
    /// size set) counts as unknown.
    pub fn new(out: W, tty: bool, width: Option<usize>) -> Self {
        ApplyProgress {
            out,
            tty,
            width: width.filter(|&w| w > 0),
            current: None,
            paused: false,
        }
    }

    /// An apply progress event: finishes the running entry, then starts the
    /// next one, if any.
    pub fn event(&mut self, p: &Progress, now: Instant) {
        if self.tty
            && let Some((pos, title, start)) = self.current.take()
        {
            let line = self.line(&pos, &title, &format!("done {}", elapsed(now - start)));
            let _ = writeln!(self.out, "{CLEAR}{line}");
        }
        self.current = None;
        self.paused = false;
        if p.done < p.total {
            let pos = format!("[{}/{}]", p.done + 1, p.total);
            if self.tty {
                self.current = Some((pos, p.current.clone(), now));
                self.tick(now);
            } else {
                let _ = writeln!(self.out, "{pos} {}", p.current);
            }
        }
        let _ = self.out.flush();
    }

    /// Redraws the running entry with its elapsed time (terminal only).
    pub fn tick(&mut self, now: Instant) {
        if !self.tty || self.paused {
            return;
        }
        if let Some((pos, title, start)) = &self.current {
            let line = self.line(pos, title, &elapsed(now - *start));
            let _ = write!(self.out, "{CLEAR}{line}");
            let _ = self.out.flush();
        }
    }

    /// Something else is about to use the terminal: end the line and stop
    /// redrawing until `resume`.
    pub fn pause(&mut self) {
        if self.tty && self.current.is_some() && !self.paused {
            let _ = writeln!(self.out);
            let _ = self.out.flush();
            self.paused = true;
        }
    }

    /// Redraw again from the next `tick`.
    pub fn resume(&mut self) {
        self.paused = false;
    }

    pub fn into_inner(self) -> W {
        self.out
    }

    /// "[2/5] title · tail", cut to one character less than the terminal
    /// width so it never wraps (a wrapped line can't be redrawn in place).
    fn line(&self, pos: &str, title: &str, tail: &str) -> String {
        let full = format!("{pos} {title} · {tail}");
        let Some(max) = self.width.map(|w| w.saturating_sub(1)) else {
            return full;
        };
        if full.chars().count() <= max {
            return full;
        }
        let fixed = pos.chars().count() + 1 + 3 + tail.chars().count();
        if fixed < max {
            let keep: String = title.chars().take(max - fixed - 1).collect();
            format!("{pos} {keep}… · {tail}")
        } else {
            let keep: String = full.chars().take(max.saturating_sub(1)).collect();
            format!("{keep}…")
        }
    }
}
