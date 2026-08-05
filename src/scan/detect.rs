//! Directory classification. `detect` is called once per visited directory and
//! answers a single question: is *this* directory an artifact root?

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use crate::model::Category;

/// The verdict for one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub category: Category,
    /// Directory treated as the owning project, when there is one.
    pub project: Option<PathBuf>,
}

impl Detected {
    fn new(category: Category, project: Option<PathBuf>) -> Detected {
        Detected { category, project }
    }
}

/// Standard first line of a `CACHEDIR.TAG` file.
const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

const UV_TOOLS: [&str; 4] = [".local", "share", "uv", "tools"];
const PIPX_VENVS: [&str; 4] = [".local", "share", "pipx", "venvs"];

const SYSTEM_DIRS: [&str; 5] = ["/proc", "/sys", "/dev", "/run", "/tmp"];

/// Classify `dir`. Returns `Some` only when `dir` itself is the artifact root.
pub fn detect(dir: &Path) -> Option<Detected> {
    if is_regular_file(&dir.join("pyvenv.cfg")) {
        if has_component_run(dir, &UV_TOOLS) {
            return Some(Detected::new(Category::UvTool, None));
        }
        if has_component_run(dir, &PIPX_VENVS) {
            return Some(Detected::new(Category::Pipx, None));
        }
        return Some(Detected::new(Category::PyVenv, parent_of(dir)));
    }

    if is_regular_file(&dir.join("conda-meta").join("history")) {
        return Some(Detected::new(Category::CondaEnv, None));
    }

    let name = dir.file_name();

    if name == Some(OsStr::new("node_modules")) {
        // Every `node_modules` is an artifact, with or without a sibling
        // manifest. Requiring one used to gate detection, which had exactly the
        // opposite of the intended effect: a global npm root (`lib/node_modules`,
        // no `lib/package.json`) failed to match, the walker descended, and each
        // installed package inside matched on its own manifest -- reporting one
        // 1.8 GB tree as 108 nested fragments.
        //
        // The manifest now only identifies the owning project for activity and
        // lockfile signals. Nesting is prevented by pruning in the walker, which
        // never descends into a `node_modules` regardless of what is returned
        // here.
        let project = dir
            .parent()
            .filter(|p| is_regular_file(&p.join("package.json")))
            .map(Path::to_path_buf);
        return Some(Detected::new(Category::NodeModules, project));
    }

    if name == Some(OsStr::new("target")) && has_cachedir_tag(&dir.join("CACHEDIR.TAG")) {
        return Some(Detected::new(Category::CargoTarget, parent_of(dir)));
    }

    if name == Some(OsStr::new("__pycache__")) {
        return Some(Detected::new(Category::Pycache, parent_of(dir)));
    }

    None
}

/// Well-known package-manager caches under `$HOME`, filtered to those present.
pub fn cache_roots() -> Vec<(PathBuf, Category)> {
    const RELATIVE: [&str; 9] = [
        ".cache/uv",
        ".cache/pip",
        ".cache/pypoetry",
        ".cache/pdm",
        ".cache/huggingface",
        ".cargo/registry",
        ".npm",
        ".local/share/pnpm/store",
        ".conda/pkgs",
    ];

    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    if home.as_os_str().is_empty() {
        return Vec::new();
    }

    RELATIVE
        .iter()
        .map(|rel| home.join(rel))
        .filter(|p| is_dir(p))
        .map(|p| (p, Category::Cache))
        .collect()
}

/// Directories the walker must never descend into.
pub fn is_skippable(dir: &Path) -> bool {
    if dir.file_name() == Some(OsStr::new(".git")) {
        return true;
    }
    SYSTEM_DIRS.iter().any(|s| dir == Path::new(s))
}

fn parent_of(dir: &Path) -> Option<PathBuf> {
    dir.parent().map(Path::to_path_buf)
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

/// True when `path` is a regular file whose first bytes are the cache-dir
/// signature. Only the first 64 bytes are read.
fn has_cachedir_tag(path: &Path) -> bool {
    if !is_regular_file(path) {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };

    let mut buf = [0u8; 64];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }

    buf[..filled].starts_with(CACHEDIR_SIGNATURE)
}

/// True when `run` appears as a consecutive sequence of path components.
/// Component-wise so a directory merely *named* like part of the run cannot match.
fn has_component_run(path: &Path, run: &[&str]) -> bool {
    if run.is_empty() {
        return true;
    }
    let parts: Vec<&OsStr> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    parts
        .windows(run.len())
        .any(|w| w.iter().zip(run).all(|(a, b)| *a == OsStr::new(*b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"").unwrap();
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn venv_is_detected() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("proj");
        let venv = proj.join(".venv");
        touch(&venv.join("pyvenv.cfg"));

        let d = detect(&venv).expect("venv should be detected");
        assert_eq!(d.category, Category::PyVenv);
        assert_eq!(d.project.as_deref(), Some(proj.as_path()));
    }

    #[test]
    fn plain_directory_is_not_an_artifact() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("src");
        fs::create_dir_all(&dir).unwrap();
        assert!(detect(&dir).is_none());
    }

    #[test]
    fn uv_tool_path_yields_uv_tool() {
        let tmp = TempDir::new().unwrap();
        let venv = tmp.path().join(".local/share/uv/tools/ruff");
        touch(&venv.join("pyvenv.cfg"));

        let d = detect(&venv).expect("uv tool should be detected");
        assert_eq!(d.category, Category::UvTool);
        assert_eq!(d.project, None);
    }

    #[test]
    fn pipx_path_yields_pipx() {
        let tmp = TempDir::new().unwrap();
        let venv = tmp.path().join(".local/share/pipx/venvs/black");
        touch(&venv.join("pyvenv.cfg"));

        let d = detect(&venv).expect("pipx venv should be detected");
        assert_eq!(d.category, Category::Pipx);
        assert_eq!(d.project, None);
    }

    #[test]
    fn directory_named_pipx_elsewhere_stays_a_plain_venv() {
        let tmp = TempDir::new().unwrap();
        let venv = tmp.path().join("pipx/venvs/thing");
        touch(&venv.join("pyvenv.cfg"));

        let d = detect(&venv).unwrap();
        assert_eq!(d.category, Category::PyVenv);
    }

    #[test]
    fn conda_env_is_detected() {
        let tmp = TempDir::new().unwrap();
        let env = tmp.path().join("miniconda3/envs/sci");
        touch(&env.join("conda-meta/history"));

        let d = detect(&env).unwrap();
        assert_eq!(d.category, Category::CondaEnv);
        assert_eq!(d.project, None);
    }

    #[test]
    fn node_modules_is_detected_with_or_without_a_sibling_manifest() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("app");
        let nm = proj.join("node_modules");
        fs::create_dir_all(&nm).unwrap();

        // No manifest: still an artifact, but no owning project.
        let d = detect(&nm).expect("a node_modules is an artifact on its own");
        assert_eq!(d.category, Category::NodeModules);
        assert_eq!(d.project, None);

        // With a manifest the project is identified, for activity signals.
        touch(&proj.join("package.json"));
        let d = detect(&nm).expect("node_modules with manifest should be detected");
        assert_eq!(d.category, Category::NodeModules);
        assert_eq!(d.project.as_deref(), Some(proj.as_path()));
    }

    #[test]
    fn a_global_npm_root_matches_so_the_walker_prunes_before_the_nested_copies() {
        // Regression: `lib/node_modules` has no `lib/package.json`. When that
        // gated detection, the walker descended and every installed package with its
        // own manifest matched instead, so one tree was reported as many nested
        // fragments. The outer directory must match on its own.
        let tmp = TempDir::new().unwrap();
        let outer = tmp.path().join("lib").join("node_modules");
        let inner = outer.join("some-pkg").join("node_modules");
        fs::create_dir_all(&inner).unwrap();
        touch(&outer.join("some-pkg").join("package.json"));

        assert_eq!(
            detect(&outer).map(|d| d.category),
            Some(Category::NodeModules),
            "the outer node_modules must match so the walk prunes there"
        );
    }

    #[test]
    fn target_requires_a_valid_cachedir_tag() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("crate");
        let target = proj.join("target");
        fs::create_dir_all(&target).unwrap();

        assert!(detect(&target).is_none(), "no CACHEDIR.TAG");

        write(
            &target.join("CACHEDIR.TAG"),
            "Signature: 0000000000000000000000000000000\nnope\n",
        );
        assert!(detect(&target).is_none(), "wrong signature");

        write(
            &target.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n\
             # This file is a cache directory tag created by cargo.\n",
        );
        let d = detect(&target).expect("valid tag should be detected");
        assert_eq!(d.category, Category::CargoTarget);
        assert_eq!(d.project.as_deref(), Some(proj.as_path()));
    }

    #[test]
    fn truncated_cachedir_tag_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        write(&target.join("CACHEDIR.TAG"), "Signature: 8a477f5");
        assert!(detect(&target).is_none());
    }

    #[test]
    fn pycache_is_detected() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("pkg");
        let cache = pkg.join("__pycache__");
        fs::create_dir_all(&cache).unwrap();

        let d = detect(&cache).unwrap();
        assert_eq!(d.category, Category::Pycache);
        assert_eq!(d.project.as_deref(), Some(pkg.as_path()));
    }

    #[test]
    fn skippable_covers_system_dirs_and_git() {
        assert!(is_skippable(Path::new("/proc")));
        assert!(is_skippable(Path::new("/sys")));
        assert!(is_skippable(Path::new("/dev")));
        assert!(is_skippable(Path::new("/run")));
        assert!(is_skippable(Path::new("/tmp")));
        assert!(is_skippable(Path::new("/home/u/repo/.git")));

        assert!(!is_skippable(Path::new("/home/u/proc")));
        assert!(!is_skippable(Path::new("/tmp/work")));
        assert!(!is_skippable(Path::new("/home/u/repo/.github")));
    }
}
