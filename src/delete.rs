//! Deletion with a paranoid guard in front of it.
//!
//! Every removal goes through [`check_guard`], which re-verifies on the live
//! filesystem that the target is still the artifact that was scanned. The scan
//! and the keypress that deletes are separated by an arbitrary amount of time,
//! so nothing discovered during the scan is trusted here.

use std::path::{Component, Path, PathBuf};

use crate::model::{Artifact, Category};
use crate::scan::detect;
use crate::scan::size::Sizer;

/// Minimum number of `Normal` path components a deletable path must have.
/// `/`, `/home` and `/home/user` all fall below this.
const MIN_DEPTH: usize = 3;

/// What actually happened to one artifact.
///
/// A `Pycache` group is several directories, so "it worked" and "it failed" are
/// not the only answers: some children can go while others survive. Collapsing
/// that into `Err` loses the bytes that really were freed and leaves the caller
/// believing the artifact is intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Bytes the caller may count as reclaimed.
    pub freed: u64,
    /// True when nothing of the artifact is left on disk, so the caller may
    /// drop it from the list. False means part of it survived.
    pub complete: bool,
    /// Set when part of the artifact survived; describes what did not go.
    pub error: Option<String>,
}

/// Returns the canonical path that passed every check; removal must use it.
pub fn check_guard(path: &Path, roots: &[PathBuf], expected: Category) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| !h.as_os_str().is_empty());
    let cwd = std::env::current_dir()
        .map_err(|e| format!("refusing to delete: cannot determine current directory: {e}"))?;
    let cache_roots: Vec<PathBuf> = detect::cache_roots()
        .into_iter()
        .map(|(p, _)| canon(&p))
        .collect();
    check_guard_inner(path, roots, expected, home.as_deref(), &cwd, &cache_roots)
}

/// Returns the canonical path that passed the guard. Removal must use *that*
/// path, not the caller's: between `canonicalize` and the `unlink` a parent
/// component could be re-pointed, and only the resolved path is the directory
/// every check above actually inspected.
fn check_guard_inner(
    path: &Path,
    roots: &[PathBuf],
    expected: Category,
    home: Option<&Path>,
    cwd: &Path,
    cache_roots: &[PathBuf],
) -> Result<PathBuf, String> {
    let shown = path.display();

    if !path.is_absolute() {
        return Err(format!("refusing to delete {shown}: path is not absolute"));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("refusing to delete {shown}: path contains `..`"));
    }
    if normal_depth(path) < MIN_DEPTH {
        return Err(format!(
            "refusing to delete {shown}: path is too close to the filesystem root"
        ));
    }

    let meta = path
        .symlink_metadata()
        .map_err(|e| format!("refusing to delete {shown}: cannot stat path: {e}"))?;
    if meta.is_symlink() {
        return Err(format!(
            "refusing to delete {shown}: path is a symlink; never deleting through one"
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "refusing to delete {shown}: path is not a directory"
        ));
    }

    // Resolve once, so every comparison below is against the real location even
    // if a parent component is a symlink.
    let real = path
        .canonicalize()
        .map_err(|e| format!("refusing to delete {shown}: cannot resolve path: {e}"))?;
    if normal_depth(&real) < MIN_DEPTH {
        return Err(format!(
            "refusing to delete {shown}: resolves to {}, too close to the filesystem root",
            real.display()
        ));
    }

    let real_home = home.map(canon);
    check_home_rules(&real, real_home.as_deref(), cache_roots)
        .map_err(|why| format!("refusing to delete {shown}: {why}"))?;

    let real_cwd = canon(cwd);
    if real_cwd.starts_with(&real) {
        return Err(format!(
            "refusing to delete {shown}: it is the current directory or an ancestor of it"
        ));
    }

    if !roots.iter().any(|r| real.starts_with(canon(r))) {
        return Err(format!(
            "refusing to delete {shown}: outside every scanned root"
        ));
    }

    verify_category(&real, expected, cache_roots)?;
    Ok(real)
}

/// `$HOME` itself, its ancestors, and its direct children are off limits — the
/// direct children being the XDG-ish top levels (`~/.cache`, `~/.local`,
/// `~/Documents`). Known cache roots stay deletable; they are the whole point.
fn check_home_rules(
    real: &Path,
    home: Option<&Path>,
    cache_roots: &[PathBuf],
) -> Result<(), String> {
    // No `$HOME` means these rules cannot be evaluated at all. Skipping them
    // would silently drop the protection for the home directory, its ancestors
    // and its top-level children exactly when the environment is least
    // ordinary; refuse instead and say why.
    let Some(home) = home else {
        return Err(
            "$HOME is not set, so the home-directory protections cannot be checked".to_string(),
        );
    };
    if real == home {
        return Err("that is the home directory".to_string());
    }
    if home.starts_with(real) {
        return Err("that is an ancestor of the home directory".to_string());
    }
    if real.parent() == Some(home) && !cache_roots.iter().any(|c| c == real) {
        return Err(format!(
            "{} is a top-level directory in $HOME and not a known cache root",
            real.display()
        ));
    }
    Ok(())
}

/// The filesystem may have changed since the scan. Refuse unless the path still
/// looks like the category we recorded.
///
/// Two categories cannot be re-verified by [`detect::detect`] and are checked
/// against their own source of truth instead. Sending them through `detect`
/// would reject every one of them, since it never yields either variant.
fn verify_category(path: &Path, expected: Category, cache_roots: &[PathBuf]) -> Result<(), String> {
    let shown = path.display();
    if expected == Category::Pycache {
        // A Pycache artifact carries the *project* directory, which detect()
        // deliberately does not classify. Only the `__pycache__` directories
        // beneath it are ever removed, each guarded separately.
        if !path.is_dir() {
            return Err(format!(
                "refusing to delete {shown}: project directory no longer exists"
            ));
        }
        return Ok(());
    }
    if expected == Category::Cache {
        // Cache artifacts come from `detect::cache_roots()` -- the curated list
        // of package-manager cache directories -- or from a top-level directory
        // of the cache home that the walk found artifacts inside. Those two
        // sources are what re-prove them. `detect()` has no arm that yields
        // `Category::Cache` at all, so routing them through it rejected every
        // single one.
        if !cache_roots.iter().any(|c| c == path) && !detect::is_cache_bucket(path) {
            return Err(format!(
                "refusing to delete {shown}: it is not a known package-manager cache root"
            ));
        }
        if !path.is_dir() {
            return Err(format!(
                "refusing to delete {shown}: cache directory no longer exists"
            ));
        }
        return Ok(());
    }
    match detect::detect(path) {
        Some(d) if d.category == expected => Ok(()),
        Some(d) => Err(format!(
            "refusing to delete {shown}: it now looks like a {} artifact, not a {}",
            d.category.label(),
            expected.label()
        )),
        None => Err(format!(
            "refusing to delete {shown}: it no longer looks like a {} artifact",
            expected.label()
        )),
    }
}

/// Remove one artifact.
///
/// `Err` means nothing at all was removed. `Ok` may still carry an `error`:
/// see [`Outcome`].
pub fn delete(artifact: &Artifact, roots: &[PathBuf]) -> Result<Outcome, String> {
    let real = check_guard(&artifact.path, roots, artifact.category)?;

    match artifact.category {
        // Never falls through: `artifact.path` is the project directory here.
        Category::Pycache => delete_pycache(artifact, &real),
        Category::PyVenv
        | Category::UvTool
        | Category::Pipx
        | Category::CondaEnv
        | Category::NodeModules
        | Category::CargoTarget
        | Category::Cache => {
            // `real`, not `artifact.path`: remove the directory the guard
            // inspected, so re-pointing a parent symlink in between cannot
            // redirect the removal.
            std::fs::remove_dir_all(&real)
                .map_err(|e| format!("failed to delete {}: {e}", artifact.path.display()))?;
            Ok(Outcome {
                freed: artifact.sizes.reclaim,
                complete: true,
                error: None,
            })
        }
    }
}

fn delete_pycache(artifact: &Artifact, real_project: &Path) -> Result<Outcome, String> {
    if artifact.children.is_empty() {
        return Err(format!(
            "refusing to delete {}: no __pycache__ directories were recorded for it",
            artifact.path.display()
        ));
    }

    let mut errors = Vec::new();
    let mut removed_bytes: u64 = 0;
    let mut removed = 0usize;
    // Each child is measured just before it goes, so a run that only gets
    // partway through can still say how much it freed. The artifact's own
    // recorded reclaim describes the whole group and would be a lie there.
    // The extra walk is over `__pycache__` directories that `remove_dir_all`
    // is about to walk anyway.
    let sizer = Sizer::new();

    // Exactly the directories the scan sized and the confirmation counted --
    // never a fresh discovery, which would remove things the user never saw.
    for child in &artifact.children {
        let real_child = match check_pycache_child(child, real_project) {
            Ok(real_child) => real_child,
            Err(why) => {
                errors.push(why);
                continue;
            }
        };
        let bytes = sizer.measure(&real_child).reclaim;
        match std::fs::remove_dir_all(&real_child) {
            Ok(()) => {
                removed += 1;
                removed_bytes = removed_bytes.saturating_add(bytes);
            }
            Err(e) => errors.push(format!("failed to delete {}: {e}", child.display())),
        }
    }

    if errors.is_empty() {
        return Ok(Outcome {
            // Every recorded child went, so the figure the user confirmed is
            // the figure that was freed.
            freed: artifact.sizes.reclaim,
            complete: true,
            error: None,
        });
    }
    if removed == 0 {
        return Err(errors.join("; "));
    }
    Ok(Outcome {
        freed: removed_bytes,
        complete: false,
        error: Some(format!(
            "removed {removed} of {} __pycache__ directories: {}",
            artifact.children.len(),
            errors.join("; ")
        )),
    })
}

/// Returns the canonical child, which is what removal must use.
fn check_pycache_child(child: &Path, real_project: &Path) -> Result<PathBuf, String> {
    let shown = child.display();
    if !child.is_absolute() {
        return Err(format!("refusing to delete {shown}: path is not absolute"));
    }
    if child.file_name().and_then(|n| n.to_str()) != Some("__pycache__") {
        return Err(format!(
            "refusing to delete {shown}: not a __pycache__ directory"
        ));
    }
    let meta = child
        .symlink_metadata()
        .map_err(|e| format!("refusing to delete {shown}: cannot stat path: {e}"))?;
    if meta.is_symlink() {
        return Err(format!("refusing to delete {shown}: path is a symlink"));
    }
    if !meta.is_dir() {
        return Err(format!(
            "refusing to delete {shown}: path is not a directory"
        ));
    }
    let real = child
        .canonicalize()
        .map_err(|e| format!("refusing to delete {shown}: cannot resolve path: {e}"))?;
    if real == real_project || !real.starts_with(real_project) {
        return Err(format!(
            "refusing to delete {shown}: not inside {}",
            real_project.display()
        ));
    }
    Ok(real)
}

fn normal_depth(path: &Path) -> usize {
    path.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count()
}

fn canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn venv_at(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("lib/python3.12/site-packages")).unwrap();
        fs::write(dir.join("bin/python"), "").unwrap();
        fs::write(
            dir.join("pyvenv.cfg"),
            "home = /usr/bin\nversion_info = 3.12.11\n",
        )
        .unwrap();
        dir.to_path_buf()
    }

    /// A scratch layout: `<tmp>/root` is the scan root, `<tmp>/home` stands in
    /// for `$HOME`, `<tmp>/elsewhere` is off-limits territory.
    struct Fixture {
        _tmp: TempDir,
        base: PathBuf,
        root: PathBuf,
        home: PathBuf,
        cwd: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = TempDir::new().unwrap();
            let base = tmp.path().canonicalize().unwrap();
            let root = base.join("root");
            let home = base.join("home");
            let cwd = base.join("cwd");
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&cwd).unwrap();
            fs::create_dir_all(base.join("elsewhere")).unwrap();
            Fixture {
                _tmp: tmp,
                base,
                root,
                home,
                cwd,
            }
        }

        fn guard(&self, path: &Path, expected: Category) -> Result<PathBuf, String> {
            check_guard_inner(
                path,
                std::slice::from_ref(&self.root),
                expected,
                Some(&self.home),
                &self.cwd,
                &[],
            )
        }
    }

    #[test]
    fn accepts_a_real_venv_inside_a_root() {
        let f = Fixture::new();
        let venv = venv_at(&f.root.join("project/.venv"));
        assert_eq!(f.guard(&venv, Category::PyVenv), Ok(venv.clone()));
    }

    #[test]
    fn accepts_a_known_cache_root() {
        // `detect()` never returns Category::Cache, so routing cache artifacts
        // through it rejected every single one of them.
        let f = Fixture::new();
        let cache = f.root.join("home/.cache/uv");
        fs::create_dir_all(&cache).unwrap();

        assert_eq!(
            check_guard_inner(
                &cache,
                std::slice::from_ref(&f.root),
                Category::Cache,
                Some(&f.home),
                &f.cwd,
                std::slice::from_ref(&cache),
            ),
            Ok(cache.clone()),
            "a live cache root must stay deletable"
        );
    }

    #[test]
    fn refuses_a_cache_artifact_that_is_not_a_known_cache_root() {
        let f = Fixture::new();
        let other = f.root.join("project/some-dir");
        fs::create_dir_all(&other).unwrap();
        let known = f.root.join("home/.npm");
        fs::create_dir_all(&known).unwrap();

        let err = check_guard_inner(
            &other,
            std::slice::from_ref(&f.root),
            Category::Cache,
            Some(&f.home),
            &f.cwd,
            std::slice::from_ref(&known),
        )
        .unwrap_err();
        assert!(
            err.contains("not a known package-manager cache root"),
            "{err}"
        );
    }

    #[test]
    fn refuses_everything_when_home_is_unknown() {
        // Without $HOME the home-directory rules cannot be evaluated; skipping
        // them silently would drop the protection instead of reporting it.
        let f = Fixture::new();
        let venv = venv_at(&f.root.join("project/.venv"));
        let err = check_guard_inner(
            &venv,
            std::slice::from_ref(&f.root),
            Category::PyVenv,
            None,
            &f.cwd,
            &[],
        )
        .unwrap_err();
        assert!(err.contains("$HOME is not set"), "{err}");

        assert!(check_home_rules(&venv, None, &[]).is_err());
    }

    #[test]
    fn refuses_relative_path() {
        let f = Fixture::new();
        let err = f
            .guard(Path::new("project/.venv"), Category::PyVenv)
            .unwrap_err();
        assert!(err.contains("not absolute"), "{err}");
    }

    #[test]
    fn refuses_parent_dir_component() {
        let f = Fixture::new();
        venv_at(&f.root.join("project/.venv"));
        let sneaky = f.root.join("project/../project/.venv");
        let err = f.guard(&sneaky, Category::PyVenv).unwrap_err();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn refuses_filesystem_root_and_shallow_paths() {
        let f = Fixture::new();
        for p in ["/", "/home", "/home/somebody"] {
            let err = f.guard(Path::new(p), Category::PyVenv).unwrap_err();
            assert!(
                err.contains("too close to the filesystem root"),
                "{p}: {err}"
            );
        }
    }

    #[test]
    fn refuses_home_itself() {
        let f = Fixture::new();
        let err = check_guard_inner(
            &f.home,
            std::slice::from_ref(&f.base),
            Category::PyVenv,
            Some(&f.home),
            &f.cwd,
            &[],
        )
        .unwrap_err();
        assert!(err.contains("home directory"), "{err}");
    }

    #[test]
    fn refuses_direct_child_of_home_unless_cache_root() {
        let f = Fixture::new();
        let cache = f.home.join(".npm");
        fs::create_dir_all(&cache).unwrap();

        let err = check_guard_inner(
            &cache,
            std::slice::from_ref(&f.base),
            Category::Cache,
            Some(&f.home),
            &f.cwd,
            &[],
        )
        .unwrap_err();
        assert!(err.contains("top-level directory in $HOME"), "{err}");

        // The same path passes the $HOME rule once it is a known cache root.
        assert_eq!(
            check_home_rules(&cache, Some(&f.home), std::slice::from_ref(&cache)),
            Ok(())
        );
    }

    #[test]
    fn refuses_ancestor_of_cwd() {
        let f = Fixture::new();
        let nested = f.root.join("project/.venv");
        venv_at(&nested);
        let inside = nested.join("lib");
        let err = check_guard_inner(
            &nested,
            std::slice::from_ref(&f.root),
            Category::PyVenv,
            Some(&f.home),
            &inside,
            &[],
        )
        .unwrap_err();
        assert!(err.contains("current directory"), "{err}");
    }

    #[test]
    fn refuses_path_outside_roots() {
        let f = Fixture::new();
        let venv = venv_at(&f.base.join("elsewhere/.venv"));
        let err = f.guard(&venv, Category::PyVenv).unwrap_err();
        assert!(err.contains("outside every scanned root"), "{err}");
    }

    #[test]
    fn root_containment_is_not_a_string_prefix() {
        let f = Fixture::new();
        fs::create_dir_all(f.base.join("p4b")).unwrap();
        let venv = venv_at(&f.base.join("p4bevil/.venv"));
        let err = check_guard_inner(
            &venv,
            &[f.base.join("p4b")],
            Category::PyVenv,
            Some(&f.home),
            &f.cwd,
            &[],
        )
        .unwrap_err();
        assert!(err.contains("outside every scanned root"), "{err}");
    }

    #[test]
    fn refuses_symlink_to_a_real_venv() {
        let f = Fixture::new();
        let real = venv_at(&f.base.join("elsewhere/real-venv"));
        let link = f.root.join("project-link-venv");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = f.guard(&link, Category::PyVenv).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(real.join("pyvenv.cfg").exists());
    }

    #[test]
    fn refuses_when_category_no_longer_matches() {
        let f = Fixture::new();
        let venv = venv_at(&f.root.join("project/.venv"));
        let err = f.guard(&venv, Category::NodeModules).unwrap_err();
        assert!(err.contains("node"), "{err}");
    }

    #[test]
    fn refuses_when_artifact_vanished() {
        let f = Fixture::new();
        let gone = f.root.join("project/.venv");
        let err = f.guard(&gone, Category::PyVenv).unwrap_err();
        assert!(err.contains("cannot stat path"), "{err}");
    }

    #[test]
    fn pycache_child_guard_rejects_outsiders() {
        let f = Fixture::new();
        let project = f.root.join("project");
        let good = project.join("pkg/__pycache__");
        fs::create_dir_all(&good).unwrap();
        let outside = f.base.join("elsewhere/__pycache__");
        fs::create_dir_all(&outside).unwrap();
        let wrong_name = project.join("pkg/site-packages");
        fs::create_dir_all(&wrong_name).unwrap();

        assert_eq!(check_pycache_child(&good, &project), Ok(good.clone()));
        assert!(check_pycache_child(&outside, &project).is_err());
        assert!(check_pycache_child(&wrong_name, &project).is_err());
        assert!(check_pycache_child(&project, &project).is_err());
    }

    /// Build a Pycache artifact whose `children` are the recorded directories.
    fn pycache_artifact(project: &Path, children: Vec<PathBuf>) -> Artifact {
        let mut a = Artifact::new(
            7,
            project.to_path_buf(),
            Category::Pycache,
            Some(project.to_path_buf()),
        );
        a.children = children;
        a.sizes.reclaim = 4096;
        a
    }

    #[test]
    fn pycache_deletion_removes_only_the_recorded_children() {
        // Anything created after the scan was never sized, never shown and
        // never confirmed, so it must survive.
        let f = Fixture::new();
        let project = f.root.join("project");
        let recorded = project.join("pkg/__pycache__");
        fs::create_dir_all(&recorded).unwrap();
        fs::write(recorded.join("m.pyc"), b"x").unwrap();
        let after_the_scan = project.join("late/__pycache__");
        fs::create_dir_all(&after_the_scan).unwrap();
        fs::write(project.join("main.py"), b"print()").unwrap();

        let artifact = pycache_artifact(&project, vec![recorded.clone()]);
        let out = delete(&artifact, std::slice::from_ref(&f.root)).unwrap();

        assert_eq!(out.freed, 4096);
        assert!(out.complete);
        assert_eq!(out.error, None);
        assert!(!recorded.exists());
        assert!(
            after_the_scan.exists(),
            "an unrecorded __pycache__ must not be swept up"
        );
        assert!(project.join("main.py").exists());
        assert!(project.exists(), "the project itself is never removed");
    }

    #[test]
    fn pycache_deletion_reaches_a_child_deeper_than_any_recursion_limit() {
        let f = Fixture::new();
        let project = f.root.join("project");
        let mut deep = project.clone();
        for i in 0..70 {
            deep = deep.join(format!("l{i}"));
        }
        let deep_cache = deep.join("__pycache__");
        fs::create_dir_all(&deep_cache).unwrap();
        fs::write(deep_cache.join("m.pyc"), b"x").unwrap();

        let artifact = pycache_artifact(&project, vec![deep_cache.clone()]);
        let out = delete(&artifact, std::slice::from_ref(&f.root)).unwrap();

        assert!(out.complete);
        assert!(
            !deep_cache.exists(),
            "a directory whose bytes were counted must actually be removed"
        );
    }

    #[test]
    fn a_partial_pycache_deletion_is_reported_as_incomplete_not_as_untouched() {
        let f = Fixture::new();
        let project = f.root.join("project");
        let good = project.join("pkg/__pycache__");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join("m.pyc"), b"x".repeat(4096)).unwrap();
        // A recorded child that has since vanished fails its own guard.
        let gone = project.join("missing/__pycache__");

        let artifact = pycache_artifact(&project, vec![good.clone(), gone.clone()]);
        let out = delete(&artifact, std::slice::from_ref(&f.root)).unwrap();

        assert!(!good.exists());
        assert!(!out.complete, "part of the artifact survived");
        let why = out.error.expect("a partial deletion must explain itself");
        assert!(why.contains("removed 1 of 2"), "{why}");
        assert!(
            out.freed > 0,
            "bytes that really were freed must still be credited"
        );
    }

    #[test]
    fn a_pycache_artifact_with_no_recorded_children_deletes_nothing() {
        let f = Fixture::new();
        let project = f.root.join("project");
        fs::create_dir_all(project.join("pkg/__pycache__")).unwrap();

        let artifact = pycache_artifact(&project, Vec::new());
        assert!(delete(&artifact, std::slice::from_ref(&f.root)).is_err());
        assert!(project.join("pkg/__pycache__").exists());
    }

    #[test]
    fn delete_removes_the_directory_the_guard_inspected() {
        let f = Fixture::new();
        let venv = venv_at(&f.root.join("project/.venv"));
        let mut artifact = Artifact::new(1, venv.clone(), Category::PyVenv, None);
        artifact.sizes.reclaim = 1234;

        let out = delete(&artifact, std::slice::from_ref(&f.root)).unwrap();
        assert_eq!(out.freed, 1234);
        assert!(out.complete);
        assert!(!venv.exists());
    }
}
