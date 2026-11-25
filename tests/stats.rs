use cleaner::log::{FsFreed, LogLine, Record, RunLine, parse_record, records};
use cleaner::stats::{
    Basis, CategoryFreed, RunFreed, Stats, Total, aggregate, from_log, parse_since,
};
use jiff::tz::{self, TimeZone};
use std::io::Write;

const DAY: i64 = 86_400;

fn ts(s: &str) -> i64 {
    s.parse::<jiff::Timestamp>().unwrap().as_second()
}

fn entry(run: &str, time: i64, category: &str, outcome: &str, bytes: u64) -> Record {
    Record::Entry(LogLine {
        time,
        run_id: run.into(),
        plan_id: "P".into(),
        entry_id: format!("{category}:0"),
        category: category.into(),
        title: "t".into(),
        outcome: outcome.into(),
        bytes,
        detail: String::new(),
    })
}

fn run(id: &str, time: i64, deltas: &[i64]) -> Record {
    Record::Run(RunLine {
        kind: "run".into(),
        time,
        run_id: id.into(),
        freed_by_fs: deltas
            .iter()
            .enumerate()
            .map(|(i, b)| FsFreed {
                fs: format!("/fs{i}"),
                bytes: *b,
            })
            .collect(),
    })
}

fn total(measured: u64, estimated: u64) -> Total {
    Total {
        bytes: measured + estimated,
        measured,
        estimated,
    }
}

const NOW: &str = "2026-11-25T12:00:00Z";

fn agg(records: Vec<Record>) -> Stats {
    aggregate(records, ts(NOW), &TimeZone::UTC, None)
}

#[test]
fn empty_log_is_all_zero() {
    let s = agg(vec![]);
    assert_eq!(s, Stats::default());
}

#[test]
fn estimated_runs_sum_deleted_bytes_only() {
    let t = ts(NOW) - DAY;
    let s = agg(vec![
        entry("A", t, "caches", "deleted", 100),
        entry("A", t, "trash", "deleted", 20),
        entry("A", t, "docker", "skipped", 0),
        // A failed line never carries bytes from the engine; ignore any anyway.
        entry("A", t, "nix", "failed", 7),
    ]);
    assert_eq!(s.runs, 1);
    assert_eq!(s.all_time, total(0, 120));
    assert_eq!(s.this_year, total(0, 120));
    assert_eq!(s.last_30_days, total(0, 120));
    assert_eq!(
        s.biggest_run,
        Some(RunFreed {
            run_id: "A".into(),
            time: t,
            bytes: 120,
            basis: Basis::Estimated,
        })
    );
}

#[test]
fn measured_run_line_replaces_the_item_sum() {
    let t = ts(NOW) - DAY;
    let s = agg(vec![
        entry("A", t, "caches", "deleted", 100),
        run("A", t + 5, &[300, -50]),
    ]);
    assert_eq!(s.runs, 1);
    assert_eq!(s.all_time, total(250, 0));
    let big = s.biggest_run.unwrap();
    assert_eq!((big.bytes, big.basis, big.time), (250, Basis::Measured, t));
    // Categories only exist per entry: still the item sum.
    assert_eq!(
        s.categories,
        vec![CategoryFreed {
            category: "caches".into(),
            bytes: 100,
        }]
    );
}

#[test]
fn negative_measurement_counts_as_zero() {
    let t = ts(NOW) - DAY;
    let s = agg(vec![
        entry("A", t, "caches", "deleted", 100),
        run("A", t, &[-4096]),
    ]);
    // The measurement wins over the 100-byte item sum, and freed nothing.
    assert_eq!(s.all_time, total(0, 0));
    assert_eq!(s.runs, 1);
    assert_eq!(s.biggest_run, None);
}

#[test]
fn measured_and_estimated_runs_add_up_separately() {
    let t = ts(NOW) - 2 * DAY;
    let s = agg(vec![
        entry("old", t, "caches", "deleted", 1000),
        entry("new", t + DAY, "trash", "deleted", 10),
        run("new", t + DAY, &[700]),
    ]);
    assert_eq!(s.runs, 2);
    assert_eq!(s.all_time, total(700, 1000));
    let big = s.biggest_run.unwrap();
    assert_eq!((big.run_id.as_str(), big.basis), ("old", Basis::Estimated));
}

#[test]
fn year_starts_at_local_midnight() {
    // UTC-5: local New Year is 05:00 UTC.
    let tz = TimeZone::fixed(tz::offset(-5));
    let now = ts("2027-01-01T05:30:00Z");
    let s = aggregate(
        vec![
            // 2026-12-31 23:59:59 local, and its run line just after midnight:
            // a run belongs to the time of its first line.
            entry("dec", ts("2027-01-01T04:59:59Z"), "caches", "deleted", 1),
            run("dec", ts("2027-01-01T05:00:01Z"), &[10]),
            // 2027-01-01 00:00:00 local.
            entry("jan", ts("2027-01-01T05:00:00Z"), "trash", "deleted", 200),
        ],
        now,
        &tz,
        None,
    );
    assert_eq!(s.all_time, total(10, 200));
    assert_eq!(s.this_year, total(0, 200));
    assert_eq!(s.last_30_days, total(10, 200));
}

#[test]
fn last_30_days_includes_its_first_second() {
    let now = ts(NOW);
    let s = agg(vec![
        entry("in", now - 30 * DAY, "caches", "deleted", 1),
        entry("out", now - 30 * DAY - 1, "caches", "deleted", 2),
    ]);
    assert_eq!(s.last_30_days, total(0, 1));
    assert_eq!(s.all_time, total(0, 3));
}

#[test]
fn since_drops_earlier_records() {
    let now = ts(NOW);
    let since = now - 10 * DAY;
    let s = aggregate(
        vec![
            entry("old", since - 1, "docker", "deleted", 5000),
            run("old", since - 1, &[6000]),
            entry("new", since, "caches", "deleted", 30),
            entry("new2", since + DAY, "trash", "skipped", 0),
        ],
        now,
        &TimeZone::UTC,
        Some(since),
    );
    assert_eq!(s.since, Some(since));
    assert_eq!(s.runs, 2);
    assert_eq!(s.all_time, total(0, 30));
    assert_eq!(s.biggest_run.unwrap().run_id, "new");
    assert_eq!(
        s.categories,
        vec![CategoryFreed {
            category: "caches".into(),
            bytes: 30,
        }]
    );
}

#[test]
fn categories_largest_first_ties_by_id_zero_dropped() {
    let t = ts(NOW) - DAY;
    let s = agg(vec![
        entry("A", t, "trash", "deleted", 50),
        entry("A", t, "caches", "deleted", 50),
        entry("B", t, "nix", "deleted", 10),
        entry("B", t, "nix", "deleted", 90),
        entry("B", t, "docker", "deleted", 0),
        entry("B", t, "worktrees", "skipped", 0),
    ]);
    let got: Vec<(&str, u64)> = s
        .categories
        .iter()
        .map(|c| (c.category.as_str(), c.bytes))
        .collect();
    assert_eq!(got, vec![("nix", 100), ("caches", 50), ("trash", 50)]);
}

#[test]
fn biggest_run_tie_goes_to_the_earlier_run() {
    let t = ts(NOW) - 3 * DAY;
    let s = agg(vec![
        entry("first", t, "caches", "deleted", 64),
        entry("second", t + DAY, "caches", "deleted", 64),
    ]);
    assert_eq!(s.biggest_run.unwrap().run_id, "first");
}

#[test]
fn runs_that_freed_nothing_still_count_as_runs() {
    let t = ts(NOW) - DAY;
    let s = agg(vec![entry("A", t, "caches", "skipped", 0)]);
    assert_eq!(s.runs, 1);
    assert_eq!(s.all_time, Total::default());
    assert_eq!(s.biggest_run, None);
}

#[test]
fn parse_record_kinds() {
    let e = br#"{"time":1,"run_id":"R","plan_id":"P","entry_id":"e","category":"c","title":"t","outcome":"deleted","bytes":9,"detail":"","extra":true}"#;
    assert_eq!(
        parse_record(e),
        Some(Record::Entry(LogLine {
            time: 1,
            run_id: "R".into(),
            plan_id: "P".into(),
            entry_id: "e".into(),
            category: "c".into(),
            title: "t".into(),
            outcome: "deleted".into(),
            bytes: 9,
            detail: String::new(),
        }))
    );
    let r = b"{\"kind\":\"run\",\"time\":2,\"run_id\":\"R\",\"freed_by_fs\":[{\"fs\":\"/\",\"bytes\":-3}]}\n";
    assert_eq!(parse_record(r), Some(run_named("R", 2, "/", -3)));
    for bad in [
        &br#"{"kind":"future","time":2,"run_id":"R","freed_by_fs":[]}"#[..],
        b"{\"time\":1,\"run_id\":\"R\",\"pla",
        b"",
        b"\n",
        b"\xff\xfe not utf-8",
    ] {
        assert_eq!(
            parse_record(bad),
            None,
            "{:?}",
            String::from_utf8_lossy(bad)
        );
    }
}

fn run_named(id: &str, time: i64, fs: &str, bytes: i64) -> Record {
    Record::Run(RunLine {
        kind: "run".into(),
        time,
        run_id: id.into(),
        freed_by_fs: vec![FsFreed {
            fs: fs.into(),
            bytes,
        }],
    })
}

#[test]
fn records_streams_and_skips_corrupt_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    let first = entry("A", 10, "caches", "deleted", 1);
    let last = run_named("A", 11, "/", 5);
    let Record::Entry(first_line) = &first else {
        unreachable!()
    };
    let mut f = std::fs::File::create(&path).unwrap();
    serde_json::to_writer(&mut f, first_line).unwrap();
    f.write_all(b"\n{\"time\":1,\"run_id\":\"torn\n\n\xff\xfe\xfd\n")
        .unwrap();
    // The final line has no trailing newline.
    f.write_all(br#"{"kind":"run","time":11,"run_id":"A","freed_by_fs":[{"fs":"/","bytes":5}]}"#)
        .unwrap();
    drop(f);
    let got: Vec<Record> = records(&path).unwrap().map(Result::unwrap).collect();
    assert_eq!(got, vec![first, last]);
}

#[test]
fn records_of_missing_log_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(records(&dir.path().join("nope.jsonl")).unwrap().count(), 0);
}

#[test]
fn from_log_matches_aggregate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/log.jsonl");
    let now = ts(NOW);
    let lines = [LogLine {
        time: now - DAY,
        run_id: "A".into(),
        plan_id: "P".into(),
        entry_id: "caches:0".into(),
        category: "caches".into(),
        title: "t".into(),
        outcome: "deleted".into(),
        bytes: 42,
        detail: String::new(),
    }];
    let r = RunLine {
        kind: "run".into(),
        time: now - DAY,
        run_id: "A".into(),
        freed_by_fs: vec![FsFreed {
            fs: "/".into(),
            bytes: 4096,
        }],
    };
    cleaner::log::append(&path, &lines, Some(&r)).unwrap();
    let s = from_log(&path, now, &TimeZone::UTC, None).unwrap();
    assert_eq!(s.runs, 1);
    assert_eq!(s.all_time, total(4096, 0));
    // The plain entry reader still sees only the entry line.
    assert_eq!(cleaner::log::read(&path, None).unwrap(), lines.to_vec());

    let missing = from_log(&dir.path().join("none"), now, &TimeZone::UTC, None).unwrap();
    assert_eq!(missing, Stats::default());
}

#[test]
fn parse_since_is_local_midnight() {
    assert_eq!(
        parse_since("2026-01-01", &TimeZone::UTC).unwrap(),
        ts("2026-01-01T00:00:00Z")
    );
    assert_eq!(
        parse_since("2026-01-01", &TimeZone::fixed(tz::offset(-5))).unwrap(),
        ts("2026-01-01T05:00:00Z")
    );
    for bad in ["2026-13-01", "yesterday", "", "2026-01-01T00:00"] {
        let e = parse_since(bad, &TimeZone::UTC).unwrap_err();
        assert!(format!("{e:#}").contains("YYYY-MM-DD"), "{bad}: {e:#}");
    }
}
