//! Walk orchestration.
//!
//! [`spawn`] returns immediately with a [`Receiver`]; discovery, hardlink-aware
//! sizing and signal analysis all happen on background threads so the UI can
//! paint partial results while a multi-terabyte filesystem is still being read.

pub mod detect;
pub mod signals;
pub mod size;

use crate::model::{Artifact, ArtifactId, Category, ScanEvent, ScanStats};
use crossbeam_channel::{Receiver, Sender, unbounded};
use ignore::{WalkBuilder, WalkState};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Lower bound on the gap between two [`ScanEvent::Progress`] sends (~20/s).
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
pub struct ScanConfig {
    pub roots: Vec<PathBuf>,
    pub one_file_system: bool,
    pub categories: HashSet<Category>,
    pub stale_after: Duration,
    /// Threads for each of the two stages, the walk and the sizing pool; `1`
    /// scans serially. They get a pool each rather than sharing one: they run
    /// at the same time and block on different things, `readdir` against
    /// `stat`, so a burst of sizing jobs would otherwise starve discovery.
    pub jobs: usize,
}

/// Start a scan. Returns at once; every event arrives on the returned channel,
/// terminated by exactly one [`ScanEvent::Done`].
pub fn spawn(cfg: ScanConfig) -> Receiver<ScanEvent> {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("venvpruner-scan".into())
        .spawn(move || run_scan(cfg, tx))
        .expect("failed to spawn scan thread");
    rx
}

/// Fold every `__pycache__` group whose project sits inside another group's
/// project into that ancestor.
///
/// Without this, nested projects yield overlapping artifacts: the ancestor's
/// group already covers the descendant's directories, so their bytes would be
/// counted twice and deleting the ancestor would silently gut the descendant
/// artifact.
fn coalesce_nested(mut grouped: HashMap<PathBuf, Vec<PathBuf>>) -> HashMap<PathBuf, Vec<PathBuf>> {
    let mut projects: Vec<PathBuf> = grouped.keys().cloned().collect();
    // Lexicographic order on components puts every ancestor before its descendants.
    projects.sort();

    let mut kept: Vec<PathBuf> = Vec::new();
    let mut out: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for project in projects {
        let children = grouped.remove(&project).unwrap_or_default();
        match kept.iter().find(|root| project.starts_with(root)) {
            Some(root) => out.entry(root.clone()).or_default().extend(children),
            None => {
                out.insert(project.clone(), children);
                kept.push(project);
            }
        }
    }
    out
}

/// Artifacts the walk cannot emit on sight, because one row is assembled from
/// directories it meets separately and far apart: every `__pycache__` beneath a
/// project, and every artifact inside one cache directory. Both are collected
/// as the walk runs and emitted once it has finished.
#[derive(Default)]
struct Deferred {
    pycache: Mutex<HashMap<PathBuf, Vec<PathBuf>>>,
    caches: Mutex<HashSet<PathBuf>>,
}

impl Deferred {
    fn add_pycache(&self, project: PathBuf, dir: &Path) {
        if let Ok(mut map) = self.pycache.lock() {
            map.entry(project).or_default().push(dir.to_path_buf());
        }
    }

    fn add_cache(&self, dir: PathBuf) {
        if let Ok(mut set) = self.caches.lock() {
            set.insert(dir);
        }
    }

    /// The cache directories, in path order.
    fn caches(&self) -> Vec<PathBuf> {
        let mut caches: Vec<PathBuf> = self
            .caches
            .lock()
            .map(|mut set| std::mem::take(&mut *set).into_iter().collect())
            .unwrap_or_default();
        caches.sort();
        caches
    }

    /// One `(project, __pycache__ dirs)` group per project, nested projects
    /// folded into their ancestor and each list sorted and deduplicated.
    fn pycache_groups(&self) -> Vec<(PathBuf, Vec<PathBuf>)> {
        let grouped = self
            .pycache
            .lock()
            .map(|mut map| std::mem::take(&mut *map))
            .unwrap_or_default();
        coalesce_nested(grouped)
            .into_iter()
            .map(|(project, mut children)| {
                children.sort();
                children.dedup();
                (project, children)
            })
            .filter(|(_, children)| !children.is_empty())
            .collect()
    }
}

struct Ctx {
    tx: Sender<ScanEvent>,
    sizer: size::Sizer,
    stale_after: Duration,
    next_id: AtomicU64,
    dirs_visited: AtomicU64,
    artifacts_found: AtomicU64,
    total_apparent: AtomicU64,
    errors: AtomicU64,
    /// Set once the receiver is gone; every worker checks it and unwinds.
    stopped: AtomicBool,
    last_progress: Mutex<Instant>,
    seen: Mutex<HashSet<PathBuf>>,
}

impl Ctx {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Returns false once the consumer has hung up; callers should bail out.
    fn send(&self, event: ScanEvent) -> bool {
        if self.stopped() {
            return false;
        }
        if self.tx.send(event).is_err() {
            self.stopped.store(true, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn error(&self, msg: String) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        self.send(ScanEvent::Error(msg));
    }

    fn next_id(&self) -> ArtifactId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// First caller to claim a path wins; overlapping roots would otherwise
    /// yield the same artifact twice.
    fn claim(&self, path: &Path) -> bool {
        match self.seen.lock() {
            Ok(mut seen) => seen.insert(path.to_path_buf()),
            Err(_) => true,
        }
    }

    fn bump_dirs(&self) {
        let visited = self.dirs_visited.fetch_add(1, Ordering::Relaxed) + 1;
        // try_lock: a thread that loses the race just skips this tick.
        let Ok(mut last) = self.last_progress.try_lock() else {
            return;
        };
        if last.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        *last = Instant::now();
        drop(last);
        self.send(ScanEvent::Progress {
            dirs_visited: visited,
        });
    }

    fn stats(&self) -> ScanStats {
        ScanStats {
            dirs_visited: self.dirs_visited.load(Ordering::Relaxed),
            artifacts_found: self.artifacts_found.load(Ordering::Relaxed),
            total_apparent: self.total_apparent.load(Ordering::Relaxed),
            total_unique: self.sizer.unique_total(),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

/// The directories an artifact's size is the sum of — and, for a `Pycache`
/// group, exactly the directories `delete` removes.
pub fn measured_paths(artifact: &Artifact) -> Vec<PathBuf> {
    if artifact.children.is_empty() {
        vec![artifact.path.clone()]
    } else {
        artifact.children.clone()
    }
}

/// Runs on the sizing pool: measure, then derive signals and a badge.
fn size_and_analyze(ctx: &Ctx, mut artifact: Artifact) {
    if ctx.stopped() {
        return;
    }
    // One `measure_many`, not a fold over per-path `measure` calls: an inode
    // hardlinked between two paths of the same artifact must be counted once
    // and credited to `reclaim`, which only a shared inode table can do.
    let sizes = ctx.sizer.measure_many(&measured_paths(&artifact));
    artifact.sizes = sizes;
    artifact.sized = true;
    ctx.total_apparent
        .fetch_add(sizes.apparent, Ordering::Relaxed);
    if !ctx.send(ScanEvent::Sized(artifact.id, sizes)) {
        return;
    }

    let signals = signals::gather(&artifact);
    let (badge, reasons) =
        signals::derive_badge(&artifact, &signals, SystemTime::now(), ctx.stale_after);
    ctx.send(ScanEvent::Analyzed(
        artifact.id,
        Box::new(signals),
        badge,
        reasons,
    ));
}

/// Announce an artifact, then hand the IO-heavy part to the sizing pool.
/// What gets measured comes from the artifact itself ([`measured_paths`]), so
/// the sized set and the deleted set cannot drift apart.
fn emit<'scope>(ctx: &Arc<Ctx>, scope: &rayon::Scope<'scope>, artifact: Artifact) -> bool {
    ctx.artifacts_found.fetch_add(1, Ordering::Relaxed);
    if !ctx.send(ScanEvent::Found(Box::new(artifact.clone()))) {
        return false;
    }
    let ctx = Arc::clone(ctx);
    scope.spawn(move |_| size_and_analyze(&ctx, artifact));
    true
}

/// The curated package-manager caches that lie inside a scan root.
///
/// Only those inside a root count. Emitting all of them unconditionally meant
/// `venvpruner ~/some/project` reported every $HOME cache as if it had been
/// found there -- tens of gigabytes the user never asked about, and totals that
/// did not describe the directory they named.
fn known_caches(cfg: &ScanConfig) -> Vec<(PathBuf, Category)> {
    detect::cache_roots()
        .into_iter()
        .filter(|(path, _)| cfg.roots.iter().any(|root| path.starts_with(root)))
        .collect()
}

/// Classify one directory the walk reached. Returns the state the walk should
/// continue in; anything worth reporting is emitted or deferred on the way.
///
/// Every match prunes, even one `--category` filters out: the walk must never
/// descend into an artifact. This is the main speed lever -- it is what avoids
/// reading the interior of an 11 GB venv.
fn visit<'scope>(
    ctx: &Arc<Ctx>,
    scope: &rayon::Scope<'scope>,
    cfg: &ScanConfig,
    deferred: &Deferred,
    cache_homes: &[PathBuf],
    path: &Path,
) -> WalkState {
    let Some(found) = detect::detect(path) else {
        return WalkState::Continue;
    };

    // Inside a cache the artifact is an implementation detail of that cache,
    // and the cache directory is what is worth reporting. This applies whatever
    // `--category` says, exactly as the curated caches do: a `node_modules` that
    // is really part of a cache is not a node artifact.
    //
    // A cache directory above every scanned root is one the user did not ask
    // about; naming a directory must never make the tool report a path above it.
    let cache = cache_homes
        .iter()
        .find_map(|home| detect::cache_bucket(home, path));
    let was_scanned = |c: &PathBuf| cfg.roots.iter().any(|root| c.starts_with(root));
    if let Some(cache) = cache.filter(was_scanned) {
        deferred.add_cache(cache);
        return WalkState::Skip;
    }

    if !cfg.categories.contains(&found.category) || !ctx.claim(path) {
        return WalkState::Skip;
    }

    if found.category == Category::Pycache {
        // Grouped per project instead of emitted as thousands of near-empty
        // directories. `claim` above is on the `__pycache__` dir, so the project
        // stays claimable by the group itself.
        let project = found
            .project
            .unwrap_or_else(|| path.parent().unwrap_or(path).to_path_buf());
        deferred.add_pycache(project, path);
        return WalkState::Skip;
    }

    let artifact = Artifact::new(
        ctx.next_id(),
        path.to_path_buf(),
        found.category,
        found.project,
    );
    if !emit(ctx, scope, artifact) {
        return WalkState::Quit;
    }
    WalkState::Skip
}

/// Walk every root, emitting what it finds and filling `deferred` with what it
/// cannot report until the walk is over.
fn walk<'scope>(
    ctx: &Arc<Ctx>,
    scope: &rayon::Scope<'scope>,
    cfg: &ScanConfig,
    deferred: &Arc<Deferred>,
    skip: &Arc<HashSet<PathBuf>>,
) {
    if cfg.roots.is_empty() || ctx.stopped() {
        return;
    }
    let cache_homes = Arc::new(detect::cache_homes());

    let mut builder = WalkBuilder::new(&cfg.roots[0]);
    for root in &cfg.roots[1..] {
        builder.add(root);
    }
    // Every ignore rule is off on purpose: venvs, node_modules and target/ are
    // nearly always gitignored, so a default walker finds nothing at all.
    builder
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .follow_links(false)
        .same_file_system(cfg.one_file_system)
        .max_depth(None)
        .threads(cfg.jobs.max(1));

    builder.build_parallel().run(|| {
        let ctx = Arc::clone(ctx);
        let skip = Arc::clone(skip);
        let deferred = Arc::clone(deferred);
        let cache_homes = Arc::clone(&cache_homes);
        Box::new(move |result| {
            if ctx.stopped() {
                return WalkState::Quit;
            }
            let entry = match result {
                Ok(entry) => entry,
                Err(err) => {
                    ctx.error(err.to_string());
                    return WalkState::Continue;
                }
            };
            if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            ctx.bump_dirs();

            if skip.contains(path) || detect::is_skippable(path) {
                return WalkState::Skip;
            }
            visit(&ctx, scope, cfg, &deferred, &cache_homes, path)
        })
    });
}

/// Emit the artifacts the walk could only assemble once it was over.
fn emit_deferred<'scope>(
    ctx: &Arc<Ctx>,
    scope: &rayon::Scope<'scope>,
    cfg: &ScanConfig,
    deferred: &Deferred,
) {
    // Caches are pruned from the walk whatever `--category` says, but only
    // reported when the user asked for them.
    //
    // Sizing covers the whole cache directory, not just the artifacts found
    // inside it, because the whole directory is what deletion removes.
    if cfg.categories.contains(&Category::Cache) {
        for cache in deferred.caches() {
            if ctx.stopped() {
                return;
            }
            if !ctx.claim(&cache) {
                continue;
            }
            let artifact = Artifact::new(ctx.next_id(), cache, Category::Cache, None);
            if !emit(ctx, scope, artifact) {
                return;
            }
        }
    }

    // A Pycache artifact's path is the *project* dir and its sizes are the sum
    // over the `__pycache__` dirs in `children`; deleting it removes exactly
    // those, never the project itself. The list comes from the walk that already
    // honoured --one-file-system, so deletion never has to re-discover a set
    // that could differ from the sized one.
    for (project, children) in deferred.pycache_groups() {
        if ctx.stopped() {
            return;
        }
        if !ctx.claim(&project) {
            continue;
        }
        let mut artifact = Artifact::new(
            ctx.next_id(),
            project.clone(),
            Category::Pycache,
            Some(project),
        );
        artifact.children = children;
        if !emit(ctx, scope, artifact) {
            return;
        }
    }
}

fn run_scan(cfg: ScanConfig, tx: Sender<ScanEvent>) {
    let ctx = Arc::new(Ctx {
        tx,
        sizer: size::Sizer::new(),
        stale_after: cfg.stale_after,
        next_id: AtomicU64::new(0),
        dirs_visited: AtomicU64::new(0),
        artifacts_found: AtomicU64::new(0),
        total_apparent: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        stopped: AtomicBool::new(false),
        last_progress: Mutex::new(Instant::now()),
        seen: Mutex::new(HashSet::new()),
    });

    // Sizing is `stat`-bound, not CPU-bound, and deliberately gets its own pool
    // rather than rayon's global one. A zero-width pool would mean "as many
    // threads as rayon likes", the opposite of what `--jobs 0` asks for; the CLI
    // rejects 0, so this only guards a config built by hand.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.jobs.max(1))
        .thread_name(|i| format!("venvpruner-size-{i}"))
        .build();
    let pool = match pool {
        Ok(pool) => pool,
        Err(err) => {
            ctx.error(format!("could not start sizing pool: {err}"));
            ctx.send(ScanEvent::Done(ctx.stats()));
            return;
        }
    };

    // Package-manager caches live under $HOME and would be traversed by the
    // walk; they are emitted directly and pruned from it instead.
    let caches = known_caches(&cfg);
    let skip: Arc<HashSet<PathBuf>> = Arc::new(caches.iter().map(|(p, _)| p.clone()).collect());
    let deferred = Arc::new(Deferred::default());

    // `in_place_scope` keeps this thread as the scope owner (it blocks on the
    // walk) while spawned sizing jobs run on the pool. The scope does not
    // return until every job has finished, which is what makes the single
    // trailing Done event correct.
    pool.in_place_scope(|scope| {
        for (path, category) in caches {
            if ctx.stopped() {
                break;
            }
            if !cfg.categories.contains(&category) || !ctx.claim(&path) {
                continue;
            }
            let artifact = Artifact::new(ctx.next_id(), path, category, None);
            if !emit(&ctx, scope, artifact) {
                break;
            }
        }

        walk(&ctx, scope, &cfg, &deferred, &skip);
        emit_deferred(&ctx, scope, &cfg, &deferred);
    });

    // The scope has joined every sizing job, so these totals are final.
    let stats = ctx.stats();
    ctx.send(ScanEvent::Progress {
        dirs_visited: stats.dirs_visited,
    });
    ctx.send(ScanEvent::Done(stats));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact_at(path: &str) -> Artifact {
        Artifact::new(0, PathBuf::from(path), Category::PyVenv, None)
    }

    /// Regression for nested `node_modules`: a global npm root (no sibling
    /// `package.json`) used to be skipped by the detector, so the walk descended
    /// and emitted one artifact per installed package that had its own manifest.
    /// A real scan of $HOME reported 108 nested entries for 246 total.
    #[test]
    fn a_global_npm_root_is_emitted_once_not_once_per_nested_package() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let outer = root.join("lib").join("node_modules");
        for pkg in ["alpha", "beta"] {
            let nested = outer.join(pkg).join("node_modules").join("dep");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(outer.join(pkg).join("package.json"), "{}").unwrap();
            std::fs::write(nested.join("index.js"), "x").unwrap();
        }

        let cfg = ScanConfig {
            roots: vec![root.clone()],
            one_file_system: false,
            categories: [Category::NodeModules].into_iter().collect(),
            stale_after: std::time::Duration::from_secs(90 * 86_400),
            jobs: 4,
        };

        let mut found: Vec<PathBuf> = Vec::new();
        for ev in spawn(cfg) {
            if let ScanEvent::Found(a) = ev {
                found.push(a.path.clone());
            }
        }

        assert_eq!(
            found,
            vec![outer.clone()],
            "only the outermost node_modules should be reported; got {found:?}"
        );
    }

    /// Regression for pnpm's global install dir: the virtual store sits beside
    /// `node_modules` instead of inside it, so the walk descended into it and
    /// emitted one artifact per installed package. A real scan reported 417
    /// rows for one 153 MB store.
    #[test]
    fn a_pnpm_global_install_is_two_artifacts_not_one_per_installed_package() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let global = root.join(".local/share/pnpm/global/5");
        let store = global.join(".pnpm");
        for pkg in [
            "typescript@5.9.3",
            "lodash@4.17.23",
            "ungit@1.5.30",
            "zod@4.4.3",
        ] {
            let inner = store.join(pkg).join("node_modules").join("pkg");
            std::fs::create_dir_all(&inner).unwrap();
            std::fs::write(inner.join("index.js"), "x").unwrap();
            std::fs::write(
                store.join(pkg).join("node_modules").join("package.json"),
                "{}",
            )
            .unwrap();
        }
        std::fs::create_dir_all(global.join("node_modules")).unwrap();
        std::fs::write(global.join("package.json"), "{}").unwrap();

        let cfg = ScanConfig {
            roots: vec![root.clone()],
            one_file_system: false,
            categories: [Category::NodeModules].into_iter().collect(),
            stale_after: std::time::Duration::from_secs(90 * 86_400),
            jobs: 4,
        };

        let mut found: Vec<PathBuf> = spawn(cfg)
            .into_iter()
            .filter_map(|ev| match ev {
                ScanEvent::Found(a) => Some(a.path.clone()),
                _ => None,
            })
            .collect();
        found.sort();

        assert_eq!(
            found,
            vec![store, global.join("node_modules")],
            "the store and the link farm, never their contents"
        );
    }

    /// Regression: scanning a specific directory used to also report every
    /// well-known $HOME cache, so `venvpruner ~/some/project` claimed tens of
    /// gigabytes that were nowhere near the path the user named.
    #[test]
    fn scanning_a_directory_does_not_report_caches_outside_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let venv = root.join("proj").join(".venv");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("pyvenv.cfg"), "version_info = 3.12.0\n").unwrap();

        let cfg = ScanConfig {
            roots: vec![root.clone()],
            one_file_system: false,
            categories: Category::ALL.into_iter().collect(),
            stale_after: std::time::Duration::from_secs(90 * 86_400),
            jobs: 4,
        };

        let found: Vec<PathBuf> = spawn(cfg)
            .into_iter()
            .filter_map(|ev| match ev {
                ScanEvent::Found(a) => Some(a.path.clone()),
                _ => None,
            })
            .collect();

        assert!(
            found.iter().all(|p| p.starts_with(&root)),
            "every artifact must lie inside the scanned root; got {found:?}"
        );
        assert_eq!(found, vec![venv]);
    }

    /// `--jobs` changes how many threads do the work, never what the work finds.
    /// A serial scan is the debugging path; it has to agree with the parallel
    /// one artifact for artifact.
    #[test]
    fn the_job_count_does_not_change_what_a_scan_finds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        for n in 0..8 {
            let proj = root.join(format!("proj{n}"));
            std::fs::create_dir_all(proj.join(".venv")).unwrap();
            std::fs::write(
                proj.join(".venv").join("pyvenv.cfg"),
                "version_info = 3.12\n",
            )
            .unwrap();
            std::fs::create_dir_all(proj.join("node_modules").join("dep")).unwrap();
            std::fs::write(proj.join("package.json"), "{}").unwrap();
            std::fs::create_dir_all(proj.join("pkg").join("__pycache__")).unwrap();
        }

        let scan_with = |jobs: usize| -> Vec<PathBuf> {
            let cfg = ScanConfig {
                roots: vec![root.clone()],
                one_file_system: false,
                categories: Category::ALL.into_iter().collect(),
                stale_after: std::time::Duration::from_secs(90 * 86_400),
                jobs,
            };
            let mut found: Vec<PathBuf> = spawn(cfg)
                .into_iter()
                .filter_map(|ev| match ev {
                    ScanEvent::Found(a) => Some(a.path.clone()),
                    _ => None,
                })
                .collect();
            found.sort();
            found
        };

        let serial = scan_with(1);
        assert_eq!(
            serial.len(),
            8 * 3,
            "8 projects x venv + node_modules + pycache"
        );
        assert_eq!(serial, scan_with(8));
    }

    #[test]
    fn measured_paths_is_the_artifact_itself_when_there_are_no_children() {
        let a = artifact_at("/w/proj/.venv");
        assert_eq!(measured_paths(&a), vec![PathBuf::from("/w/proj/.venv")]);
    }

    #[test]
    fn measured_paths_is_the_children_when_there_are_any() {
        // For a Pycache group this list is both what gets sized and what gets
        // deleted; the project directory itself must never appear in it.
        let mut a = artifact_at("/w/proj");
        a.category = Category::Pycache;
        a.children = vec![
            PathBuf::from("/w/proj/__pycache__"),
            PathBuf::from("/w/proj/pkg/__pycache__"),
        ];
        assert_eq!(measured_paths(&a), a.children);
        assert!(!measured_paths(&a).contains(&a.path));
    }

    /// The whole point of `children`: a `__pycache__` dir nested deeper than any
    /// fixed recursion limit is sized *and* deleted, or neither.
    #[test]
    fn a_very_deep_pycache_dir_stays_in_the_measured_set() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let mut deep = project.clone();
        for i in 0..70 {
            deep = deep.join(format!("l{i}"));
        }
        let deep_cache = deep.join("__pycache__");
        std::fs::create_dir_all(&deep_cache).unwrap();
        let shallow_cache = project.join("__pycache__");
        std::fs::create_dir_all(&shallow_cache).unwrap();

        let mut grouped: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        grouped.insert(project.clone(), vec![shallow_cache.clone()]);
        grouped.insert(deep.clone(), vec![deep_cache.clone()]);

        let out = coalesce_nested(grouped);
        assert_eq!(out.len(), 1, "the deep group folds into the project");
        let mut children = out[&project].clone();
        children.sort();

        let mut a = Artifact::new(0, project, Category::Pycache, None);
        a.children = children;
        let measured = measured_paths(&a);
        assert!(measured.contains(&shallow_cache));
        assert!(
            measured.contains(&deep_cache),
            "depth must not silently drop a directory whose bytes were counted"
        );
    }

    #[test]
    fn coalesce_nested_folds_descendants_into_their_ancestor() {
        let mut grouped: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        grouped.insert(
            PathBuf::from("/w/proj/pkg"),
            vec![PathBuf::from("/w/proj/pkg/__pycache__")],
        );
        grouped.insert(
            PathBuf::from("/w/proj"),
            vec![PathBuf::from("/w/proj/__pycache__")],
        );
        grouped.insert(
            PathBuf::from("/w/other"),
            vec![PathBuf::from("/w/other/__pycache__")],
        );

        let out = coalesce_nested(grouped);
        assert_eq!(out.len(), 2);
        assert_eq!(out[Path::new("/w/proj")].len(), 2);
        assert_eq!(out[Path::new("/w/other")].len(), 1);
    }

    #[test]
    fn coalesce_nested_keeps_sibling_prefixes_apart() {
        // "/w/proj2" starts with the *string* "/w/proj" but is not inside it.
        let mut grouped: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        grouped.insert(
            PathBuf::from("/w/proj"),
            vec![PathBuf::from("/w/proj/__pycache__")],
        );
        grouped.insert(
            PathBuf::from("/w/proj2"),
            vec![PathBuf::from("/w/proj2/__pycache__")],
        );

        let out = coalesce_nested(grouped);
        assert_eq!(out.len(), 2);
    }
}
