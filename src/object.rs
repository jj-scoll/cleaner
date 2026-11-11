//! Plan entries that name discrete things deleted by exact id through
//! their owning tool (`Object`), and tool commands whose tool decides what
//! goes (`Operation`), spec §5.1a.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Object {
    Worktree(WorktreeRef),
    /// A worktree dir git no longer knows; removed with delete_tree (DECISION D2).
    OrphanWorktree {
        #[serde(with = "crate::plan::path_codec")]
        path: PathBuf,
        fp: crate::plan::Fingerprint,
    },
    /// Deleted one by one by exact id/name; several refs only for grouped volumes (DECISION D6).
    Docker { refs: Vec<DockerRef> },
    /// Exact generation numbers of one profile. root = true → via pkexec (DECISION D3).
    NixGenerations {
        #[serde(with = "crate::plan::path_codec")]
        profile: PathBuf,
        /// (generation number, store path it points to)
        gens: Vec<(u32, String)>,
        root: bool,
    },
    /// A symlink deleted with unlinkat and never followed: Nix `result*` links, orphaned profile links.
    Symlink {
        #[serde(with = "crate::plan::path_codec")]
        path: PathBuf,
        #[serde(with = "crate::plan::path_codec")]
        target: PathBuf,
    },
}

/// A linked worktree as `git worktree list` showed it at plan time.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WorktreeRef {
    #[serde(with = "crate::plan::path_codec")]
    pub path: PathBuf,
    /// The main worktree of the repository.
    #[serde(with = "crate::plan::path_codec")]
    pub main: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub newest_mtime: i64,
    pub dev: u64,
    pub ino: u64,
    /// Ignored paths outside build dirs at plan time (DECISION D1); apply
    /// skips if the list changed. Missing in older plans → empty.
    #[serde(default)]
    pub ignored: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum DockerKind {
    Container,
    Image,
    Volume,
    Builder,
    BuilderCache,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DockerRef {
    pub kind: DockerKind,
    pub id: String,
    pub created: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Operation {
    UvCachePrune,
    DockerBuilderPrune { until_hours: u64 },
    NixStoreGc,
    GoCleanModcache,
}
