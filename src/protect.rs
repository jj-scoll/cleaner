//! Protected paths (spec §5.3): never candidates, even if a rule matches.

use std::path::{Component, Path, PathBuf};

pub const BUILTIN_PROTECTED: &[&str] = &[
    "~/.claude",
    "~/.claude-memd",
    "~/.cursor",
    "~/.local/share/atuin",
    "~/.local/share/memory",
];

/// Path components that protect anything at or below them.
const PROTECTED_COMPONENTS: &[&str] = &["vendored", ".git"];

pub struct Protector {
    roots: Vec<PathBuf>,
}

/// Why a path is protected (see [`Protector::why_protected`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Protection {
    /// A `vendored` or `.git` path component.
    Component(&'static str),
    /// At or inside a protected root; `builtin` unless from the user's list.
    Inside { root: PathBuf, builtin: bool },
    /// Contains a protected root: deleting the path would delete it.
    Contains { root: PathBuf, builtin: bool },
}

/// `~` and `~/…` become `home`-relative; anything else is taken literally (no glob).
fn tilde(entry: &str, home: &Path) -> PathBuf {
    if entry == "~" {
        home.to_path_buf()
    } else if let Some(rest) = entry.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(entry)
    }
}

impl Protector {
    /// Built-ins plus the user's `protect` entries, tilde-expanded.
    pub fn new(home: &Path, user: &[String]) -> Protector {
        let roots = BUILTIN_PROTECTED
            .iter()
            .copied()
            .chain(user.iter().map(String::as_str))
            .map(|e| tilde(e, home))
            .collect();
        Protector { roots }
    }

    /// Also protects `root` (and so everything at, below or above it).
    pub fn with_root(mut self, root: PathBuf) -> Protector {
        self.roots.push(root);
        self
    }

    /// True if `p` is inside a protected root, contains one (deleting `p`
    /// would delete it), or has a `vendored` or `.git` component.
    pub fn is_protected(&self, p: &Path) -> bool {
        self.why_protected(p).is_some()
    }

    /// Why `p` is protected, or `None`: a bad component first, then the
    /// first root (built-ins before the user's) `p` is in or contains.
    pub fn why_protected(&self, p: &Path) -> Option<Protection> {
        for c in p.components() {
            if let Component::Normal(n) = c
                && let Some(b) = PROTECTED_COMPONENTS.iter().find(|b| n == **b)
            {
                return Some(Protection::Component(b));
            }
        }
        self.roots.iter().enumerate().find_map(|(i, r)| {
            let builtin = i < BUILTIN_PROTECTED.len();
            if p.starts_with(r) {
                Some(Protection::Inside {
                    root: r.clone(),
                    builtin,
                })
            } else if r.starts_with(p) {
                Some(Protection::Contains {
                    root: r.clone(),
                    builtin,
                })
            } else {
                None
            }
        })
    }
}
