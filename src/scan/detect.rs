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
    // installed packages belonging to the directory above.
    //
    // Both match on the name alone, with or without a sibling manifest.
    // Requiring one used to gate `node_modules` detection, which had exactly
    // the opposite of the intended effect: a global npm root
    // (`lib/node_modules`, no `lib/package.json`) failed to match, the walker
    // descended, and each installed package inside matched on its own manifest
    // -- reporting one 1.8 GB tree as 108 nested fragments.
    //
    // `.pnpm` normally sits *inside* a `node_modules` and is never reached,
    // because the walk prunes there. pnpm's global install directory puts it
    // beside `node_modules` instead, where the walk went straight in and
    // reported one 153 MB store as 417 fragments of itself, one row per
    // installed package.
    //
    // The manifest now only identifies the owning project, for activity and
    // lockfile signals. Nesting is prevented by pruning in the walker, which
    // never descends into either directory regardless of what is returned here.
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
/// Each entry becomes a single `Cache` artifact and is pruned from the main
/// walk. Whole-tree granularity is the point: `~/.cache/pnpm/dlx` holds one
/// throwaway `node_modules` per `pnpm dlx` invocation, so walking into it
/// turned one cache into dozens of rows that describe nothing the user chose
/// to create and cannot be acted on individually.
///
/// `XDG_CACHE_HOME` and `XDG_DATA_HOME` are honoured when set to an absolute
/// path; otherwise the usual `~/.cache` and `~/.local/share` apply.
pub fn cache_roots() -> Vec<(PathBuf, Category)> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let cache_home = xdg_dir("XDG_CACHE_HOME", &home, ".cache");
    let data_home = xdg_dir("XDG_DATA_HOME", &home, ".local/share");
    cache_roots_in(&home, &cache_home, &data_home)
}

/// `$XDG_CACHE_HOME`, or `~/.cache`. `None` when there is no usable `$HOME`.
pub fn cache_home() -> Option<PathBuf> {
    home_dir().map(|home| xdg_dir("XDG_CACHE_HOME", &home, ".cache"))
}

/// The top-level cache directory `path` belongs to, if it is in one.
///
/// Everything under the cache home is regenerable by definition, so an artifact
/// found in there describes the tool that created it, not a directory the user
/// made: `~/.cache/pnpm/dlx/<hash>/pkg/node_modules` is one `pnpm dlx`
/// invocation out of dozens, and `~/.cache/node/corepack/v1/pnpm/<version>/
/// dist/node_modules` is one downloaded pnpm out of a dozen more. Listing each
/// separately buries the one directory a user would actually act on under rows
/// that individually mean nothing.
///
/// The cache home itself yields `None`: the buckets inside it are what get
/// reported, never the whole of `~/.cache` in one row.
pub fn cache_bucket(cache_home: &Path, path: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(cache_home).ok()?;
    let first = rest.components().next()?;
    match first {
        std::path::Component::Normal(name) => Some(cache_home.join(name)),
        _ => None,
    }
}

/// True when `path` is a top-level directory of the cache home, i.e. exactly
/// what [`cache_bucket`] hands back. Deletion re-proves cache artifacts against
/// this, since a bucket is discovered by the walk rather than listed up front.
pub fn is_cache_bucket(path: &Path) -> bool {
    cache_home().is_some_and(|home| cache_bucket(&home, path).as_deref() == Some(path))
}

fn home_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    (!home.as_os_str().is_empty()).then_some(home)
}

/// The cache roots for explicit base directories. Split out from
/// [`cache_roots`] so the layout is testable without touching the environment.
fn cache_roots_in(home: &Path, cache_home: &Path, data_home: &Path) -> Vec<(PathBuf, Category)> {
    /// Fixed locations, relative to `$HOME`.
    const UNDER_HOME: [&str; 5] = [
        ".cargo/registry",
        ".npm",
        ".conda/pkgs",
        ".bun/install/cache",
        ".yarn/berry/cache",
    ];
    /// Relative to `$XDG_CACHE_HOME`, default `~/.cache`.
    const UNDER_CACHE: [&str; 7] = [
        "uv",
        "pip",
        "pypoetry",
        "pdm",
        "huggingface",
        // pnpm keeps its store, its package metadata and the `dlx` scratch
        // trees here; all of it is regenerable.
        "pnpm",
        "yarn",
    ];
    /// Relative to `$XDG_DATA_HOME`, default `~/.local/share`.
    const UNDER_DATA: [&str; 1] = ["pnpm/store"];

    let candidates = UNDER_HOME
        .iter()
        .map(|rel| home.join(rel))
        .chain(UNDER_CACHE.iter().map(|rel| cache_home.join(rel)))
        .chain(UNDER_DATA.iter().map(|rel| data_home.join(rel)))
        .filter(|p| is_dir(p));

    outermost(candidates)
        .into_iter()
        .map(|p| (p, Category::Cache))
        .collect()
}

/// Resolve an XDG base directory. A relative or empty value is ignored, as the
/// spec requires, so a stray `XDG_CACHE_HOME=cache` cannot point the scanner at
/// a path relative to the working directory.
fn xdg_dir(var: &str, home: &Path, fallback: &str) -> PathBuf {
    match std::env::var_os(var).map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => home.join(fallback),
    }
}

/// Drop every path that lies inside another, keeping the outermost.
///
/// The base directories are configurable and can be pointed at each other
/// (`XDG_DATA_HOME=$HOME/.cache` nests `pnpm/store` inside `pnpm`). Two nested
/// cache roots would be emitted as two artifacts covering overlapping bytes,
/// double-counting the total and letting one deletion gut the other.
fn outermost(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut sorted: Vec<PathBuf> = paths.into_iter().collect();
    // Lexicographic order on components puts every ancestor before its descendants.
    sorted.sort();
    sorted.dedup();

    let mut kept: Vec<PathBuf> = Vec::new();
    for path in sorted {
        if kept.iter().any(|root| path.starts_with(root)) {
            continue;
        }
        kept.push(path);
    }
    kept
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

    /// Regression: pnpm's global install directory keeps its virtual store
    /// *beside* `node_modules` rather than inside it, so the walk descended and
    /// reported one 153 MB store as 417 rows -- one per installed package.
    #[test]
    fn a_pnpm_virtual_store_is_one_artifact_not_one_per_package() {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("global/5/.pnpm");
        for pkg in ["typescript@5.9.3", "lodash@4.17.23", "@types+node@25.4.0"] {
            fs::create_dir_all(store.join(pkg).join("node_modules")).unwrap();
        }

        let d = detect(&store).expect("a virtual store is an artifact on its own");
        assert_eq!(d.category, Category::NodeModules);
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

    /// Regression: `~/.cache/pnpm/dlx` holds one throwaway `node_modules` per
    /// `pnpm dlx` invocation. With no cache root covering it the walker
    /// descended and reported every one of them as a separate artifact, so a
    /// pure cache showed up as a pile of rows. It is one directory.
    #[test]
    fn the_pnpm_cache_is_one_root_that_swallows_its_dlx_node_modules() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let dlx = home.join(".cache/pnpm/dlx");
        for hash in ["0deb95f6", "1aa7da22", "36d0c130"] {
            fs::create_dir_all(dlx.join(hash).join("pkg").join("node_modules")).unwrap();
        }
        fs::create_dir_all(home.join(".cache/pnpm/v11")).unwrap();

        let roots = cache_roots_in(home, &home.join(".cache"), &home.join(".local/share"));
        let paths: Vec<&Path> = roots.iter().map(|(p, _)| p.as_path()).collect();

        assert_eq!(paths, vec![home.join(".cache/pnpm").as_path()]);
        assert!(roots.iter().all(|(_, c)| *c == Category::Cache));
    }

    #[test]
    fn cache_roots_skip_what_is_not_there() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".cargo/registry")).unwrap();
        fs::create_dir_all(home.join(".local/share/pnpm/store")).unwrap();
        // A file, not a directory: not a cache root.
        touch(&home.join(".npm"));

        let mut paths: Vec<PathBuf> =
            cache_roots_in(home, &home.join(".cache"), &home.join(".local/share"))
                .into_iter()
                .map(|(p, _)| p)
                .collect();
        paths.sort();

        assert_eq!(
            paths,
            vec![
                home.join(".cargo/registry"),
                home.join(".local/share/pnpm/store"),
            ]
        );
    }

    /// `XDG_DATA_HOME=$XDG_CACHE_HOME` nests `pnpm/store` inside `pnpm`. Two
    /// overlapping roots would count the same bytes twice and let one deletion
    /// gut the other, so only the outer one survives.
    #[test]
    fn nested_cache_roots_collapse_to_the_outermost() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let cache = home.join(".cache");
        fs::create_dir_all(cache.join("pnpm/store")).unwrap();

        let paths: Vec<PathBuf> = cache_roots_in(home, &cache, &cache)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(paths, vec![cache.join("pnpm")]);
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
            (
                "/home/u/.cache/node/corepack/v1/pnpm/10.29.2/dist/node_modules",
                "/home/u/.cache/node",
            ),
            (
                "/home/u/.cache/typescript/6.0/node_modules",
                "/home/u/.cache/typescript",
            ),
            (
                "/home/u/.cache/pre-commit/repoabc/py_env-python3",
                "/home/u/.cache/pre-commit",
            ),
            // A bucket handed its own path back is what `is_cache_bucket` checks.
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
    fn outermost_keeps_sibling_prefixes_apart() {
        // "/c/pnpm2" starts with the *string* "/c/pnpm" but is not inside it.
        let kept = outermost([
            PathBuf::from("/c/pnpm"),
            PathBuf::from("/c/pnpm2"),
            PathBuf::from("/c/pnpm/dlx"),
            PathBuf::from("/c/pnpm"),
        ]);
        assert_eq!(
            kept,
            vec![PathBuf::from("/c/pnpm"), PathBuf::from("/c/pnpm2")]
        );
    }

    #[test]
    fn xdg_dir_falls_back_to_home_when_the_variable_is_unset() {
        assert_eq!(
            xdg_dir("VENVPRUNER_XDG_TEST_UNSET", Path::new("/home/u"), ".cache"),
            PathBuf::from("/home/u/.cache")
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
