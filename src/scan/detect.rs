//! Directory classification. `detect` is called once per visited directory and
//! answers a single question: is *this* directory an artifact root?

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

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
/// `$PNPM_HOME/global`, wherever `$PNPM_HOME` happens to be.
const PNPM_GLOBAL: [&str; 2] = ["pnpm", "global"];

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

    // A `node_modules` and pnpm's `.pnpm` virtual store are the same thing: the
    // installed packages belonging to the directory above. Both match on the
    // name alone. Gating `node_modules` on a sibling manifest used to have the
    // opposite of the intended effect -- a global npm root (`lib/node_modules`,
    // no `lib/package.json`) failed to match, the walk descended, and every
    // package inside matched on its own manifest, reporting one 1.8 GB tree as
    // 108 fragments. `.pnpm` normally sits inside a `node_modules` and is never
    // reached; pnpm's global directory puts it beside one, where the same thing
    // happened: 417 rows for one store.
    //
    // The manifest now only names the owning project, for activity signals.
    if name == Some(OsStr::new("node_modules")) || name == Some(OsStr::new(".pnpm")) {
        // Under pnpm's global directory these hold installed CLI tools, the way
        // a uv tool or a pipx venv does: whatever is on `$PATH` resolves into
        // them, and the directory above is pnpm's own bookkeeping rather than a
        // project whose activity says anything about them. Reporting no project
        // keeps them off the `SAFE` badge for the same reason those two are:
        // the bytes are worth showing, the call is not ours to make.
        if has_component_run(dir, &PNPM_GLOBAL) {
            return Some(Detected::new(Category::NodeModules, None));
        }
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

/// Well-known package-manager cache roots, filtered to those that exist.
///
/// Each becomes a single `Cache` artifact and is pruned from the walk. The list
/// exists for caches that hold nothing [`detect`] would recognise -- without an
/// entry here, `~/.cache/uv` is just an opaque pile of files. Caches that *do*
/// hold recognisable artifacts are found by [`cache_bucket`] instead.
pub fn cache_roots() -> Vec<(PathBuf, Category)> {
    const UNDER_HOME: [&str; 5] = [
        ".cargo/registry",
        ".npm",
        ".conda/pkgs",
        ".local/share/pnpm/store",
        // Not under the cache home, so `cache_bucket` cannot reach it, and the
        // cached packages ship test fixtures with `node_modules` in them.
        ".bun/install/cache",
    ];
    const UNDER_CACHE: [&str; 6] = ["uv", "pip", "pypoetry", "pdm", "huggingface", "pnpm"];

    let (Some(home), Some(cache_home)) = (home_dir(), cache_home()) else {
        return Vec::new();
    };
    UNDER_HOME
        .iter()
        .map(|rel| home.join(rel))
        .chain(UNDER_CACHE.iter().map(|rel| cache_home.join(rel)))
        .filter(|p| is_dir(p))
        .map(|p| (p, Category::Cache))
        .collect()
}

/// `$XDG_CACHE_HOME` when it is absolute, else `~/.cache`. A relative value is
/// ignored as the XDG spec requires, so a stray `XDG_CACHE_HOME=cache` cannot
/// point the scanner at the working directory.
pub fn cache_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => Some(dir),
        _ => Some(home_dir()?.join(".cache")),
    }
}

/// The top-level cache directory `path` sits in, if it is in one.
///
/// Everything under the cache home is regenerable, so an artifact found there
/// describes the tool that created it rather than anything the user made:
/// `~/.cache/pnpm/dlx/<hash>/pkg/node_modules` is one `pnpm dlx` run out of
/// dozens. Reporting each one buries the directory a user would actually act on.
///
/// The cache home itself yields `None`; the directories inside it get reported,
/// never the whole of `~/.cache` in one row.
pub fn cache_bucket(cache_home: &Path, path: &Path) -> Option<PathBuf> {
    match path.strip_prefix(cache_home).ok()?.components().next()? {
        Component::Normal(name) => Some(cache_home.join(name)),
        _ => None,
    }
}

/// True when `path` is exactly what [`cache_bucket`] hands back. Deletion
/// re-proves cache artifacts against this, since a bucket is discovered by the
/// walk rather than listed up front.
pub fn is_cache_bucket(path: &Path) -> bool {
    cache_home().is_some_and(|home| cache_bucket(&home, path).as_deref() == Some(path))
}

fn home_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    (!home.as_os_str().is_empty()).then_some(home)
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

    /// A `.pnpm` under `$PNPM_HOME/global` holds installed CLI tools that
    /// `$PATH` resolves into, not build output of a project. Reporting no
    /// project is what keeps it off the `SAFE` badge, exactly as for a uv tool
    /// or a pipx venv -- the bytes get shown, the call stays with the user.
    #[test]
    fn pnpm_global_installs_report_no_owning_project() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join(".local/share/pnpm/global/5");
        fs::create_dir_all(global.join(".pnpm")).unwrap();
        fs::create_dir_all(global.join("node_modules")).unwrap();
        // pnpm writes both of these; they would otherwise read as "rebuildable".
        touch(&global.join("package.json"));
        touch(&global.join("pnpm-lock.yaml"));

        for dir in [global.join(".pnpm"), global.join("node_modules")] {
            let d = detect(&dir).unwrap();
            assert_eq!(d.category, Category::NodeModules);
            assert_eq!(d.project, None, "{}", dir.display());
        }
    }

    #[test]
    fn a_project_local_virtual_store_keeps_its_project() {
        // Not under pnpm's global dir: a `virtual-store-dir` pointed out of
        // `node_modules` still belongs to the project beside it.
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("app");
        let store = proj.join(".pnpm");
        fs::create_dir_all(&store).unwrap();
        touch(&proj.join("package.json"));

        let d = detect(&store).unwrap();
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

    /// The generalisation of the pnpm-dlx fix: every tool that shells out to a
    /// package manager gets its own directory under the cache home, and each
    /// one hides its own crop of `node_modules`. A curated list cannot keep up,
    /// so anything under the cache home collapses to the directory that owns it.
    #[test]
    fn artifacts_under_the_cache_home_collapse_to_their_top_level_directory() {
        let cache = Path::new("/home/u/.cache");
        for (found, bucket) in [
            (
                "/home/u/.cache/pnpm/dlx/0deb95f6/pkg/node_modules",
                "/home/u/.cache/pnpm",
            ),
            // A directory that is already a bucket answers with itself, which is
            // what `is_cache_bucket` leans on.
            ("/home/u/.cache/opencode", "/home/u/.cache/opencode"),
        ] {
            assert_eq!(
                cache_bucket(cache, Path::new(found)).as_deref(),
                Some(Path::new(bucket)),
                "{found}"
            );
        }
    }

    #[test]
    fn the_cache_home_itself_is_not_a_bucket() {
        let cache = Path::new("/home/u/.cache");
        // Reporting one row for the whole of ~/.cache is not a useful offer.
        assert_eq!(cache_bucket(cache, cache), None);
        // Nor is anything outside it a bucket, including a sibling whose name
        // merely starts with the same string.
        assert_eq!(
            cache_bucket(cache, Path::new("/home/u/code/app/node_modules")),
            None
        );
        assert_eq!(
            cache_bucket(cache, Path::new("/home/u/.cachexyz/pnpm")),
            None
        );
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
