use std::time::{Duration, Instant};

use cleaner::engine::Progress;
use cleaner::fmt::elapsed;
use cleaner::progress::ApplyProgress;

const CLR: &str = "\r\x1b[2K";

fn ev(done: usize, total: usize, current: &str) -> Progress {
    Progress {
        done,
        total,
        current: current.into(),
        found: 0,
    }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn text(p: ApplyProgress<Vec<u8>>) -> String {
    String::from_utf8(p.into_inner()).unwrap()
}

#[test]
fn elapsed_formats() {
    for (s, want) in [
        (0, "0s"),
        (4, "4s"),
        (59, "59s"),
        (60, "1m00s"),
        (72, "1m12s"),
        (3599, "59m59s"),
        (3600, "1h00m"),
        (3780, "1h03m"),
    ] {
        assert_eq!(elapsed(secs(s)), want, "{s}s");
    }
}

#[test]
fn non_tty_prints_one_plain_line_per_entry() {
    let t0 = Instant::now();
    let mut p = ApplyProgress::new(Vec::new(), false, Some(40));
    p.event(&ev(0, 2, "Cargo target dirs"), t0);
    p.tick(t0 + secs(5));
    p.event(&ev(1, 2, "Nix garbage"), t0 + secs(6));
    p.pause();
    p.tick(t0 + secs(70));
    p.event(&ev(2, 2, ""), t0 + secs(80));
    assert_eq!(text(p), "[1/2] Cargo target dirs\n[2/2] Nix garbage\n");
}

#[test]
fn tty_redraws_in_place_and_finishes_lines() {
    let t0 = Instant::now();
    let mut p = ApplyProgress::new(Vec::new(), true, None);
    p.event(&ev(0, 2, "Cargo target dirs"), t0);
    p.tick(t0 + secs(72));
    p.event(&ev(1, 2, "Nix garbage"), t0 + secs(74));
    p.event(&ev(2, 2, ""), t0 + secs(80));
    let want = format!(
        "{CLR}[1/2] Cargo target dirs · 0s\
         {CLR}[1/2] Cargo target dirs · 1m12s\
         {CLR}[1/2] Cargo target dirs · done 1m14s\n\
         {CLR}[2/2] Nix garbage · 0s\
         {CLR}[2/2] Nix garbage · done 6s\n"
    );
    assert_eq!(text(p), want);
}

#[test]
fn tty_pause_ends_line_and_stops_redraw() {
    let t0 = Instant::now();
    let mut p = ApplyProgress::new(Vec::new(), true, None);
    p.event(&ev(0, 1, "System generations"), t0);
    p.pause();
    p.tick(t0 + secs(3));
    p.pause();
    p.resume();
    p.tick(t0 + secs(9));
    let want = format!(
        "{CLR}[1/1] System generations · 0s\n\
         {CLR}[1/1] System generations · 9s"
    );
    assert_eq!(text(p), want);
}

#[test]
fn tick_after_end_draws_nothing() {
    let t0 = Instant::now();
    let mut p = ApplyProgress::new(Vec::new(), true, None);
    p.event(&ev(0, 1, "a"), t0);
    p.event(&ev(1, 1, ""), t0 + secs(1));
    let before = format!("{CLR}[1/1] a · 0s{CLR}[1/1] a · done 1s\n");
    p.tick(t0 + secs(5));
    p.pause();
    assert_eq!(text(p), before);
}

#[test]
fn empty_run_prints_nothing() {
    for tty in [true, false] {
        let mut p = ApplyProgress::new(Vec::new(), tty, Some(80));
        p.event(&ev(0, 0, ""), Instant::now());
        assert_eq!(text(p), "", "tty={tty}");
    }
}

/// The visible lines drawn: each segment after a clear, minus a trailing newline.
fn segments(out: &str) -> Vec<&str> {
    out.split(CLR)
        .filter(|s| !s.is_empty())
        .map(|s| s.strip_suffix('\n').unwrap_or(s))
        .collect()
}

#[test]
fn tty_line_is_truncated_to_width() {
    let t0 = Instant::now();
    let title = "x".repeat(200);
    let mut p = ApplyProgress::new(Vec::new(), true, Some(40));
    p.event(&ev(0, 1, &title), t0);
    p.tick(t0 + secs(72));
    p.event(&ev(1, 1, ""), t0 + secs(73));
    let out = text(p);
    assert!(out.ends_with('\n'), "{out:?}");
    let segs = segments(&out);
    assert_eq!(segs.len(), 3, "{out:?}");
    for s in segs {
        assert!(s.chars().count() <= 39, "{s:?}");
        assert!(s.starts_with("[1/1] x"), "{s:?}");
        assert!(s.contains('…'), "{s:?}");
    }
}

#[test]
fn truncation_is_char_safe() {
    let mut p = ApplyProgress::new(Vec::new(), true, Some(12));
    p.event(&ev(0, 1, &"é".repeat(16)), Instant::now());
    let out = text(p);
    let segs = segments(&out);
    assert_eq!(segs.len(), 1, "{out:?}");
    assert!(segs[0].chars().count() <= 11, "{:?}", segs[0]);
    assert!(segs[0].contains('…'), "{:?}", segs[0]);
}

#[test]
fn zero_width_means_unknown() {
    // A pty with no size set (e.g. under `script`) reports 0 columns.
    let mut p = ApplyProgress::new(Vec::new(), true, Some(0));
    p.event(&ev(0, 1, "Cargo target dirs"), Instant::now());
    assert_eq!(text(p), format!("{CLR}[1/1] Cargo target dirs · 0s"));
}
