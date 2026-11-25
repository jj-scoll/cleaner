mod common;

use cleaner::category::ai_models::AiModels;
use cleaner::category::{Category, Outcome, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::fsutil::measure::InodeSet;
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use common::{mkfile, now, set_age};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

struct Fixture {
    _tmp: tempfile::TempDir,
    /// Outside `home`, for env-var overrides and symlink targets.
    outside: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        Fixture {
            _tmp: tmp,
            outside,
            home,
        }
    }

    fn cat(env: &[(&str, &Path)]) -> AiModels {
        let env: HashMap<String, OsString> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_os_str().to_owned()))
            .collect();
        AiModels { env }
    }

    fn with_ctx<R>(&self, config: &Config, f: impl FnOnce(&ScanCtx) -> R) -> R {
        let protector = Protector::new(&self.home, &config.protect);
        let tools = common::FakeRunner::new();
        let ctx = ScanCtx {
            home: &self.home,
            now: now(),
            config,
            protector: &protector,
            tools: &tools,
            euid: common::my_uid(),
            usage: None,
        };
        f(&ctx)
    }

    fn scan_with(&self, config: &Config, cat: &AiModels) -> ScanOutput {
        self.with_ctx(config, |ctx| cat.scan(ctx, &mut InodeSet::default()))
            .unwrap()
    }

    fn scan(&self, env: &[(&str, &Path)]) -> ScanOutput {
        self.scan_with(&Config::default(), &Fixture::cat(env))
    }

    fn apply(&self, entry: &Entry, env: &[(&str, &Path)]) -> Outcome {
        self.with_ctx(&Config::default(), |ctx| {
            Fixture::cat(env).apply(entry, ctx)
        })
    }
}

fn targets(e: &Entry) -> Vec<PathBuf> {
    match &e.kind {
        EntryKind::Item { targets } => targets.iter().map(|t| t.path.clone()).collect(),
        other => panic!("not an item: {other:?}"),
    }
}

fn titles(out: &ScanOutput) -> Vec<&str> {
    let mut v: Vec<&str> = out.entries.iter().map(|e| e.title.as_str()).collect();
    v.sort();
    v
}

fn find<'a>(out: &'a ScanOutput, title: &str) -> &'a Entry {
    out.entries
        .iter()
        .find(|e| e.title == title)
        .unwrap_or_else(|| panic!("no entry {title:?} in {:#?}", out.entries))
}

// ---- Hugging Face -------------------------------------------------------

/// Sets the mtime of `root` and everything under it (symlinks themselves
/// included, never followed) to `days` ago, children first.
fn age_all(root: &Path, days: u64) {
    let md = std::fs::symlink_metadata(root).unwrap();
    if md.is_dir() {
        for e in std::fs::read_dir(root).unwrap() {
            age_all(&e.unwrap().path(), days);
        }
    }
    let t = rustix::fs::Timespec {
        tv_sec: now() - (days * 86_400) as i64,
        tv_nsec: 0,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        root,
        &rustix::fs::Timestamps {
            last_access: t,
            last_modification: t,
        },
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

/// A hub repo dir with one real blob of `bytes` and a snapshot symlink to
/// it, all last changed 40 days ago.
fn hf_repo(hub: &Path, name: &str, bytes: usize) -> PathBuf {
    let repo = hub.join(name);
    let blob = repo.join("blobs/0123abcd");
    mkfile(&blob, bytes);
    mkfile(&repo.join("refs/main"), 40);
    let snap = repo.join("snapshots/c0ffee");
    std::fs::create_dir_all(&snap).unwrap();
    symlink("../../blobs/0123abcd", snap.join("model.safetensors")).unwrap();
    age_all(&repo, 40);
    repo
}

fn default_hub(f: &Fixture) -> PathBuf {
    f.home.join(".cache/huggingface/hub")
}

#[test]
fn hf_each_repo_is_one_unselected_item() {
    let f = Fixture::new();
    let hub = default_hub(&f);
    let a = hf_repo(&hub, "models--org--big", 200_000);
    hf_repo(&hub, "datasets--org--data", 10_000);
    mkfile(&hub.join("version.txt"), 1);
    std::fs::create_dir_all(hub.join(".locks/models--org--big")).unwrap();

    let out = f.scan(&[]);
    assert_eq!(
        titles(&out),
        [
            "Hugging Face dataset org/data",
            "Hugging Face model org/big"
        ]
    );
    let e = find(&out, "Hugging Face model org/big");
    assert_eq!(e.category, "ai_models");
    assert!(!e.selected, "models are never pre-selected");
    assert!(e.bytes >= 200_000, "{}", e.bytes);
    assert_eq!(targets(e), [a]);
    assert!(e.last_active.is_some());
    assert!(e.reason.contains("40 days"), "{}", e.reason);
}

#[test]
fn hf_home_env_overrides_default() {
    let f = Fixture::new();
    hf_repo(&default_hub(&f), "models--org--default", 1000);
    let hf_home = f.outside.join("hf");
    hf_repo(&hf_home.join("hub"), "models--org--moved", 1000);

    let out = f.scan(&[("HF_HOME", &hf_home)]);
    assert_eq!(titles(&out), ["Hugging Face model org/moved"]);
}

#[test]
fn hf_hub_cache_env_beats_hf_home() {
    let f = Fixture::new();
    let hf_home = f.outside.join("hf");
    hf_repo(&hf_home.join("hub"), "models--org--home", 1000);
    let hub = f.outside.join("hub-cache");
    hf_repo(&hub, "models--org--hubcache", 1000);

    let out = f.scan(&[("HF_HOME", &hf_home), ("HF_HUB_CACHE", &hub)]);
    assert_eq!(titles(&out), ["Hugging Face model org/hubcache"]);
}

#[test]
fn hf_xdg_cache_home_is_honoured() {
    let f = Fixture::new();
    hf_repo(&default_hub(&f), "models--org--default", 1000);
    let xdg = f.outside.join("xdg");
    hf_repo(&xdg.join("huggingface/hub"), "models--org--xdg", 1000);

    let out = f.scan(&[("XDG_CACHE_HOME", &xdg)]);
    assert_eq!(titles(&out), ["Hugging Face model org/xdg"]);
}

#[test]
fn hf_symlinked_repo_is_skipped() {
    let f = Fixture::new();
    let hub = default_hub(&f);
    hf_repo(&hub, "models--org--real", 1000);
    let elsewhere = hf_repo(&f.outside, "models--org--linked", 5000);
    symlink(&elsewhere, hub.join("models--org--linked")).unwrap();

    let out = f.scan(&[]);
    assert_eq!(titles(&out), ["Hugging Face model org/real"]);
    assert!(
        out.skipped
            .iter()
            .any(|s| s.title.contains("models--org--linked")),
        "{:?}",
        out.skipped
    );
    assert!(elsewhere.join("blobs/0123abcd").exists());
}

#[test]
fn hf_symlinked_hub_is_skipped() {
    let f = Fixture::new();
    let real = f.outside.join("hub");
    hf_repo(&real, "models--org--x", 1000);
    std::fs::create_dir_all(f.home.join(".cache/huggingface")).unwrap();
    symlink(&real, default_hub(&f)).unwrap();

    let out = f.scan(&[]);
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
}

#[test]
fn hf_protected_repo_is_not_offered() {
    let f = Fixture::new();
    let hub = default_hub(&f);
    hf_repo(&hub, "models--org--keep", 1000);
    hf_repo(&hub, "models--org--other", 1000);
    let config = Config {
        protect: vec!["~/.cache/huggingface/hub/models--org--keep".into()],
        ..Config::default()
    };

    let out = f.scan_with(&config, &Fixture::cat(&[]));
    assert_eq!(titles(&out), ["Hugging Face model org/other"]);
}

/// The cache-wide shared blob store: `hub/blobs/<prefix>/<hash>`, linked
/// from each repo's `blobs/<etag>`.
fn shared_blob(hub: &Path, rel: &str, bytes: usize) -> PathBuf {
    let p = hub.join("blobs").join(rel);
    mkfile(&p, bytes);
    mkfile(&PathBuf::from(format!("{}.refs", p.display())), 10);
    p
}

fn link_shared(repo: &Path, etag: &str, rel: &str) {
    let blobs = repo.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    symlink(format!("../../blobs/{rel}"), blobs.join(etag)).unwrap();
}

#[test]
fn hf_shared_store_blobs_go_only_with_their_sole_repo() {
    let f = Fixture::new();
    let hub = default_hub(&f);
    let a = hf_repo(&hub, "models--org--a", 1000);
    let b = hf_repo(&hub, "models--org--b", 1000);
    let only_a = shared_blob(&hub, "aa/onlya", 300_000);
    let both = shared_blob(&hub, "bb/both", 500_000);
    link_shared(&a, "e1", "aa/onlya");
    link_shared(&a, "e2", "bb/both");
    link_shared(&b, "e3", "bb/both");

    let out = f.scan(&[]);
    let ea = find(&out, "Hugging Face model org/a");
    let t = targets(ea);
    assert_eq!(t[0], a, "the repo dir goes first");
    assert!(t.contains(&only_a), "{t:?}");
    assert!(
        !t.contains(&both),
        "a blob another repo uses is kept: {t:?}"
    );
    assert!(ea.bytes >= 300_000 && ea.bytes < 500_000, "{}", ea.bytes);
    let eb = find(&out, "Hugging Face model org/b");
    assert_eq!(targets(eb), [b]);

    let res = f.apply(ea, &[]);
    assert!(matches!(res, Outcome::Deleted { .. }), "{res:?}");
    assert!(!a.exists() && !only_a.exists());
    assert!(both.exists(), "shared blob must survive");
}

#[test]
fn hf_apply_skips_when_a_blob_became_shared() {
    let f = Fixture::new();
    let hub = default_hub(&f);
    let a = hf_repo(&hub, "models--org--a", 1000);
    let only_a = shared_blob(&hub, "aa/onlya", 300_000);
    link_shared(&a, "e1", "aa/onlya");
    let out = f.scan(&[]);
    let ea = find(&out, "Hugging Face model org/a").clone();
    assert!(targets(&ea).contains(&only_a));

    // After the plan, another repo starts using the same blob.
    let c = hf_repo(&hub, "models--org--c", 1000);
    link_shared(&c, "e9", "aa/onlya");

    let res = f.apply(&ea, &[]);
    assert!(matches!(res, Outcome::Skipped { .. }), "{res:?}");
    assert!(a.exists() && only_a.exists());
}

// ---- Ollama -------------------------------------------------------------

fn digest(n: u32) -> String {
    format!("{n:064x}")
}

fn blob(root: &Path, n: u32, bytes: usize) -> PathBuf {
    let p = root.join(format!("blobs/sha256-{}", digest(n)));
    mkfile(&p, bytes);
    set_age(&p, 20);
    p
}

/// Writes manifest `manifests/registry.ollama.ai/library/<model>/<tag>`
/// with config blob `cfg` and the given layer blobs.
fn manifest(root: &Path, model: &str, tag: &str, cfg: u32, layers: &[u32]) -> PathBuf {
    let layer_json: Vec<String> = layers
        .iter()
        .map(|n| {
            format!(
                r#"{{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:{}","size":1}}"#,
                digest(*n)
            )
        })
        .collect();
    let json = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"sha256:{}","size":1}},"layers":[{}]}}"#,
        digest(cfg),
        layer_json.join(",")
    );
    let p = root.join(format!(
        "manifests/registry.ollama.ai/library/{model}/{tag}"
    ));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, json).unwrap();
    set_age(&p, 20);
    p
}

/// Two models sharing blob 9 (a common base layer).
fn two_ollama_models(root: &Path) -> (PathBuf, PathBuf) {
    blob(root, 1, 100); // config of llama3
    blob(root, 2, 400_000); // weights of llama3
    blob(root, 3, 100); // config of mistral
    blob(root, 4, 300_000); // weights of mistral
    blob(root, 9, 700_000); // shared
    let m1 = manifest(root, "llama3", "latest", 1, &[2, 9]);
    let m2 = manifest(root, "mistral", "7b", 3, &[4, 9]);
    (m1, m2)
}

fn default_ollama(f: &Fixture) -> PathBuf {
    f.home.join(".ollama/models")
}

#[test]
fn ollama_model_is_manifest_plus_exclusive_blobs() {
    let f = Fixture::new();
    let root = default_ollama(&f);
    let (m1, _) = two_ollama_models(&root);

    let out = f.scan(&[]);
    assert_eq!(titles(&out), ["Ollama llama3:latest", "Ollama mistral:7b"]);
    let e = find(&out, "Ollama llama3:latest");
    assert!(!e.selected);
    let t = targets(e);
    assert_eq!(t[0], m1, "the manifest goes first");
    let mut blobs: Vec<PathBuf> = t[1..].to_vec();
    blobs.sort();
    assert_eq!(
        blobs,
        [
            root.join(format!("blobs/sha256-{}", digest(1))),
            root.join(format!("blobs/sha256-{}", digest(2)))
        ]
    );
    assert!(e.bytes >= 400_000 && e.bytes < 700_000, "{}", e.bytes);
}

#[test]
fn ollama_apply_never_deletes_a_shared_blob() {
    let f = Fixture::new();
    let root = default_ollama(&f);
    let (m1, m2) = two_ollama_models(&root);
    let out = f.scan(&[]);
    let e = find(&out, "Ollama llama3:latest");

    let res = f.apply(e, &[]);
    assert!(matches!(res, Outcome::Deleted { .. }), "{res:?}");
    assert!(!m1.exists());
    assert!(!root.join(format!("blobs/sha256-{}", digest(2))).exists());
    assert!(root.join(format!("blobs/sha256-{}", digest(9))).exists());
    assert!(m2.exists());
    assert!(root.join(format!("blobs/sha256-{}", digest(4))).exists());
}

#[test]
fn ollama_apply_skips_when_another_manifest_now_uses_a_blob() {
    let f = Fixture::new();
    let root = default_ollama(&f);
    let (m1, _) = two_ollama_models(&root);
    let out = f.scan(&[]);
    let e = find(&out, "Ollama llama3:latest").clone();

    // A new tag of the same weights is pulled after the plan.
    manifest(&root, "llama3", "8b", 1, &[2]);

    let res = f.apply(&e, &[]);
    assert!(matches!(res, Outcome::Skipped { .. }), "{res:?}");
    assert!(m1.exists());
    assert!(root.join(format!("blobs/sha256-{}", digest(2))).exists());
}

#[test]
fn ollama_tag_sharing_everything_frees_nothing_but_is_listed() {
    let f = Fixture::new();
    let root = default_ollama(&f);
    blob(&root, 1, 100);
    blob(&root, 2, 400_000);
    manifest(&root, "llama3", "latest", 1, &[2]);
    let alias = manifest(&root, "llama3", "8b", 1, &[2]);

    let out = f.scan(&[]);
    let e = find(&out, "Ollama llama3:8b");
    assert_eq!(targets(e), [alias], "no blob is exclusively its own");
    assert!(e.bytes < 100_000, "{}", e.bytes);
}

#[test]
fn ollama_models_env_overrides_default() {
    let f = Fixture::new();
    two_ollama_models(&default_ollama(&f));
    let moved = f.outside.join("ollama");
    blob(&moved, 5, 100);
    blob(&moved, 6, 1000);
    manifest(&moved, "qwen", "latest", 5, &[6]);

    let out = f.scan(&[("OLLAMA_MODELS", &moved)]);
    assert_eq!(titles(&out), ["Ollama qwen:latest"]);
}

#[test]
fn ollama_unreadable_manifest_offers_nothing() {
    let f = Fixture::new();
    let root = default_ollama(&f);
    two_ollama_models(&root);
    let bad = root.join("manifests/registry.ollama.ai/library/broken/latest");
    std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
    std::fs::write(&bad, "{not json").unwrap();

    let out = f.scan(&[]);
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert!(
        out.skipped[0].reason.contains("manifest"),
        "{:?}",
        out.skipped
    );
}

#[test]
fn ollama_digest_with_path_traversal_offers_nothing() {
    let f = Fixture::new();
    let root = default_ollama(&f);
    blob(&root, 1, 100);
    let victim = f.home.join("precious");
    mkfile(&victim, 100);
    let p = root.join("manifests/registry.ollama.ai/library/evil/latest");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(
        &p,
        format!(
            r#"{{"config":{{"digest":"sha256:{}"}},"layers":[{{"digest":"sha256:../../../precious"}}]}}"#,
            digest(1)
        ),
    )
    .unwrap();

    let out = f.scan(&[]);
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert!(victim.exists());
}

#[test]
fn ollama_symlinked_models_dir_is_skipped() {
    let f = Fixture::new();
    let real = f.outside.join("ollama");
    two_ollama_models(&real);
    std::fs::create_dir_all(f.home.join(".ollama")).unwrap();
    symlink(&real, default_ollama(&f)).unwrap();

    let out = f.scan(&[]);
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
}

#[test]
fn ollama_symlinked_manifest_offers_nothing() {
    let f = Fixture::new();
    let root = default_ollama(&f);
    let (m1, _) = two_ollama_models(&root);
    // A manifest we can't read safely might reference any blob.
    let copy = f.outside.join("m");
    std::fs::copy(&m1, &copy).unwrap();
    symlink(&copy, m1.with_file_name("linked")).unwrap();

    let out = f.scan(&[]);
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
}

// ---- LM Studio ----------------------------------------------------------

#[test]
fn lmstudio_each_model_dir_is_one_item() {
    let f = Fixture::new();
    let new = f
        .home
        .join(".lmstudio/models/lmstudio-community/Qwen3-8B-GGUF");
    mkfile(&new.join("Qwen3-8B-Q4_K_M.gguf"), 200_000);
    let legacy = f.home.join(".cache/lm-studio/models/TheBloke/Mistral-GGUF");
    mkfile(&legacy.join("mistral.Q4.gguf"), 100_000);
    let elsewhere = f.outside.join("Linked-GGUF");
    mkfile(&elsewhere.join("x.gguf"), 100_000);
    symlink(
        &elsewhere,
        f.home
            .join(".lmstudio/models/lmstudio-community/Linked-GGUF"),
    )
    .unwrap();

    let out = f.scan(&[]);
    assert_eq!(
        titles(&out),
        [
            "LM Studio TheBloke/Mistral-GGUF",
            "LM Studio lmstudio-community/Qwen3-8B-GGUF"
        ]
    );
    let e = find(&out, "LM Studio lmstudio-community/Qwen3-8B-GGUF");
    assert!(!e.selected);
    assert_eq!(targets(e), [new]);
    assert!(
        out.skipped.iter().any(|s| s.title.contains("Linked-GGUF")),
        "{:?}",
        out.skipped
    );
}

// ---- config -------------------------------------------------------------

fn all_three(f: &Fixture) {
    hf_repo(&default_hub(f), "models--org--x", 1000);
    two_ollama_models(&default_ollama(f));
    mkfile(&f.home.join(".lmstudio/models/p/m/w.gguf"), 1000);
}

#[test]
fn defaults_enable_all_three() {
    let c = Config::default().ai_models;
    assert!(c.enabled && c.huggingface && c.ollama && c.lmstudio);
}

#[test]
fn config_disables_each_source() {
    let f = Fixture::new();
    all_three(&f);
    let cat = Fixture::cat(&[]);
    assert_eq!(f.scan_with(&Config::default(), &cat).entries.len(), 4);

    let mut c = Config::default();
    c.ai_models.huggingface = false;
    assert!(
        !titles(&f.scan_with(&c, &cat))
            .iter()
            .any(|t| t.starts_with("Hugging"))
    );
    let mut c = Config::default();
    c.ai_models.ollama = false;
    assert!(
        !titles(&f.scan_with(&c, &cat))
            .iter()
            .any(|t| t.starts_with("Ollama"))
    );
    let mut c = Config::default();
    c.ai_models.lmstudio = false;
    assert!(
        !titles(&f.scan_with(&c, &cat))
            .iter()
            .any(|t| t.starts_with("LM"))
    );
    let mut c = Config::default();
    c.ai_models.enabled = false;
    assert!(f.scan_with(&c, &cat).entries.is_empty());
}
