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

    /// True if `p` is inside a protected root, contains one (deleting `p`
    /// would delete it), or has a `vendored` or `.git` component.
    pub fn is_protected(&self, p: &Path) -> bool {
        let bad_component = p.components().any(|c| {
            matches!(c, Component::Normal(n) if PROTECTED_COMPONENTS.iter().any(|b| n == *b))
        });
        bad_component
            || self
                .roots
                .iter()
                .any(|r| p.starts_with(r) || r.starts_with(p))
    }
}
