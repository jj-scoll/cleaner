//! Docker containers, images, volumes, builders and build cache (spec §5.2e).
//! Each object is deleted by exact id through `docker` itself and never
//! forced, so docker keeps its own in-use refusals. The daemon's build cache
//! is an Operation: buildkit decides what an `until` prune removes.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::Duration;

use crate::category::{Category, Outcome, ScanCtx, ScanOutput, apply_default};
use crate::fsutil::measure::InodeSet;
use crate::object::{DockerKind, DockerRef, Object, Operation};
use crate::plan::{Entry, EntryKind, Skip};
use crate::tool::{CmdOut, OP_TIMEOUT, QUERY_TIMEOUT, ToolError, parse_human_size};

const ID: &str = "docker";
const DAY: i64 = 86_400;
/// Container states `docker rm` (without force) accepts.
const STOPPED: [&str; 3] = ["exited", "created", "dead"];
const VERSION: [&str; 3] = ["version", "--format", "{{.Server.Version}}"];

/// One per run (the engine builds categories per scan or apply).
#[derive(Default)]
pub struct Docker {
    /// Why docker is unusable for the rest of this apply run: the first
    /// failed daemon probe or tool timeout. A hung daemon then costs one
    /// timeout, not one per entry and ref (§4: no hang holding the lock).
    down: Mutex<Option<String>>,
}

const TIMED_OUT: &str = "docker timed out earlier in this run";

/// `docker system df -v --format json`. Docker prints counts as strings.
#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Df {
    images: Vec<Image>,
    containers: Vec<Container>,
    volumes: Vec<Volume>,
    build_cache: Vec<Cache>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Image {
    #[serde(rename = "ID")]
    id: String,
    repository: String,
    tag: String,
    containers: String,
    unique_size: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Container {
    #[serde(rename = "ID")]
    id: String,
    names: String,
    state: String,
    created_at: String,
    size: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Volume {
    name: String,
    links: String,
    size: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Cache {
    size: String,
    in_use: String,
    shared: String,
    created_at: String,
    last_used_at: String,
}

/// One line of `docker buildx ls --format json`.
#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Builder {
    name: String,
    current: bool,
    driver: String,
    nodes: Vec<Node>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Node {
    name: String,
}

fn docker(ctx: &ScanCtx, args: &[&str], timeout: Duration) -> Result<CmdOut, ToolError> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    ctx.tools.run("docker", &args, timeout)
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// Runs a query and returns its stdout, or why it failed.
fn query(ctx: &ScanCtx, args: &[&str]) -> Result<String, String> {
    match docker(ctx, args, QUERY_TIMEOUT) {
        Ok(o) if o.ok() => Ok(o.stdout),
        Ok(o) => Err(format!("docker {} failed: {}", args[..2].join(" "), first_line(&o.stderr))),
        Err(e) => Err(e.to_string()),
    }
}

/// `2026-11-04 21:02:39 -0500 EST` or `2026-10-29 00:26:34.98 +0000 UTC`;
/// the trailing zone name is ignored.
fn parse_time(s: &str) -> Option<jiff::Timestamp> {
    let mut parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() == 4 {
        parts.pop();
    }
    jiff::fmt::strtime::parse("%Y-%m-%d %H:%M:%S%.f %z", parts.join(" "))
        .ok()?
        .to_timestamp()
        .ok()
}

/// `State.FinishedAt` of each container, by full id, from one batched
/// inspect; empty if it fails (then none is pre-selected). Docker exits
/// non-zero if one id vanished but still prints the rest.
fn finished_times(ctx: &ScanCtx, cs: &[&Container]) -> Vec<(String, jiff::Timestamp)> {
    if cs.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["container", "inspect", "--format", "{{.Id}} {{.State.FinishedAt}}"];
    args.extend(cs.iter().map(|c| c.id.as_str()));
    let Ok(out) = docker(ctx, &args, QUERY_TIMEOUT) else { return Vec::new() };
    out.stdout
        .lines()
        .filter_map(|l| {
            let (id, t) = l.trim().split_once(' ')?;
            Some((id.to_string(), t.parse().ok()?))
        })
        .collect()
}

fn size(s: &str) -> u64 {
    parse_human_size(s).unwrap_or(0)
}

/// `buildx_buildkit_{node}_state` → `node`.
fn state_node(volume: &str) -> Option<&str> {
    volume.strip_prefix("buildx_buildkit_")?.strip_suffix("_state")
}

fn is_anonymous(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("{n} {word}") } else { format!("{n} {word}s") }
}

fn object(title: String, reason: &str, bytes: u64, selected: bool, refs: Vec<DockerRef>) -> Entry {
    Entry {
        id: String::new(),
        category: ID.into(),
        title,
        kind: EntryKind::Object { obj: Object::Docker { refs } },
        bytes,
        reason: reason.into(),
        selected,
        permanent: true,
        note: None,
    }
}

fn one(kind: DockerKind, id: &str) -> Vec<DockerRef> {
    vec![DockerRef { kind, id: id.into(), created: None }]
}

impl Category for Docker {
    fn id(&self) -> &'static str {
        ID
    }

    fn scan(&self, ctx: &ScanCtx, _seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let mut out = ScanOutput::default();
        let skip = |reason: String| Skip { category: ID.into(), title: ID.into(), reason };
        match docker(ctx, &VERSION, QUERY_TIMEOUT) {
            Err(e) => {
                out.skipped.push(skip(e.to_string()));
                return Ok(out);
            }
            Ok(o) if !o.ok() => {
                out.skipped.push(skip(format!("docker daemon not reachable: {}", first_line(&o.stderr))));
                return Ok(out);
            }
            Ok(_) => {}
        }
        let df: Df = match query(ctx, &["system", "df", "-v", "--format", "json"])
            .and_then(|s| serde_json::from_str(&s).map_err(|e| format!("can't read docker system df: {e}")))
        {
            Ok(df) => df,
            Err(reason) => {
                out.skipped.push(skip(reason));
                return Ok(out);
            }
        };
        // Without the builder list, no state volume can be told orphaned.
        let builders: Option<Vec<Builder>> = match query(ctx, &["buildx", "ls", "--format", "json"]) {
            Ok(s) => s
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()
                .map_err(|e| out.skipped.push(skip(format!("can't read docker buildx ls: {e}"))))
                .ok(),
            Err(reason) => {
                out.skipped.push(skip(reason));
                None
            }
        };
        let cfg = &ctx.config.docker;

        // Builder containers belong to their builder's row.
        let stopped: Vec<&Container> = df
            .containers
            .iter()
            .filter(|c| STOPPED.contains(&c.state.as_str()) && !c.names.starts_with("buildx_buildkit_"))
            .collect();
        let finished = finished_times(ctx, &stopped);
        for c in stopped {
            let title = format!("container {}", c.names);
            let Some(created) = parse_time(&c.created_at) else {
                out.skipped.push(Skip { category: ID.into(), title, reason: "unreadable creation time".into() });
                continue;
            };
            let refs = vec![DockerRef {
                kind: DockerKind::Container,
                id: c.id.clone(),
                created: Some(created.to_string()),
            }];
            // Spec §5.2e "stopped > 7 days": aged by when it stopped. A
            // never-started container has a zero FinishedAt: its creation.
            let days = |t: jiff::Timestamp| (ctx.now - t.as_second()) / DAY;
            let (reason, since) = match finished.iter().find(|(id, _)| id.starts_with(&c.id)).map(|(_, t)| *t) {
                Some(t) if t.as_second() > 0 => (format!("stopped {} days ago", days(t).max(0)), Some(t)),
                Some(_) => (format!("never started, created {} days ago", days(created).max(0)), Some(created)),
                None => ("stopped, stop time unknown".to_string(), None),
            };
            let old = since.is_some_and(|t| ctx.now - t.as_second() > cfg.container_days as i64 * DAY);
            out.entries.push(object(title, &reason, size(&c.size), old, refs));
        }

        for i in &df.images {
            let Ok(users) = i.containers.parse::<usize>() else { continue };
            let refs = one(DockerKind::Image, &i.id);
            let bytes = size(&i.unique_size);
            if i.repository == "<none>" {
                let short = i.id.strip_prefix("sha256:").unwrap_or(&i.id);
                let title = format!("image {}", short.get(..12).unwrap_or(short));
                // A dangling image a container still uses: rmi would refuse it.
                let reason = match users {
                    0 => "dangling image".to_string(),
                    n => format!("dangling image, used by {}", plural(n, "container")),
                };
                out.entries.push(object(title, &reason, bytes, users == 0, refs));
            } else if users == 0 {
                let title = format!("image {}:{}", i.repository, i.tag);
                out.entries.push(object(title, "unused image", bytes, false, refs));
            }
        }

        let nodes: Option<BTreeSet<&str>> = builders
            .as_ref()
            .map(|bs| bs.iter().flat_map(|b| &b.nodes).map(|n| n.name.as_str()).collect());
        let mut anonymous = Vec::new();
        let mut anonymous_bytes = 0;
        let mut orphans = Vec::new();
        for v in &df.volumes {
            if v.links.parse::<u64>() != Ok(0) {
                continue;
            }
            if let Some(node) = state_node(&v.name) {
                if nodes.as_ref().is_some_and(|ns| !ns.contains(node)) {
                    let title = format!("builder state {}", v.name);
                    let refs = one(DockerKind::Volume, &v.name);
                    orphans.push(object(title, "state of a builder that no longer exists", size(&v.size), false, refs));
                }
            } else if is_anonymous(&v.name) {
                anonymous.push(DockerRef { kind: DockerKind::Volume, id: v.name.clone(), created: None });
                anonymous_bytes += size(&v.size);
            } else {
                let refs = one(DockerKind::Volume, &v.name);
                out.entries.push(object(format!("volume {}", v.name), "unused volume", size(&v.size), false, refs));
            }
        }
        if !anonymous.is_empty() {
            // DECISION D6: one row, not pre-selected.
            let title = plural(anonymous.len(), "anonymous volume");
            out.entries.push(object(title, "unused anonymous volumes", anonymous_bytes, false, anonymous));
        }

        // Only docker-container builders keep local state; the docker-driver
        // builder is the daemon, whose cache is the operation below.
        for b in builders.iter().flatten().filter(|b| b.driver == "docker-container") {
            let bytes = b
                .nodes
                .iter()
                .filter_map(|n| df.volumes.iter().find(|v| state_node(&v.name) == Some(&n.name)))
                .map(|v| size(&v.size))
                .sum();
            let entry = if b.current {
                let reason = "current builder; its cache is pruned, the builder stays";
                object(format!("builder {} cache", b.name), reason, bytes, false, one(DockerKind::BuilderCache, &b.name))
            } else {
                let reason = "removes the builder and its state";
                object(format!("builder {}", b.name), reason, bytes, false, one(DockerKind::Builder, &b.name))
            };
            out.entries.push(entry);
        }
        out.entries.extend(orphans);

        // A shared record frees nothing until every record sharing it goes,
        // so the estimate leaves it out (as docker's own "reclaimable" does).
        let cutoff = ctx.now - cfg.build_cache_days as i64 * DAY;
        let stale: Vec<&Cache> = df
            .build_cache
            .iter()
            .filter(|c| c.in_use != "true" && c.shared != "true")
            .filter(|c| {
                let last = parse_time(&c.last_used_at).or_else(|| parse_time(&c.created_at));
                last.is_some_and(|t| t.as_second() < cutoff)
            })
            .collect();
        if !stale.is_empty() {
            out.entries.push(Entry {
                id: String::new(),
                category: ID.into(),
                title: "build cache".into(),
                kind: EntryKind::Operation {
                    op: Operation::DockerBuilderPrune { until_hours: cfg.build_cache_days * 24 },
                },
                bytes: stale.iter().map(|c| size(&c.size)).sum(),
                reason: format!("build cache unused for {}+ days", cfg.build_cache_days),
                selected: true,
                permanent: true,
                note: None,
            });
        }
        Ok(out)
    }

    fn apply(&self, entry: &Entry, ctx: &ScanCtx) -> Outcome {
        let docker_kind = matches!(
            entry.kind,
            EntryKind::Object { obj: Object::Docker { .. } }
                | EntryKind::Operation { op: Operation::DockerBuilderPrune { .. } }
        );
        if !docker_kind {
            return apply_default(entry, ctx);
        }
        let mut down = self.down.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(reason) = down.as_ref() {
            return Outcome::Skipped { reason: reason.clone() };
        }
        let probe = match docker(ctx, &VERSION, QUERY_TIMEOUT) {
            Ok(o) if o.ok() => None,
            Err(e @ ToolError::Missing(_)) => Some(e.to_string()),
            _ => Some("docker daemon not reachable at apply".to_string()),
        };
        if let Some(reason) = probe {
            *down = Some(reason.clone());
            return Outcome::Skipped { reason };
        }
        let (outcome, timed_out) = match &entry.kind {
            EntryKind::Object { obj: Object::Docker { refs } } => apply_refs(entry, refs, ctx),
            EntryKind::Operation { op: Operation::DockerBuilderPrune { until_hours } } => {
                let filter = format!("until={until_hours}h");
                match prune(ctx, &["builder", "prune", "--force", "--filter", &filter]) {
                    Ok(bytes) => (Outcome::Deleted { bytes, failures: Vec::new() }, false),
                    Err(RefErr::Fail(error)) => (Outcome::Failed { error }, false),
                    Err(RefErr::Timeout(error)) => (Outcome::Failed { error }, true),
                    Err(RefErr::Skip(reason)) => (Outcome::Skipped { reason }, false),
                }
            }
            _ => (apply_default(entry, ctx), false),
        };
        if timed_out {
            *down = Some(TIMED_OUT.into());
        }
        outcome
    }
}

enum RefErr {
    /// A re-check failed: the object changed since plan.
    Skip(String),
    /// Docker refused or the call failed.
    Fail(String),
    /// The call timed out: the daemon may be hung, so stop calling it.
    Timeout(String),
}

/// Applies each ref on its own. Discrete refs are credited a share of
/// `entry.bytes`; a builder-cache prune is credited what it reports. After
/// a timeout the remaining refs are not tried; `.1` says one happened.
fn apply_refs(entry: &Entry, refs: &[DockerRef], ctx: &ScanCtx) -> (Outcome, bool) {
    let mut discrete = 0u64;
    let mut pruned = 0u64;
    let mut any = false;
    let mut failures = Vec::new();
    let mut first_skip = None;
    let mut first_fail = None;
    let mut timed_out = false;
    for r in refs {
        if timed_out {
            failures.push(format!("{}: skipped: docker timed out", r.id));
            continue;
        }
        match apply_ref(r, ctx) {
            Ok(Some(bytes)) => {
                any = true;
                pruned += bytes;
            }
            Ok(None) => {
                any = true;
                discrete += 1;
            }
            Err(RefErr::Skip(reason)) => {
                failures.push(format!("{}: {reason}", r.id));
                first_skip.get_or_insert(reason);
            }
            Err(RefErr::Fail(error)) => {
                failures.push(format!("{}: {error}", r.id));
                first_fail.get_or_insert(error);
            }
            Err(RefErr::Timeout(error)) => {
                failures.push(format!("{}: {error}", r.id));
                first_fail.get_or_insert(error);
                timed_out = true;
            }
        }
    }
    let outcome = if any {
        let bytes = pruned + entry.bytes * discrete / refs.len().max(1) as u64;
        Outcome::Deleted { bytes, failures }
    } else if let Some(error) = first_fail {
        Outcome::Failed { error }
    } else {
        Outcome::Skipped { reason: first_skip.unwrap_or_else(|| "nothing to delete".into()) }
    };
    (outcome, timed_out)
}

fn tool_err(e: ToolError) -> RefErr {
    match e {
        ToolError::Timeout(..) => RefErr::Timeout(e.to_string()),
        e => RefErr::Fail(e.to_string()),
    }
}

/// Re-checks one ref and deletes it. `Ok(Some(bytes))` for a prune that
/// reports its total, `Ok(None)` for a discrete removal.
fn apply_ref(r: &DockerRef, ctx: &ScanCtx) -> Result<Option<u64>, RefErr> {
    let id = r.id.as_str();
    match r.kind {
        DockerKind::Container => {
            let out = exists(ctx, &["container", "inspect", "--format", "{{.State.Status}} {{.Created}}", id])?;
            let line = out.trim();
            let (status, created) = line.split_once(' ').unwrap_or((line, ""));
            if !STOPPED.contains(&status) {
                return Err(RefErr::Skip(format!("container is {status} now")));
            }
            let planned = r.created.as_deref().and_then(|s| s.parse::<jiff::Timestamp>().ok());
            let now = created.parse::<jiff::Timestamp>().ok();
            match (planned, now) {
                (Some(a), Some(b)) if a.as_second() == b.as_second() => {}
                _ => return Err(RefErr::Skip("container was re-created since plan".into())),
            }
            remove(ctx, &["rm", id])
        }
        DockerKind::Image => {
            exists(ctx, &["image", "inspect", id])?;
            remove(ctx, &["rmi", id])
        }
        DockerKind::Volume => {
            exists(ctx, &["volume", "inspect", id])?;
            let filter = format!("volume={id}");
            let users = exists(ctx, &["ps", "-a", "-q", "--filter", &filter])?;
            if !users.trim().is_empty() {
                return Err(RefErr::Skip("volume is in use by a container now".into()));
            }
            remove(ctx, &["volume", "rm", id])
        }
        DockerKind::Builder => {
            exists(ctx, &["buildx", "inspect", id])?;
            remove(ctx, &["buildx", "rm", id])
        }
        DockerKind::BuilderCache => {
            exists(ctx, &["buildx", "inspect", id])?;
            prune(ctx, &["buildx", "prune", "--builder", id, "--all", "--force"]).map(Some)
        }
    }
}

/// A re-check query: non-zero exit means the object is gone.
fn exists(ctx: &ScanCtx, args: &[&str]) -> Result<String, RefErr> {
    match docker(ctx, args, QUERY_TIMEOUT) {
        Ok(o) if o.ok() => Ok(o.stdout),
        Ok(_) => Err(RefErr::Skip("no longer exists".into())),
        Err(e) => Err(tool_err(e)),
    }
}

fn run_op(ctx: &ScanCtx, args: &[&str]) -> Result<CmdOut, RefErr> {
    match docker(ctx, args, OP_TIMEOUT) {
        Ok(o) if o.ok() => Ok(o),
        Ok(o) => Err(RefErr::Fail(format!("docker refused: {}", first_line(&o.stderr)))),
        Err(e) => Err(tool_err(e)),
    }
}

fn remove(ctx: &ScanCtx, args: &[&str]) -> Result<Option<u64>, RefErr> {
    run_op(ctx, args).map(|_| None)
}

/// Runs a prune and returns the bytes it reports (`Total:` or
/// `Total reclaimed space:`), or 0 if it reports none.
fn prune(ctx: &ScanCtx, args: &[&str]) -> Result<u64, RefErr> {
    let out = run_op(ctx, args)?;
    let total = out.stdout.lines().chain(out.stderr.lines()).find_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix("Total reclaimed space:").or_else(|| l.strip_prefix("Total:"))?;
        parse_human_size(rest)
    });
    Ok(total.unwrap_or(0))
}
