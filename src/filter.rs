//! Filtering and sorting of the scanned artifact list.
//!
//! Nothing here clones an [`Artifact`]: [`apply`] returns indices into the
//! caller's slice so the UI can re-filter on every keystroke cheaply.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::model::{Artifact, Category};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Reclaim,
    Size,
    Age,
    Path,
    Category,
    Badge,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Reclaim => "reclaim",
            SortKey::Size => "size",
            SortKey::Age => "age",
            SortKey::Path => "path",
            SortKey::Category => "category",
            SortKey::Badge => "badge",
        }
    }

    pub fn next(self) -> SortKey {
        match self {
            SortKey::Reclaim => SortKey::Size,
            SortKey::Size => SortKey::Age,
            SortKey::Age => SortKey::Path,
            SortKey::Path => SortKey::Category,
            SortKey::Category => SortKey::Badge,
            SortKey::Badge => SortKey::Reclaim,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FilterState {
    pub query: String,
    pub categories: HashSet<Category>,
    pub min_size: u64,
    pub min_age: Option<Duration>,
    pub sort: SortKey,
    pub descending: bool,
}

impl Default for FilterState {
    fn default() -> Self {
        FilterState {
            query: String::new(),
            categories: Category::ALL.into_iter().collect(),
            min_size: 0,
            min_age: None,
            sort: SortKey::Reclaim,
            descending: true,
        }
    }
}

/// Filter then sort. Returns indices into `artifacts`.
pub fn apply(artifacts: &[Artifact], f: &FilterState, now: SystemTime) -> Vec<usize> {
    let mut out: Vec<usize> = artifacts
        .iter()
        .enumerate()
        .filter(|(_, a)| keeps(a, f, now))
        .map(|(i, _)| i)
        .collect();

    out.sort_by(|&x, &y| {
        let (a, b) = (&artifacts[x], &artifacts[y]);
        let primary = match f.sort {
            SortKey::Reclaim => a.sizes.reclaim.cmp(&b.sizes.reclaim),
            SortKey::Size => a.sizes.apparent.cmp(&b.sizes.apparent),
            // Age needs its own path so that rows without an mtime stay last
            // in both directions instead of flipping to the top.
            SortKey::Age => return cmp_age(a, b, now, f.descending),
            SortKey::Path => a.path.cmp(&b.path),
            SortKey::Category => a.category.cmp(&b.category),
            SortKey::Badge => a.badge.cmp(&b.badge),
        };
        let primary = if f.descending {
            primary.reverse()
        } else {
            primary
        };
        primary.then_with(|| a.path.cmp(&b.path))
    });

    out
}

fn keeps(a: &Artifact, f: &FilterState, now: SystemTime) -> bool {
    if !f.categories.contains(&a.category) {
        return false;
    }
    if a.sizes.reclaim < f.min_size {
        return false;
    }
    if let Some(min_age) = f.min_age {
        // A missing mtime means signals have not streamed in yet; keep the row
        // rather than making it appear and vanish mid-scan.
        if let Some(mtime) = a.signals.artifact_mtime
            && age_of(mtime, now) < min_age
        {
            return false;
        }
    }
    subsequence_ci(&a.path.to_string_lossy(), &f.query)
}

fn age_of(mtime: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(mtime).unwrap_or(Duration::ZERO)
}

fn cmp_age(a: &Artifact, b: &Artifact, now: SystemTime, descending: bool) -> Ordering {
    match (a.signals.artifact_mtime, b.signals.artifact_mtime) {
        (None, None) => a.path.cmp(&b.path),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let ord = age_of(x, now).cmp(&age_of(y, now));
            let ord = if descending { ord.reverse() } else { ord };
            ord.then_with(|| a.path.cmp(&b.path))
        }
    }
}

fn ci_eq(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Every char of `needle` appears in `haystack`, in order, ignoring case.
fn subsequence_ci(haystack: &str, needle: &str) -> bool {
    let mut hay = haystack.chars();
    'needle: for n in needle.chars() {
        for h in hay.by_ref() {
            if ci_eq(h, n) {
                continue 'needle;
            }
        }
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Sizes;
    use std::path::PathBuf;

    fn art(id: u64, path: &str, cat: Category, reclaim: u64) -> Artifact {
        let mut a = Artifact::new(id, PathBuf::from(path), cat, None);
        a.sizes = Sizes {
            reclaim,
            apparent: reclaim,
            ..Sizes::default()
        };
        a
    }

    fn paths<'a>(artifacts: &'a [Artifact], idx: &[usize]) -> Vec<&'a str> {
        idx.iter()
            .map(|&i| artifacts[i].path.to_str().unwrap())
            .collect()
    }

    #[test]
    fn subsequence_matches_out_of_order_chars() {
        assert!(subsequence_ci("docspanext", "dsp"));
        assert!(subsequence_ci("docspanext", "DSP"));
        assert!(subsequence_ci("DocsPanExt", "dsp"));
        assert!(subsequence_ci("docspanext", ""));
        assert!(!subsequence_ci("docspanext", "psd"));
        assert!(!subsequence_ci("docspanext", "zz"));
        assert!(!subsequence_ci("doc", "docs"));
    }

    #[test]
    fn query_filters_by_subsequence_of_full_path() {
        let artifacts = vec![
            art(1, "/home/u/docspanext/.venv", Category::PyVenv, 10),
            art(2, "/home/u/other/.venv", Category::PyVenv, 20),
        ];
        let f = FilterState {
            query: "dsp".into(),
            ..FilterState::default()
        };
        let idx = apply(&artifacts, &f, SystemTime::now());
        assert_eq!(paths(&artifacts, &idx), ["/home/u/docspanext/.venv"]);
    }

    #[test]
    fn min_size_drops_small_rows() {
        let artifacts = vec![
            art(1, "/a/b/small", Category::PyVenv, 100),
            art(2, "/a/b/big", Category::PyVenv, 5_000),
        ];
        let f = FilterState {
            min_size: 1_000,
            ..FilterState::default()
        };
        let idx = apply(&artifacts, &f, SystemTime::now());
        assert_eq!(paths(&artifacts, &idx), ["/a/b/big"]);
    }

    #[test]
    fn category_filter_excludes_disabled_categories() {
        let artifacts = vec![
            art(1, "/a/b/venv", Category::PyVenv, 1),
            art(2, "/a/b/node_modules", Category::NodeModules, 1),
        ];
        let f = FilterState {
            categories: [Category::NodeModules].into_iter().collect(),
            ..FilterState::default()
        };
        let idx = apply(&artifacts, &f, SystemTime::now());
        assert_eq!(paths(&artifacts, &idx), ["/a/b/node_modules"]);
    }

    #[test]
    fn ties_break_on_path_in_both_directions() {
        let artifacts = vec![
            art(1, "/a/c/venv", Category::PyVenv, 42),
            art(2, "/a/a/venv", Category::PyVenv, 42),
            art(3, "/a/b/venv", Category::PyVenv, 42),
        ];
        let mut f = FilterState::default();
        let desc = apply(&artifacts, &f, SystemTime::now());
        assert_eq!(
            paths(&artifacts, &desc),
            ["/a/a/venv", "/a/b/venv", "/a/c/venv"]
        );
        f.descending = false;
        let asc = apply(&artifacts, &f, SystemTime::now());
        assert_eq!(paths(&artifacts, &asc), paths(&artifacts, &desc));
    }

    #[test]
    fn reclaim_sort_honors_direction() {
        let artifacts = vec![
            art(1, "/a/b/one", Category::PyVenv, 10),
            art(2, "/a/b/two", Category::PyVenv, 30),
            art(3, "/a/b/three", Category::PyVenv, 20),
        ];
        let mut f = FilterState::default();
        let idx = apply(&artifacts, &f, SystemTime::now());
        assert_eq!(
            paths(&artifacts, &idx),
            ["/a/b/two", "/a/b/three", "/a/b/one"]
        );
        f.descending = false;
        let idx = apply(&artifacts, &f, SystemTime::now());
        assert_eq!(
            paths(&artifacts, &idx),
            ["/a/b/one", "/a/b/three", "/a/b/two"]
        );
    }

    #[test]
    fn age_filter_keeps_rows_without_mtime() {
        let now = SystemTime::now();
        let mut fresh = art(1, "/a/b/fresh", Category::PyVenv, 1);
        fresh.signals.artifact_mtime = Some(now - Duration::from_secs(60));
        let mut old = art(2, "/a/b/old", Category::PyVenv, 1);
        old.signals.artifact_mtime = Some(now - Duration::from_secs(90 * 86_400));
        let pending = art(3, "/a/b/pending", Category::PyVenv, 1);

        let artifacts = vec![fresh, old, pending];
        let f = FilterState {
            min_age: Some(Duration::from_secs(30 * 86_400)),
            sort: SortKey::Path,
            descending: false,
            ..FilterState::default()
        };
        let idx = apply(&artifacts, &f, now);
        assert_eq!(paths(&artifacts, &idx), ["/a/b/old", "/a/b/pending"]);
    }

    #[test]
    fn age_sort_puts_unknown_mtime_last_in_both_directions() {
        let now = SystemTime::now();
        let mut recent = art(1, "/a/b/recent", Category::PyVenv, 1);
        recent.signals.artifact_mtime = Some(now - Duration::from_secs(60));
        let mut ancient = art(2, "/a/b/ancient", Category::PyVenv, 1);
        ancient.signals.artifact_mtime = Some(now - Duration::from_secs(400 * 86_400));
        let pending = art(3, "/a/b/pending", Category::PyVenv, 1);

        let artifacts = vec![recent, ancient, pending];
        let mut f = FilterState {
            sort: SortKey::Age,
            ..FilterState::default()
        };
        let idx = apply(&artifacts, &f, now);
        assert_eq!(
            paths(&artifacts, &idx),
            ["/a/b/ancient", "/a/b/recent", "/a/b/pending"]
        );
        f.descending = false;
        let idx = apply(&artifacts, &f, now);
        assert_eq!(
            paths(&artifacts, &idx),
            ["/a/b/recent", "/a/b/ancient", "/a/b/pending"]
        );
    }

    #[test]
    fn sort_key_cycles() {
        let mut k = SortKey::Reclaim;
        let mut seen = Vec::new();
        for _ in 0..6 {
            seen.push(k.label());
            k = k.next();
        }
        assert_eq!(k, SortKey::Reclaim);
        assert_eq!(
            seen,
            ["reclaim", "size", "age", "path", "category", "badge"]
        );
    }
}
