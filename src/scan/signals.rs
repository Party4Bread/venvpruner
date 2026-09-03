//! Cheap, per-artifact evidence gathering and the badge rules derived from it.

use crate::model::{Artifact, Badge, Category, Signals};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Upper bound on directory entries examined while looking for project activity.
const MAX_PROJECT_ENTRIES: usize = 2000;
/// `.git/refs/heads` is tiny; this only guards against a pathological repo.
const MAX_GIT_ENTRIES: usize = 512;
const MAX_REF_DEPTH: usize = 8;
const MAX_CFG_BYTES: u64 = 64 * 1024;

/// Directory names that are themselves artifacts, so their mtimes say nothing
/// about whether a human touched the project.
const SKIP_DIRS: [&str; 6] = [
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
];

/// Lock/manifest files that make an artifact reconstructible, in report order.
const REINSTALL_FILES: [&str; 11] = [
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "pyproject.toml",
    "requirements.txt",
    "environment.yml",
    "environment.yaml",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
];

pub fn gather(artifact: &Artifact) -> Signals {
    let mut signals = Signals {
        artifact_mtime: mtime(&artifact.path),
        ..Signals::default()
    };

    if let Some(project) = artifact.project.as_deref() {
        scan_project(project, &artifact.path, &mut signals);
        signals.git_mtime = git_mtime(project);
    }

    if is_venv(artifact.category)
        && let Some(cfg) = read_cfg(&artifact.path.join("pyvenv.cfg"))
    {
        signals.python_version = cfg_get(&cfg, "version_info");
        if let Some(home) = home_from_cfg(&cfg) {
            signals.interpreter_missing = Some(fs::metadata(&home).is_err());
        }
    }

    signals
}

pub fn derive_badge(
    a: &Artifact,
    s: &Signals,
    now: SystemTime,
    stale_after: Duration,
) -> (Badge, Vec<String>) {
    let mut reasons = Vec::new();

    if s.interpreter_missing == Some(true) {
        match venv_home(a) {
            Some(home) => reasons.push(format!("base interpreter {home} no longer exists")),
            None => {
                reasons.push("base interpreter recorded in pyvenv.cfg no longer exists".to_string())
            }
        }
        if let Some(v) = &s.python_version {
            reasons.push(format!("python {v} environment can no longer be activated"));
        }
        return (Badge::Dead, reasons);
    }

    let activity = match (s.project_mtime, s.git_mtime) {
        (Some(p), Some(g)) => Some(p.max(g)),
        (p, g) => p.or(g),
    };
    let age = activity.map(|t| now.duration_since(t).unwrap_or_default());

    if let Some(age) = age
        && age < stale_after
    {
        reasons.push(format!("project modified {}", when(age)));
        return (Badge::Active, reasons);
    }

    if a.category == Category::Cache {
        reasons.push("package cache; contents are re-downloadable".to_string());
        return (Badge::Safe, reasons);
    }

    if !s.reinstall_from.is_empty() {
        match age {
            Some(age) => reasons.push(format!("no project activity for {}", humanize_age(age))),
            None => reasons.push("no project activity signal found".to_string()),
        }
        reasons.push(format!(
            "can be rebuilt from {}",
            s.reinstall_from.join(", ")
        ));
        return (Badge::Safe, reasons);
    }

    reasons.push(
        "no lockfile or manifest found next to it — rebuilding may not be reproducible".to_string(),
    );
    if let Some(age) = age {
        reasons.push(format!("project last modified {}", when(age)));
    } else if let Some(t) = s.artifact_mtime {
        let age = now.duration_since(t).unwrap_or_default();
        reasons.push(format!("last modified {}", when(age)));
    }
    (Badge::Review, reasons)
}

/// Compact age for tables: `today`, `3d`, `5mo`, `2y`.
pub fn humanize_age(d: Duration) -> String {
    let days = d.as_secs() / 86_400;
    if days == 0 {
        "today".to_string()
    } else if days < 30 {
        format!("{days}d")
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

fn when(d: Duration) -> String {
    let age = humanize_age(d);
    if age == "today" {
        age
    } else {
        format!("{age} ago")
    }
}

fn is_venv(c: Category) -> bool {
    matches!(c, Category::PyVenv | Category::UvTool | Category::Pipx)
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path).ok()?.modified().ok()
}

/// Keep `md`'s mtime if it is the newest seen. Unreadable mtimes are dropped:
/// there is nothing to compare, and a missing one must not read as "now".
fn keep_newest_of(slot: &mut Option<SystemTime>, md: &fs::Metadata) {
    if let Ok(t) = md.modified() {
        keep_newest(slot, t);
    }
}

fn keep_newest(slot: &mut Option<SystemTime>, t: SystemTime) {
    if slot.is_none_or(|cur| t > cur) {
        *slot = Some(t);
    }
}

/// True for directories whose contents are themselves build/install output.
fn is_skipped_dir(name: &str, path: &Path) -> bool {
    SKIP_DIRS.contains(&name) || fs::symlink_metadata(path.join("pyvenv.cfg")).is_ok()
}

/// Newest mtime under `project` at depth <= 2, plus the lock/manifest files
/// sitting directly in it. The artifact itself and sibling artifacts are skipped.
/// True when an entry's mtime counts as someone working on the project.
///
/// Both passes below ask exactly this, and asking it in one place is what keeps
/// them from drifting apart. Symlinks are excluded because their mtime tracks
/// the link, not the work; the artifact being judged, because it cannot be
/// evidence for its own staleness; and other build output nearby, because a
/// rebuild is not a human editing the project.
fn counts_as_activity(path: &Path, name: &str, md: &fs::Metadata, artifact: &Path) -> bool {
    if md.file_type().is_symlink() {
        return false;
    }
    if !md.is_dir() {
        return true;
    }
    path != artifact && !is_skipped_dir(name, path)
}

fn scan_project(project: &Path, artifact: &Path, signals: &mut Signals) {
    let Ok(entries) = fs::read_dir(project) else {
        return;
    };

    let mut budget = MAX_PROJECT_ENTRIES;
    let mut newest: Option<SystemTime> = None;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut names: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        if budget == 0 {
            break;
        }
        budget -= 1;

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // Every name is collected, symlinked and unreadable ones included: a
        // lockfile still identifies what the artifact rebuilds from even when
        // its mtime cannot be trusted.
        let Ok(md) = entry.metadata() else {
            names.push(name);
            continue;
        };
        if counts_as_activity(&path, &name, &md, artifact) {
            keep_newest_of(&mut newest, &md);
            if md.is_dir() {
                subdirs.push(path);
            }
        }
        names.push(name);
    }

    for dir in subdirs {
        if budget == 0 {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if budget == 0 {
                break;
            }
            budget -= 1;

            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(md) = entry.metadata() else { continue };
            if counts_as_activity(&path, &name, &md, artifact) {
                keep_newest_of(&mut newest, &md);
            }
        }
    }

    signals.project_mtime = newest;
    signals.reinstall_from = reinstall_from(&names);
}

fn reinstall_from(names: &[String]) -> Vec<String> {
    let mut found: Vec<String> = REINSTALL_FILES
        .iter()
        .filter(|known| names.iter().any(|n| n == *known))
        .map(|known| (*known).to_string())
        .collect();

    let mut extra: Vec<String> = names
        .iter()
        .filter(|n| n.starts_with("requirements") && n.ends_with(".txt"))
        .filter(|n| !found.iter().any(|f| f == *n))
        .cloned()
        .collect();
    extra.sort();
    extra.dedup();
    found.extend(extra);
    found
}

fn git_mtime(project: &Path) -> Option<SystemTime> {
    let git = project.join(".git");
    let md = fs::symlink_metadata(&git).ok()?;
    if !md.is_dir() {
        // Worktree or submodule: the `.git` file's own mtime is the best proxy.
        return md.modified().ok();
    }

    let mut newest = None;
    let mut budget = MAX_GIT_ENTRIES;
    walk_refs(&git.join("refs").join("heads"), 0, &mut budget, &mut newest);
    if newest.is_none() {
        newest = mtime(&git.join("HEAD"));
    }
    newest
}

fn walk_refs(dir: &Path, depth: usize, budget: &mut usize, newest: &mut Option<SystemTime>) {
    if depth > MAX_REF_DEPTH || *budget == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let Ok(md) = entry.metadata() else { continue };
        if md.file_type().is_symlink() {
            continue;
        }
        if md.is_dir() {
            walk_refs(&entry.path(), depth + 1, budget, newest);
        } else if let Ok(t) = md.modified() {
            keep_newest(newest, t);
        }
    }
}

fn read_cfg(path: &Path) -> Option<String> {
    let md = fs::symlink_metadata(path).ok()?;
    if !md.is_file() || md.len() > MAX_CFG_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn norm_key(k: &str) -> String {
    k.trim().to_ascii_lowercase().replace('_', "-")
}

fn cfg_get(text: &str, key: &str) -> Option<String> {
    let want = norm_key(key);
    text.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        if norm_key(k) != want {
            return None;
        }
        let v = v.trim();
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// The directory (or executable) holding the base interpreter, as recorded by
/// `venv` (`home`) or `virtualenv` (`base-prefix` / `base-executable`).
fn home_from_cfg(cfg: &str) -> Option<String> {
    cfg_get(cfg, "home")
        .or_else(|| cfg_get(cfg, "base-prefix"))
        .or_else(|| cfg_get(cfg, "base-executable"))
}

fn venv_home(a: &Artifact) -> Option<String> {
    if !is_venv(a.category) {
        return None;
    }
    home_from_cfg(&read_cfg(&a.path.join("pyvenv.cfg"))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    const STALE: Duration = Duration::from_secs(30 * 86_400);

    fn venv(project: &Path, cfg_body: &str) -> Artifact {
        let path = project.join(".venv");
        fs::create_dir_all(&path).unwrap();
        let mut f = File::create(path.join("pyvenv.cfg")).unwrap();
        f.write_all(cfg_body.as_bytes()).unwrap();
        Artifact::new(1, path, Category::PyVenv, Some(project.to_path_buf()))
    }

    #[test]
    fn missing_interpreter_is_dead() {
        let tmp = tempdir().unwrap();
        let gone = tmp.path().join("no-such-python-root");
        let a = venv(
            tmp.path(),
            &format!("home = {}\nversion_info = 3.12.11\n", gone.display()),
        );

        let s = gather(&a);
        assert_eq!(s.interpreter_missing, Some(true));
        assert_eq!(s.python_version.as_deref(), Some("3.12.11"));

        let (badge, reasons) = derive_badge(&a, &s, SystemTime::now(), STALE);
        assert_eq!(badge, Badge::Dead);
        assert!(reasons.iter().any(|r| r.contains("no-such-python-root")));
    }

    #[test]
    fn unparseable_home_stays_undetermined() {
        let tmp = tempdir().unwrap();
        let a = venv(
            tmp.path(),
            "include-system-site-packages = false\nversion_info = 3.11.0\n",
        );

        let s = gather(&a);
        assert_eq!(s.interpreter_missing, None);
        assert_eq!(s.python_version.as_deref(), Some("3.11.0"));
    }

    #[test]
    fn present_interpreter_is_not_missing() {
        let tmp = tempdir().unwrap();
        let a = venv(
            tmp.path(),
            &format!("home = {}\n", tmp.path().join("usr-bin").display()),
        );
        fs::create_dir_all(tmp.path().join("usr-bin")).unwrap();

        assert_eq!(gather(&a).interpreter_missing, Some(false));
    }

    #[test]
    fn reinstall_files_are_collected() {
        let tmp = tempdir().unwrap();
        let a = venv(tmp.path(), "home = /usr\n");
        for name in ["uv.lock", "requirements-dev.txt", "README.md"] {
            File::create(tmp.path().join(name)).unwrap();
        }

        let s = gather(&a);
        assert_eq!(s.reinstall_from, vec!["uv.lock", "requirements-dev.txt"]);
        assert!(!s.reinstall_from.iter().any(|n| n == "README.md"));
    }

    #[test]
    fn recent_project_activity_is_active() {
        let tmp = tempdir().unwrap();
        let a = venv(tmp.path(), "home = /usr\n");
        File::create(tmp.path().join("main.py")).unwrap();

        let s = gather(&a);
        assert!(s.project_mtime.is_some());

        let (badge, reasons) = derive_badge(&a, &s, SystemTime::now(), STALE);
        assert_eq!(badge, Badge::Active);
        assert!(reasons.iter().any(|r| r.contains("project modified")));
    }

    #[test]
    fn stale_project_with_lockfile_is_safe() {
        let tmp = tempdir().unwrap();
        let a = venv(tmp.path(), "home = /usr\n");
        File::create(tmp.path().join("uv.lock")).unwrap();

        let s = gather(&a);
        let now = SystemTime::now() + Duration::from_secs(400 * 86_400);
        let (badge, reasons) = derive_badge(&a, &s, now, STALE);
        assert_eq!(badge, Badge::Safe);
        assert!(reasons.iter().any(|r| r.contains("uv.lock")));
    }

    #[test]
    fn stale_project_without_lockfile_needs_review() {
        let tmp = tempdir().unwrap();
        let a = venv(tmp.path(), "home = /usr\n");
        File::create(tmp.path().join("scratch.py")).unwrap();

        let s = gather(&a);
        let now = SystemTime::now() + Duration::from_secs(400 * 86_400);
        let (badge, reasons) = derive_badge(&a, &s, now, STALE);
        assert_eq!(badge, Badge::Review);
        assert!(!reasons.is_empty());
    }

    #[test]
    fn cache_is_safe_without_a_project() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("cache");
        fs::create_dir_all(&path).unwrap();
        let a = Artifact::new(2, path, Category::Cache, None);

        let s = gather(&a);
        assert!(s.artifact_mtime.is_some());
        assert!(s.project_mtime.is_none());

        let (badge, reasons) = derive_badge(&a, &s, SystemTime::now(), STALE);
        assert_eq!(badge, Badge::Safe);
        assert!(reasons.iter().any(|r| r.contains("re-downloadable")));
    }

    #[test]
    fn artifact_dir_does_not_count_as_project_activity() {
        let tmp = tempdir().unwrap();
        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let a = venv(&project, "home = /usr\n");
        fs::create_dir_all(project.join("node_modules")).unwrap();
        File::create(project.join("node_modules").join("x.js")).unwrap();

        let s = gather(&a);
        assert!(s.project_mtime.is_none());
    }

    #[test]
    fn git_refs_provide_activity() {
        let tmp = tempdir().unwrap();
        let a = venv(tmp.path(), "home = /usr\n");
        let heads = tmp.path().join(".git").join("refs").join("heads");
        fs::create_dir_all(&heads).unwrap();
        File::create(heads.join("main")).unwrap();

        let s = gather(&a);
        assert!(s.git_mtime.is_some());
        assert_eq!(
            derive_badge(&a, &s, SystemTime::now(), STALE).0,
            Badge::Active
        );
    }

    #[test]
    fn git_head_is_the_fallback_for_a_fresh_repo() {
        let tmp = tempdir().unwrap();
        let a = venv(tmp.path(), "home = /usr\n");
        let git = tmp.path().join(".git");
        fs::create_dir_all(git.join("refs").join("heads")).unwrap();
        File::create(git.join("HEAD")).unwrap();

        assert!(gather(&a).git_mtime.is_some());
    }

    #[test]
    fn ages_are_humanized() {
        assert_eq!(humanize_age(Duration::from_secs(3600)), "today");
        assert_eq!(humanize_age(Duration::from_secs(3 * 86_400)), "3d");
        assert_eq!(humanize_age(Duration::from_secs(150 * 86_400)), "5mo");
        assert_eq!(humanize_age(Duration::from_secs(800 * 86_400)), "2y");
    }
}
