//! Shared data model. Every other module codes against these types.

use serde::Serialize;
use std::path::PathBuf;
use std::time::SystemTime;

/// Stable identifier for an artifact within a single scan session.
pub type ArtifactId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Category {
    /// A project-local Python virtualenv (`pyvenv.cfg` present).
    PyVenv,
    /// A venv under `~/.local/share/uv/tools/*`.
    UvTool,
    /// A venv under `~/.local/share/pipx/venvs/*`.
    Pipx,
    /// A conda environment (`conda-meta/history` present).
    CondaEnv,
    /// A `node_modules` directory with a sibling `package.json`.
    NodeModules,
    /// A cargo `target/` directory carrying `CACHEDIR.TAG`.
    CargoTarget,
    /// Aggregated `__pycache__` directories beneath one project root.
    /// The artifact's `path` is the project; its [`Artifact::children`] are the
    /// `__pycache__` directories, and they are the only things ever removed.
    Pycache,
    /// A well-known package-manager cache root.
    Cache,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::PyVenv => "venv",
            Category::UvTool => "uv-tool",
            Category::Pipx => "pipx",
            Category::CondaEnv => "conda",
            Category::NodeModules => "node",
            Category::CargoTarget => "cargo",
            Category::Pycache => "pycache",
            Category::Cache => "cache",
        }
    }

    pub const ALL: [Category; 8] = [
        Category::PyVenv,
        Category::UvTool,
        Category::Pipx,
        Category::CondaEnv,
        Category::NodeModules,
        Category::CargoTarget,
        Category::Pycache,
        Category::Cache,
    ];

    /// Parse a user-supplied `--category` token. Accepts the `label()` form.
    pub fn parse(s: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.label() == s)
    }
}

/// Hardlink-aware size accounting for one artifact.
///
/// All byte counts are *on-disk* usage (`st_blocks * 512`), not `st_size`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Sizes {
    /// Sum of on-disk usage of every regular file inside, counting each
    /// distinct inode once. Comparable to `du -s`.
    pub apparent: u64,
    /// Bytes actually freed by deleting *this artifact alone*: inodes whose
    /// every hardlink lives inside this artifact.
    pub reclaim: u64,
    /// Bytes held by inodes that also have links outside this artifact
    /// (`apparent - reclaim`). Deleting this artifact will not free them.
    pub shared: u64,
    /// Number of regular files encountered (distinct inodes).
    pub files: u64,
    /// Number of directories encountered.
    pub dirs: u64,
    /// True if the size walk hit errors (permissions, races) and is a lower bound.
    pub partial: bool,
}

/// Evidence gathered about whether an artifact is safe to remove.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Signals {
    /// mtime of the artifact directory itself.
    pub artifact_mtime: Option<SystemTime>,
    /// Newest mtime among non-artifact files in the parent project dir (depth <= 2).
    pub project_mtime: Option<SystemTime>,
    /// Newest mtime under `.git/refs/heads` in the parent project dir, if any.
    pub git_mtime: Option<SystemTime>,
    /// Lock/manifest filenames found next to the artifact, e.g. `["uv.lock"]`.
    pub reinstall_from: Vec<String>,
    /// `version_info` from `pyvenv.cfg`, e.g. `"3.12.11"`.
    pub python_version: Option<String>,
    /// For venvs: `Some(true)` when the base interpreter recorded in
    /// `pyvenv.cfg` no longer exists on disk, so the venv cannot be activated.
    /// `None` when not applicable or undetermined.
    pub interpreter_missing: Option<bool>,
}

/// Verdict derived from [`Signals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
pub enum Badge {
    /// Base interpreter is gone; the env is unusable. Always safe to delete.
    Dead,
    /// Stale, reinstallable, and no recent project activity.
    Safe,
    /// Not clearly safe; needs a human look.
    #[default]
    Review,
    /// The surrounding project was touched recently.
    Active,
}

impl Badge {
    pub fn label(self) -> &'static str {
        match self {
            Badge::Dead => "DEAD",
            Badge::Safe => "SAFE",
            Badge::Review => "REVIEW",
            Badge::Active => "ACTIVE",
        }
    }
}

/// One prunable thing on disk.
#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub id: ArtifactId,
    pub path: PathBuf,
    pub category: Category,
    /// Parent directory treated as "the project" for activity signals.
    /// `None` for cache roots, which have no owning project.
    pub project: Option<PathBuf>,
    /// The directories this artifact is actually made of, when `path` alone is
    /// not the thing to remove.
    ///
    /// Empty for every category whose `path` *is* the artifact. Non-empty only
    /// for [`Category::Pycache`], whose `path` is the owning project directory
    /// and must never be removed: the `__pycache__` directories listed here are.
    ///
    /// This is the single list that both sizing and deletion work from, so the
    /// bytes shown in the confirmation are the bytes that get removed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PathBuf>,
    pub sizes: Sizes,
    pub signals: Signals,
    pub badge: Badge,
    /// Human-readable justification for `badge`, shown in the detail pane.
    pub badge_reasons: Vec<String>,
    /// True once sizing has completed for this artifact.
    pub sized: bool,
    /// True once signals + badge have been computed.
    pub analyzed: bool,
    #[serde(skip)]
    pub marked: bool,
}

impl Artifact {
    pub fn new(
        id: ArtifactId,
        path: PathBuf,
        category: Category,
        project: Option<PathBuf>,
    ) -> Self {
        Artifact {
            id,
            path,
            category,
            project,
            children: Vec::new(),
            sizes: Sizes::default(),
            signals: Signals::default(),
            badge: Badge::Review,
            badge_reasons: Vec::new(),
            sized: false,
            analyzed: false,
            marked: false,
        }
    }
}

/// Totals for the whole scan.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ScanStats {
    pub dirs_visited: u64,
    pub artifacts_found: u64,
    /// Sum of `Sizes::apparent` across artifacts.
    pub total_apparent: u64,
    /// Session-deduplicated bytes: each inode counted once across *all*
    /// artifacts, so venvs sharing hardlinks with a cache don't double-count.
    pub total_unique: u64,
    pub errors: u64,
}

/// Streamed from the scanner to the UI so results appear before the walk ends.
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// An artifact was discovered. Sizes/signals are not yet filled in.
    Found(Box<Artifact>),
    /// Hardlink-aware sizing finished for an artifact.
    Sized(ArtifactId, Sizes),
    /// Signals and the derived badge finished for an artifact.
    Analyzed(ArtifactId, Box<Signals>, Badge, Vec<String>),
    /// Periodic walk progress.
    Progress { dirs_visited: u64 },
    /// A non-fatal error (unreadable dir, etc.).
    Error(String),
    /// The scan is complete; no further events will be sent.
    Done(ScanStats),
}
