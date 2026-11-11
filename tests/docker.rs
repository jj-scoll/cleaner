mod common;

use cleaner::category::docker::Docker;
use cleaner::category::{Category, Outcome, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::fsutil::measure::InodeSet;
use cleaner::object::{DockerKind, Object, Operation};
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use common::{FakeRunner, err_out, ok_out};

const DF: &str = include_str!("fixtures/docker/df.json");
const BUILDX: &str = include_str!("fixtures/docker/buildx-ls.json");

const STALE: &str = "aaaa000000000000000000000000000000000000000000000000000000000001";
const YOUNG: &str = "aaaa000000000000000000000000000000000000000000000000000000000002";
const DANGLING: &str = "sha256:b908dda6c4d7b0000000000000000000000000000000000000000000000000aa";
const INSPECT: &str = "docker container inspect --format {{.State.Status}} {{.Created}}";
const FINISHED: &str = "docker container inspect --format {{.Id}} {{.State.FinishedAt}}";

/// 2026-11-10 12:00 UTC.
fn fixed_now() -> i64 {
    "2026-11-10T12:00:00Z".parse::<jiff::Timestamp>().unwrap().as_second()
}

fn with_ctx<R>(tools: &FakeRunner, f: impl FnOnce(&ScanCtx) -> R) -> R {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config::default();
    let protector = Protector::new(tmp.path(), &config.protect);
    let ctx = ScanCtx { home: tmp.path(), now: fixed_now(), config: &config, protector: &protector, tools, euid: common::my_uid(), usage: None };
    f(&ctx)
}

fn scan(tools: &FakeRunner) -> ScanOutput {
    with_ctx(tools, |ctx| Docker::default().scan(ctx, &mut InodeSet::default()).unwrap())
}

/// A daemon that answers the scan queries with the fixtures.
fn daemon() -> FakeRunner {
    FakeRunner::new()
        .respond("docker version", ok_out("29.9.2\n"))
        .respond("docker system df -v --format json", ok_out(DF))
        .respond("docker buildx ls --format json", ok_out(BUILDX))
        .respond(
            FINISHED,
            ok_out(&format!("{STALE} 2026-11-01T12:00:00.5Z\n{YOUNG} 2026-11-09T12:00:00.123456789Z\n")),
        )
}

/// `daemon()` plus apply-time answers under which every check passes.
fn healthy() -> FakeRunner {
    daemon()
        .respond(INSPECT, ok_out("exited 2026-10-31T12:00:00.123456789Z\n"))
        .respond(&format!("{INSPECT} {YOUNG}"), ok_out("exited 2026-11-09T12:00:00Z\n"))
        .respond("docker image inspect", ok_out("[{}]\n"))
        .respond("docker volume inspect", ok_out("[{}]\n"))
        .respond("docker ps -a -q --filter volume=", ok_out(""))
        .respond("docker buildx inspect", ok_out("Name: old\n"))
        .respond("docker rm", ok_out(""))
        .respond("docker rmi", ok_out(""))
        .respond("docker volume rm", ok_out(""))
        .respond("docker buildx rm", ok_out(""))
        .respond("docker buildx prune", ok_out("Total:\t29.51GB\n"))
        .respond("docker builder prune", ok_out("ID\tRECLAIMABLE\tSIZE\nabc\ttrue\t1GB\nTotal:\t1.114GB\n"))
}

fn entry<'a>(out: &'a ScanOutput, title: &str) -> &'a Entry {
    out.entries
        .iter()
        .find(|e| e.title == title)
        .unwrap_or_else(|| panic!("no entry {title:?} in {:?}", titles(out)))
}

fn titles(out: &ScanOutput) -> Vec<&str> {
    out.entries.iter().map(|e| e.title.as_str()).collect()
}

fn refs(e: &Entry) -> &[cleaner::object::DockerRef] {
    match &e.kind {
        EntryKind::Object { obj: Object::Docker { refs } } => refs,
        k => panic!("not a docker object: {k:?}"),
    }
}

fn apply(tools: &FakeRunner, e: &Entry) -> Outcome {
    with_ctx(tools, |ctx| Docker::default().apply(e, ctx))
}

#[test]
fn docker_missing_skips() {
    let out = scan(&FakeRunner::new());
    assert!(out.entries.is_empty());
    assert_eq!(out.skipped.len(), 1);
    assert_eq!(out.skipped[0].reason, "docker is not installed");
}

#[test]
fn daemon_down_skips() {
    let tools = FakeRunner::new().respond(
        "docker version",
        err_out(1, "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?\nmore\n"),
    );
    let out = scan(&tools);
    assert!(out.entries.is_empty());
    assert_eq!(out.skipped.len(), 1);
    assert_eq!(
        out.skipped[0].reason,
        "docker daemon not reachable: Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?"
    );
}

#[test]
fn plans_from_df() {
    let out = scan(&daemon());
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
    assert!(out.entries.iter().all(|e| e.permanent && e.category == "docker"));

    let selected: Vec<&str> = out.entries.iter().filter(|e| e.selected).map(|e| e.title.as_str()).collect();
    assert_eq!(selected, ["container stale_box", "image b908dda6c4d7", "build cache"]);

    let stale = entry(&out, "container stale_box");
    assert_eq!(stale.reason, "stopped 9 days ago");
    assert_eq!(stale.bytes, 521_000);
    assert_eq!(refs(stale)[0].kind, DockerKind::Container);
    assert_eq!(refs(stale)[0].id, STALE);
    assert!(refs(stale)[0].created.is_some());
    let young = entry(&out, "container young_box");
    assert_eq!(young.reason, "stopped 1 days ago");
    assert!(!young.selected);

    let dangling = entry(&out, "image b908dda6c4d7");
    assert_eq!(dangling.reason, "dangling image");
    assert_eq!(dangling.bytes, 250_000_000);
    assert_eq!(refs(dangling)[0].id, DANGLING);
    // Dangling but still used by stopped containers: rmi would refuse it.
    let used = entry(&out, "image c00f27cc74ff");
    assert!(!used.selected);
    assert_eq!(used.reason, "dangling image, used by 2 containers");
    let unused = entry(&out, "image app:new");
    assert_eq!(unused.reason, "unused image");
    assert!(!unused.selected);
    assert!(!titles(&out).iter().any(|t| t.contains("postgres")), "used image listed");

    assert_eq!(entry(&out, "volume oldproj_db").bytes, 300_000_000);
    entry(&out, "volume oldproj_cache");
    assert!(!titles(&out).contains(&"volume pgdata"), "linked volume listed");
    let anon: Vec<&Entry> = out.entries.iter().filter(|e| e.title.ends_with("anonymous volumes")).collect();
    assert_eq!(anon.len(), 1);
    assert_eq!(anon[0].title, "3 anonymous volumes");
    assert_eq!(refs(anon[0]).len(), 3);
    assert_eq!(anon[0].bytes, 3_000);

    let cur = entry(&out, "builder cur cache");
    assert_eq!(cur.reason, "current builder; its cache is pruned, the builder stays");
    assert_eq!(refs(cur)[0].kind, DockerKind::BuilderCache);
    assert_eq!(cur.bytes, 29_510_000_000);
    let old = entry(&out, "builder old");
    assert_eq!(old.reason, "removes the builder and its state");
    assert_eq!(refs(old)[0].kind, DockerKind::Builder);
    assert_eq!(old.bytes, 4_188_000_000);
    // The docker-driver builder is the daemon itself; its cache is the operation below.
    assert!(!titles(&out).iter().any(|t| t.starts_with("builder default")));
    let orphan = entry(&out, "builder state buildx_buildkit_gone0_state");
    assert_eq!(orphan.reason, "state of a builder that no longer exists");
    assert_eq!(refs(orphan)[0].kind, DockerKind::Volume);
    // Builder state volumes are not also listed as plain volumes.
    assert!(!titles(&out).iter().any(|t| t.starts_with("volume buildx_buildkit")));

    let cache = entry(&out, "build cache");
    assert_eq!(cache.kind, EntryKind::Operation { op: Operation::DockerBuilderPrune { until_hours: 168 } });
    assert_eq!(cache.reason, "build cache unused for 7+ days");
    // Shared records free nothing until every sharer goes (docker's own "reclaimable").
    assert_eq!(cache.bytes, 1_114_000_000);
    assert!(cache.is_estimate());

    assert_eq!(out.entries.len(), 12, "{:?}", titles(&out));
}

#[test]
fn apply_removes_by_id_without_force() {
    let tools = healthy();
    let out = scan(&tools);
    for e in &out.entries {
        let outcome = apply(&tools, e);
        assert!(matches!(outcome, Outcome::Deleted { .. }), "{}: {outcome:?}", e.title);
    }
    let calls = tools.calls();
    for want in [
        format!("docker rm {STALE}"),
        format!("docker rmi {DANGLING}"),
        "docker volume rm oldproj_db".into(),
        "docker volume rm cefe3c61bc4b1d54b620254cca496e5c4463df0bd723d9ca321c1bd4c218f6af".into(),
        "docker volume rm buildx_buildkit_gone0_state".into(),
        "docker buildx rm old".into(),
        "docker buildx prune --builder cur --all --force".into(),
        "docker builder prune --force --filter until=168h".into(),
    ] {
        assert!(calls.contains(&want), "missing {want:?} in {calls:#?}");
    }
    let removals: Vec<&String> = calls
        .iter()
        .filter(|c| {
            ["docker rm ", "docker rmi ", "docker volume rm ", "docker buildx rm "].iter().any(|p| c.starts_with(p))
        })
        .collect();
    assert!(removals.len() >= 10, "{removals:#?}");
    for c in removals {
        let words: Vec<&str> = c.split(' ').collect();
        assert!(!words.contains(&"-f") && !words.contains(&"--force"), "forced: {c}");
    }
}

#[test]
fn apply_skips_restarted_container() {
    let tools = healthy().respond(INSPECT, ok_out("running 2026-10-31T12:00:00.123456789Z\n"));
    let out = scan(&tools);
    match apply(&tools, entry(&out, "container stale_box")) {
        Outcome::Skipped { reason } => assert!(reason.contains("running"), "{reason}"),
        o => panic!("{o:?}"),
    }
    assert!(!tools.calls().iter().any(|c| c.starts_with("docker rm ")));
}

#[test]
fn apply_skips_recreated_container() {
    let tools = healthy().respond(INSPECT, ok_out("exited 2026-11-10T11:00:00Z\n"));
    let out = scan(&tools);
    match apply(&tools, entry(&out, "container stale_box")) {
        Outcome::Skipped { reason } => assert!(reason.contains("re-created"), "{reason}"),
        o => panic!("{o:?}"),
    }
    assert!(!tools.calls().iter().any(|c| c.starts_with("docker rm ")));
}

#[test]
fn apply_skips_volume_now_in_use() {
    let tools = healthy().respond("docker ps -a -q --filter volume=oldproj_db", ok_out("abc123\n"));
    let out = scan(&tools);
    match apply(&tools, entry(&out, "volume oldproj_db")) {
        Outcome::Skipped { reason } => assert!(reason.contains("in use"), "{reason}"),
        o => panic!("{o:?}"),
    }
    assert!(!tools.calls().iter().any(|c| c.starts_with("docker volume rm")));
}

#[test]
fn image_in_use_is_failure_not_abort() {
    let tools = healthy().respond(
        "docker rmi",
        err_out(
            1,
            "Error response from daemon: conflict: unable to delete b908dda6c4d7 (must be forced) \
             - image is being used by stopped container 1234\n",
        ),
    );
    let out = scan(&tools);
    match apply(&tools, entry(&out, "image b908dda6c4d7")) {
        Outcome::Failed { error } => assert!(
            error.starts_with("docker refused: Error response from daemon: conflict: unable to delete"),
            "{error}"
        ),
        o => panic!("{o:?}"),
    }
    // The next entry is still applied.
    let next = apply(&tools, entry(&out, "volume oldproj_db"));
    assert!(matches!(next, Outcome::Deleted { bytes: 300_000_000, .. }), "{next:?}");
}

#[test]
fn group_partial_failure_is_listed() {
    let tools = healthy().respond(
        "docker volume rm 0123456789abcdef",
        err_out(1, "Error response from daemon: remove 0123: volume is in use\n"),
    );
    let out = scan(&tools);
    match apply(&tools, entry(&out, "3 anonymous volumes")) {
        Outcome::Deleted { bytes, failures } => {
            assert_eq!(bytes, 2_000);
            assert_eq!(failures.len(), 1);
            assert!(failures[0].contains("docker refused"), "{failures:?}");
        }
        o => panic!("{o:?}"),
    }
}

#[test]
fn daemon_down_at_apply_skips_all() {
    let out = scan(&healthy());
    let down = FakeRunner::new()
        .respond("docker version", err_out(1, "Cannot connect to the Docker daemon\n"))
        .respond("docker", ok_out(""));
    for e in &out.entries {
        match apply(&down, e) {
            Outcome::Skipped { reason } => assert_eq!(reason, "docker daemon not reachable at apply"),
            o => panic!("{}: {o:?}", e.title),
        }
    }
    assert!(down.calls().iter().all(|c| c.starts_with("docker version")), "{:#?}", down.calls());
}

#[test]
fn builder_prune_parses_total() {
    let tools = healthy();
    let out = scan(&tools);
    let outcome = apply(&tools, entry(&out, "build cache"));
    assert_eq!(outcome, Outcome::Deleted { bytes: 1_114_000_000, failures: vec![] });
    let outcome = apply(&tools, entry(&out, "builder cur cache"));
    assert_eq!(outcome, Outcome::Deleted { bytes: 29_510_000_000, failures: vec![] });
}

#[test]
fn builder_prune_timeout_is_failed() {
    let tools = healthy().timeout("docker builder prune", 600);
    let out = scan(&tools);
    match apply(&tools, entry(&out, "build cache")) {
        Outcome::Failed { error } => assert!(error.contains("timed out"), "{error}"),
        o => panic!("{o:?}"),
    }
}

#[test]
fn container_aged_by_stop_time_not_creation() {
    // Created 10 days ago, but started and stopped again yesterday.
    let tools = daemon().respond(FINISHED, ok_out(&format!("{STALE} 2026-11-09T18:00:00Z\n{YOUNG} 2026-11-09T12:00:00Z\n")));
    let out = scan(&tools);
    let stale = entry(&out, "container stale_box");
    assert!(!stale.selected, "{stale:?}");
    assert_eq!(stale.reason, "stopped 0 days ago");
    // One batched inspect for every stopped container.
    let calls: Vec<String> = tools.calls().into_iter().filter(|c| c.starts_with(FINISHED)).collect();
    assert_eq!(calls, vec![format!("{FINISHED} {STALE} {YOUNG}")]);
}

#[test]
fn never_started_container_falls_back_to_creation_time() {
    let df = r#"{"Containers":[{"ID":"cccc","Names":"fresh","State":"created","CreatedAt":"2026-10-20 12:00:00 +0000 UTC","Size":"0B"}]}"#;
    let tools = FakeRunner::new()
        .respond("docker version", ok_out("29.9.2\n"))
        .respond("docker system df -v --format json", ok_out(df))
        .respond("docker buildx ls --format json", ok_out(""))
        .respond(FINISHED, ok_out("cccc0000 0001-01-01T00:00:00Z\n"));
    let out = scan(&tools);
    let e = entry(&out, "container fresh");
    assert!(e.selected, "{e:?}");
    assert_eq!(e.reason, "never started, created 21 days ago");
}

#[test]
fn unknown_stop_time_is_never_preselected() {
    let tools = daemon().timeout(FINISHED, 60);
    let out = scan(&tools);
    let stale = entry(&out, "container stale_box");
    assert!(!stale.selected, "{stale:?}");
    assert_eq!(stale.reason, "stopped, stop time unknown");
}

#[test]
fn hung_daemon_is_probed_once_per_run() {
    let out = scan(&healthy());
    let hung = FakeRunner::new().timeout("docker version", 60).respond("docker", ok_out(""));
    let d = Docker::default();
    with_ctx(&hung, |ctx| {
        for title in ["container stale_box", "image b908dda6c4d7", "build cache"] {
            match d.apply(entry(&out, title), ctx) {
                Outcome::Skipped { reason } => assert_eq!(reason, "docker daemon not reachable at apply"),
                o => panic!("{title}: {o:?}"),
            }
        }
    });
    assert_eq!(hung.calls(), vec!["docker version --format {{.Server.Version}}".to_string()]);
}

#[test]
fn timeout_stops_the_remaining_refs_and_the_run() {
    let tools = healthy().timeout("docker volume inspect", 60);
    let out = scan(&tools);
    let d = Docker::default();
    let before = tools.calls().len();
    with_ctx(&tools, |ctx| {
        match d.apply(entry(&out, "3 anonymous volumes"), ctx) {
            Outcome::Failed { error } => assert_eq!(error, "docker timed out after 60s"),
            o => panic!("{o:?}"),
        }
        match d.apply(entry(&out, "volume oldproj_db"), ctx) {
            Outcome::Skipped { reason } => assert_eq!(reason, "docker timed out earlier in this run"),
            o => panic!("{o:?}"),
        }
    });
    let calls = tools.calls()[before..].to_vec();
    assert_eq!(calls.iter().filter(|c| c.starts_with("docker volume inspect")).count(), 1, "{calls:#?}");
    assert_eq!(calls.iter().filter(|c| c.starts_with("docker version")).count(), 1, "{calls:#?}");
}

#[test]
fn builder_cache_of_removed_builder_is_skipped() {
    let tools = healthy().respond("docker buildx inspect cur", err_out(1, "ERROR: no builder \"cur\" found\n"));
    let out = scan(&tools);
    assert_eq!(apply(&tools, entry(&out, "builder cur cache")), Outcome::Skipped { reason: "no longer exists".into() });
    assert!(!tools.calls().iter().any(|c| c.starts_with("docker buildx prune")), "{:#?}", tools.calls());
}
