//! Hardlink-aware directory sizing.
//!
//! Package managers such as `uv` hardlink files out of a shared cache into
//! every environment they create, so naive summing reports bytes that deleting
//! the environment would not actually free. Every count here is derived from
//! `(st_dev, st_ino)` identity and `st_nlink`, and uses on-disk usage
//! (`st_blocks * 512`) rather than `st_size` so compression and sparse files
//! are reported the way the filesystem sees them.

use crate::model::Sizes;
use std::collections::{HashMap, HashSet};
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// One inode's accounting within a single walk.
struct InodeAcct {
    /// How many links to this inode were found inside the artifact.
    links_seen: u64,
    /// Total link count reported by the filesystem.
    nlink: u64,
    /// On-disk usage of the inode, counted once.
    bytes: u64,
}

/// Sizes directories, deduplicating hardlinks within each walk and across the
/// whole session.
///
/// `measure` takes `&self` and synchronises internally, so one `Sizer` can be
/// shared by reference across rayon threads.
pub struct Sizer {
    /// Inodes already attributed to some artifact during this session.
    seen: Mutex<HashSet<(u64, u64)>>,
    /// Bytes of inodes counted exactly once across all `measure` calls.
    unique: AtomicU64,
}

impl Sizer {
    pub fn new() -> Self {
        Sizer {
            seen: Mutex::new(HashSet::new()),
            unique: AtomicU64::new(0),
        }
    }

    /// Walk `root` and report its hardlink-aware sizes.
    ///
    /// Symlinks are skipped entirely: they are neither descended into nor
    /// counted as bytes. Their own on-disk footprint is negligible and counting
    /// them would make totals depend on link-target lengths.
    ///
    /// IO errors are counted, never propagated: a lower bound flagged via
    /// [`Sizes::partial`] is more useful than no number at all.
    pub fn measure(&self, root: &Path) -> Sizes {
        self.measure_many(std::slice::from_ref(&root.to_path_buf()))
    }

    /// Walk every path in `roots` as **one** artifact.
    ///
    /// Summing independent [`measure`](Self::measure) calls is wrong for an
    /// artifact made of several directories (an aggregated `__pycache__` group,
    /// say): each call keeps its own inode table, so an inode hardlinked
    /// between two of the paths is counted twice in `apparent` and classified
    /// `shared` in both — reporting `reclaim == 0` for bytes that deleting the
    /// whole artifact genuinely frees. One shared table gets it right.
    pub fn measure_many(&self, roots: &[PathBuf]) -> Sizes {
        let mut inodes: HashMap<(u64, u64), InodeAcct> = HashMap::new();
        let mut dirs: u64 = 0;
        let mut errors: u64 = 0;
        let mut stack: Vec<PathBuf> = Vec::new();

        for root in roots {
            match fs::symlink_metadata(root) {
                Ok(md) if md.file_type().is_dir() => stack.push(root.to_path_buf()),
                Ok(md) if md.file_type().is_file() => record(&mut inodes, &md),
                // A symlink, fifo, socket or device where a directory was
                // expected: nothing to count, but the number we return is not
                // the whole story, so say so instead of reporting a confident 0.
                Ok(_) => errors += 1,
                Err(_) => errors += 1,
            }
        }

        // Explicit stack rather than recursion: node_modules trees nest deeply
        // enough to be a real stack-overflow risk.
        while let Some(dir) = stack.pop() {
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };
            dirs += 1;
            for entry in rd {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => {
                        errors += 1;
                        continue;
                    }
                };
                let path = entry.path();
                let md = match fs::symlink_metadata(&path) {
                    Ok(md) => md,
                    Err(_) => {
                        errors += 1;
                        continue;
                    }
                };
                let ft = md.file_type();
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() {
                    record(&mut inodes, &md);
                }
            }
        }

        let mut apparent: u64 = 0;
        let mut reclaim: u64 = 0;
        let mut shared: u64 = 0;
        for acct in inodes.values() {
            apparent = apparent.saturating_add(acct.bytes);
            // Every link accounted for inside this artifact means deleting it
            // drops the last reference and the extents come back.
            if acct.links_seen >= acct.nlink {
                reclaim = reclaim.saturating_add(acct.bytes);
            } else {
                shared = shared.saturating_add(acct.bytes);
            }
        }
        debug_assert_eq!(apparent, reclaim.saturating_add(shared));

        // Single short critical section: the walk above ran lock-free.
        let mut newly_unique: u64 = 0;
        {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            for (key, acct) in &inodes {
                if seen.insert(*key) {
                    newly_unique = newly_unique.saturating_add(acct.bytes);
                }
            }
        }
        self.unique.fetch_add(newly_unique, Ordering::Relaxed);

        Sizes {
            apparent,
            reclaim,
            shared,
            files: inodes.len() as u64,
            dirs,
            partial: errors > 0,
        }
    }

    /// Bytes counted once across every `measure` call on this `Sizer`.
    ///
    /// Summing `Sizes::apparent` over artifacts double-counts anything they
    /// hardlink-share; this is the number that survives contact with `df`.
    pub fn unique_total(&self) -> u64 {
        self.unique.load(Ordering::Relaxed)
    }
}

impl Default for Sizer {
    fn default() -> Self {
        Sizer::new()
    }
}

fn record(inodes: &mut HashMap<(u64, u64), InodeAcct>, md: &Metadata) {
    let acct = inodes
        .entry((md.dev(), md.ino()))
        .or_insert_with(|| InodeAcct {
            links_seen: 0,
            nlink: md.nlink(),
            bytes: md.blocks().saturating_mul(512),
        });
    acct.links_seen += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, bytes: usize) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&vec![b'x'; bytes]).unwrap();
        f.sync_all().unwrap();
    }

    /// On-disk usage of one path, straight from the filesystem.
    fn disk(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().blocks() * 512
    }

    #[test]
    fn plain_files_are_all_reclaimable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_file(&dir.join("a"), 8192);
        write_file(&dir.join("b"), 4096);
        let expected = disk(&dir.join("a")) + disk(&dir.join("b"));

        let sizes = Sizer::new().measure(dir);

        assert_eq!(sizes.apparent, expected);
        assert_eq!(sizes.reclaim, sizes.apparent);
        assert_eq!(sizes.shared, 0);
        assert_eq!(sizes.files, 2);
        assert_eq!(sizes.dirs, 1);
        assert!(!sizes.partial);
        assert!(sizes.apparent >= 8192 + 4096);
    }

    #[test]
    fn hardlink_outside_artifact_is_shared_not_reclaimable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir1 = tmp.path().join("dir1");
        let dir2 = tmp.path().join("dir2");
        fs::create_dir(&dir1).unwrap();
        fs::create_dir(&dir2).unwrap();

        write_file(&dir1.join("shared.so"), 16384);
        fs::hard_link(dir1.join("shared.so"), dir2.join("shared.so")).unwrap();
        let bytes = disk(&dir1.join("shared.so"));
        assert!(bytes > 0, "filesystem must report st_blocks for this test");

        // A link lives outside dir1, so deleting dir1 frees nothing.
        let s1 = Sizer::new().measure(&dir1);
        assert_eq!(s1.apparent, bytes);
        assert_eq!(s1.shared, bytes);
        assert_eq!(s1.reclaim, 0);
        assert_eq!(s1.files, 1);

        // Symmetrically for dir2.
        let s2 = Sizer::new().measure(&dir2);
        assert_eq!(s2.shared, bytes);
        assert_eq!(s2.reclaim, 0);

        // Both links are inside the common parent, so it owns every byte.
        let both = Sizer::new().measure(tmp.path());
        assert_eq!(both.apparent, bytes);
        assert_eq!(both.reclaim, bytes);
        assert_eq!(both.shared, 0);
        assert_eq!(both.files, 1);
    }

    #[test]
    fn hardlink_within_one_walk_is_counted_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_file(&dir.join("a"), 16384);
        fs::hard_link(dir.join("a"), dir.join("a-link")).unwrap();
        let bytes = disk(&dir.join("a"));

        let sizes = Sizer::new().measure(dir);

        assert_eq!(sizes.apparent, bytes, "inode must not be double-counted");
        assert_eq!(sizes.reclaim, bytes);
        assert_eq!(sizes.shared, 0);
        assert_eq!(sizes.files, 1);
    }

    #[test]
    fn session_dedup_counts_a_shared_inode_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir1 = tmp.path().join("dir1");
        let dir2 = tmp.path().join("dir2");
        fs::create_dir(&dir1).unwrap();
        fs::create_dir(&dir2).unwrap();

        write_file(&dir1.join("shared.so"), 16384);
        fs::hard_link(dir1.join("shared.so"), dir2.join("shared.so")).unwrap();
        write_file(&dir1.join("own1"), 4096);
        write_file(&dir2.join("own2"), 4096);

        let shared_bytes = disk(&dir1.join("shared.so"));
        assert!(shared_bytes > 0, "filesystem must report st_blocks");

        let sizer = Sizer::new();
        let s1 = sizer.measure(&dir1);
        let s2 = sizer.measure(&dir2);

        let naive = s1.apparent + s2.apparent;
        assert_eq!(sizer.unique_total(), naive - shared_bytes);
        assert!(
            sizer.unique_total() < naive,
            "session total must not double-count the shared inode"
        );

        // Re-measuring adds nothing new.
        let before = sizer.unique_total();
        sizer.measure(&dir1);
        assert_eq!(sizer.unique_total(), before);
    }

    #[test]
    fn symlinks_are_skipped_and_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        write_file(&outside.join("big"), 32768);

        let dir = tmp.path().join("artifact");
        fs::create_dir(&dir).unwrap();
        write_file(&dir.join("real"), 4096);
        std::os::unix::fs::symlink(&outside, dir.join("link-to-dir")).unwrap();
        std::os::unix::fs::symlink(outside.join("big"), dir.join("link-to-file")).unwrap();

        let sizes = Sizer::new().measure(&dir);

        assert_eq!(sizes.files, 1);
        assert_eq!(sizes.dirs, 1);
        assert_eq!(sizes.apparent, disk(&dir.join("real")));
        assert!(!sizes.partial);
    }

    #[test]
    fn nested_directories_are_walked_without_recursion() {
        let tmp = tempfile::tempdir().unwrap();
        let mut deep = tmp.path().to_path_buf();
        for i in 0..64 {
            deep = deep.join(format!("lvl{i}"));
            fs::create_dir(&deep).unwrap();
        }
        write_file(&deep.join("leaf"), 4096);

        let sizes = Sizer::new().measure(tmp.path());

        assert_eq!(sizes.files, 1);
        assert_eq!(sizes.dirs, 65);
        assert_eq!(sizes.apparent, disk(&deep.join("leaf")));
        assert_eq!(sizes.reclaim, sizes.apparent);
    }

    #[test]
    fn missing_root_is_flagged_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let sizes = Sizer::new().measure(&tmp.path().join("nope"));
        assert!(sizes.partial);
        assert_eq!(sizes.apparent, 0);
        assert_eq!(sizes.files, 0);
    }

    #[test]
    fn measure_many_shares_one_inode_table_across_paths() {
        // One inode hardlinked between two directories that make up a *single*
        // artifact. Summing two independent measures would report it twice and
        // call it shared in both; measuring them together must not.
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("pkg1/__pycache__");
        let d2 = tmp.path().join("pkg2/__pycache__");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();
        write_file(&d1.join("mod.pyc"), 65536);
        fs::hard_link(d1.join("mod.pyc"), d2.join("mod.pyc")).unwrap();
        let bytes = disk(&d1.join("mod.pyc"));
        assert!(bytes > 0);

        let sizes = Sizer::new().measure_many(&[d1.clone(), d2.clone()]);
        assert_eq!(sizes.apparent, bytes, "inode counted once across paths");
        assert_eq!(sizes.reclaim, bytes, "both links are inside the artifact");
        assert_eq!(sizes.shared, 0);
        assert_eq!(sizes.files, 1);
        assert_eq!(sizes.dirs, 2);

        // The naive fold this replaced gets both numbers wrong.
        let sizer = Sizer::new();
        let (a, b) = (sizer.measure(&d1), sizer.measure(&d2));
        assert_eq!(a.apparent + b.apparent, 2 * bytes);
        assert_eq!(a.reclaim + b.reclaim, 0);
    }

    #[test]
    fn measure_many_of_disjoint_paths_sums_them() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("a");
        let d2 = tmp.path().join("b");
        fs::create_dir(&d1).unwrap();
        fs::create_dir(&d2).unwrap();
        write_file(&d1.join("f"), 8192);
        write_file(&d2.join("f"), 4096);
        let expected = disk(&d1.join("f")) + disk(&d2.join("f"));

        let sizes = Sizer::new().measure_many(&[d1, d2]);
        assert_eq!(sizes.apparent, expected);
        assert_eq!(sizes.reclaim, expected);
        assert_eq!(sizes.files, 2);
        assert!(!sizes.partial);
    }

    #[test]
    fn a_root_that_is_not_a_directory_is_flagged_partial() {
        // A directory swapped for a symlink between discovery and sizing must
        // render as a lower bound, never as a confident "0 B".
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        write_file(&real.join("big"), 1 << 20);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let sizes = Sizer::new().measure(&link);
        assert_eq!(sizes.apparent, 0, "symlinks are never followed");
        assert!(
            sizes.partial,
            "a zero that is not the whole truth must say so"
        );
    }

    #[test]
    fn measure_is_shareable_across_threads() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dirs = Vec::new();
        for i in 0..8 {
            let d = tmp.path().join(format!("d{i}"));
            fs::create_dir(&d).unwrap();
            write_file(&d.join("f"), 4096);
            dirs.push(d);
        }
        let common = tmp.path().join("common.bin");
        write_file(&common, 8192);
        for d in &dirs {
            fs::hard_link(&common, d.join("common.bin")).unwrap();
        }
        let common_bytes = disk(&common);

        let sizer = Sizer::new();
        let sizer = &sizer;
        let total: u64 = std::thread::scope(|s| {
            let handles: Vec<_> = dirs
                .iter()
                .map(|d| s.spawn(move || sizer.measure(d).apparent))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });

        // The common inode appears in all 8 walks but is unique-counted once.
        assert_eq!(sizer.unique_total(), total - 7 * common_bytes);
    }
}
