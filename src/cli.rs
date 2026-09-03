//! Command-line surface: argument parsing and the value parsers for the
//! human-friendly size and duration syntax.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;

use crate::model::Category;
use crate::scan::ScanConfig;

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;
const MONTH: u64 = 30 * DAY;
const YEAR: u64 = 365 * DAY;

/// Find and prune reclaimable build/environment directories, hardlink-aware.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "venvpruner",
    version,
    about = "Find and prune venvs, node_modules, target dirs and caches",
    long_about = None,
    after_help = "\
EXAMPLES:
    venvpruner                              scan $HOME in the interactive TUI
    venvpruner ~/code ~/work                scan two roots
    venvpruner --no-tui --min-size 100M     print everything at least 100 MiB
    venvpruner --dry-run --older-than 6mo   what a cleanup would remove
    venvpruner --json --category venv,node  machine-readable output"
)]
pub struct Args {
    /// Directories to scan. Defaults to $HOME.
    #[arg(value_name = "ROOT")]
    pub roots: Vec<PathBuf>,

    /// Ignore artifacts smaller than this (100M, 1.5G, 500K, or plain bytes).
    #[arg(long, value_name = "SIZE", default_value = "0", value_parser = parse_size)]
    pub min_size: u64,

    /// Only show artifacts untouched for at least this long (90d, 6mo, 2y, 12h).
    #[arg(long, value_name = "DUR", value_parser = parse_duration)]
    pub older_than: Option<Duration>,

    /// Comma-separated categories to scan. Defaults to all.
    #[arg(
        long = "category",
        value_name = "LIST",
        value_delimiter = ',',
        value_parser = parse_category
    )]
    pub category: Vec<Category>,

    /// How long without activity before an artifact counts as stale.
    #[arg(long, value_name = "DUR", default_value = "90d", value_parser = parse_duration)]
    pub stale_after: Duration,

    /// Emit JSON instead of a table. Implies --no-tui.
    #[arg(long)]
    pub json: bool,

    /// Scan, print the results, and exit without starting the TUI.
    #[arg(long)]
    pub no_tui: bool,

    /// Print what a cleanup would remove, delete nothing, and exit.
    #[arg(long)]
    pub dry_run: bool,

    /// Cross filesystem boundaries while walking (off by default).
    #[arg(long)]
    pub all_filesystems: bool,

    /// Worker threads per stage. Defaults to the machine's parallelism; 1 scans serially.
    #[arg(long, short = 'j', value_name = "N", value_parser = parse_jobs)]
    pub jobs: Option<usize>,
}

impl Args {
    /// True when no interactive UI should be started.
    pub fn headless(&self) -> bool {
        self.json || self.no_tui || self.dry_run
    }

    pub fn scan_config(&self) -> Result<ScanConfig> {
        let roots = self.resolve_roots()?;
        let categories: HashSet<Category> = if self.category.is_empty() {
            Category::ALL.into_iter().collect()
        } else {
            self.category.iter().copied().collect()
        };
        Ok(ScanConfig {
            roots,
            one_file_system: !self.all_filesystems,
            categories,
            stale_after: self.stale_after,
            jobs: self.jobs.unwrap_or_else(default_jobs),
        })
    }

    fn resolve_roots(&self) -> Result<Vec<PathBuf>> {
        let raw: Vec<PathBuf> = if self.roots.is_empty() {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                anyhow!("no roots given and $HOME is not set; pass one or more directories to scan")
            })?;
            vec![PathBuf::from(home)]
        } else {
            self.roots.clone()
        };

        let mut out: Vec<PathBuf> = Vec::with_capacity(raw.len());
        for root in raw {
            let resolved = std::fs::canonicalize(&root)
                .with_context(|| format!("cannot scan {}", root.display()))?;
            if !resolved.is_dir() {
                bail!("{} is not a directory", root.display());
            }
            if !out.contains(&resolved) {
                out.push(resolved);
            }
        }
        Ok(out)
    }
}

/// Worker threads per stage when `--jobs` is not given.
///
/// One per logical CPU, so a 32-core machine is not held to the four threads a
/// laptop gets. The upper bound keeps a very wide host from pointing hundreds
/// of `stat`-bound threads at one disk, where the queue depth stops helping
/// well before that and contention starts costing.
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 32)
}

/// Parse `--jobs`. Zero is rejected rather than silently meaning "auto": a
/// caller writing `-j0` in a script is asking for something, and picking a
/// count for them would hide the mistake.
fn parse_jobs(s: &str) -> std::result::Result<usize, String> {
    let text = s.trim();
    let value: usize = text
        .parse()
        .map_err(|_| format!("invalid job count {s:?}: expected a whole number, e.g. 8"))?;
    if value == 0 {
        return Err("job count must be at least 1; omit --jobs to size it to the machine".into());
    }
    Ok(value)
}

fn parse_category(s: &str) -> std::result::Result<Category, String> {
    let token = s.trim();
    Category::parse(token).ok_or_else(|| {
        let valid: Vec<&str> = Category::ALL.iter().map(|c| c.label()).collect();
        format!(
            "unknown category {token:?}; valid values: {}",
            valid.join(", ")
        )
    })
}

/// Parse a human size into bytes. Units are binary (K = 1024).
/// Accepts `0`, `4096`, `500K`, `100M`, `1.5G`, `2TiB`.
pub fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let text = s.trim();
    if text.is_empty() {
        return Err("empty size; expected something like 0, 500K, 100M or 1.5G".to_string());
    }
    let (number, unit) = split_number(text);
    if number.is_empty() {
        return Err(format!(
            "invalid size {s:?}: expected a number followed by an optional unit, e.g. 100M"
        ));
    }
    let value: f64 = number
        .replace('_', "")
        .parse()
        .map_err(|_| format!("invalid size {s:?}: {number:?} is not a number"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid size {s:?}: must be a non-negative number"));
    }

    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        other => {
            return Err(format!(
                "invalid size unit {other:?} in {s:?}; use B, K, M, G or T (e.g. 500K, 1.5G)"
            ));
        }
    };

    let bytes = value * multiplier as f64;
    if bytes >= u64::MAX as f64 {
        return Err(format!("size {s:?} is too large"));
    }
    Ok(bytes.round() as u64)
}

/// Parse a human duration. Accepts `30s`, `12h`, `90d`, `2w`, `6mo`, `2y`.
///
/// A bare `m` is rejected: it reads as both "minutes" and "months". Spell it
/// `min` or `mo`.
pub fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let text = s.trim();
    if text.is_empty() {
        return Err("empty duration; expected something like 90d, 6mo or 12h".to_string());
    }
    let (number, unit) = split_number(text);
    if number.is_empty() {
        return Err(format!(
            "invalid duration {s:?}: expected a number followed by a unit, e.g. 90d"
        ));
    }
    let value: f64 = number
        .replace('_', "")
        .parse()
        .map_err(|_| format!("invalid duration {s:?}: {number:?} is not a number"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!(
            "invalid duration {s:?}: must be a non-negative number"
        ));
    }

    let unit = unit.trim().to_ascii_lowercase();
    let seconds: u64 = match unit.as_str() {
        "" => {
            return Err(format!(
                "duration {s:?} has no unit; use s, min, h, d, w, mo or y (e.g. 90d)"
            ));
        }
        "m" => {
            return Err(format!(
                "ambiguous duration unit \"m\" in {s:?}; write \"min\" for minutes or \"mo\" for months (30 days)"
            ));
        }
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "min" | "mins" | "minute" | "minutes" => MINUTE,
        "h" | "hr" | "hrs" | "hour" | "hours" => HOUR,
        "d" | "day" | "days" => DAY,
        "w" | "wk" | "wks" | "week" | "weeks" => WEEK,
        "mo" | "mon" | "mos" | "month" | "months" => MONTH,
        "y" | "yr" | "yrs" | "year" | "years" => YEAR,
        other => {
            return Err(format!(
                "invalid duration unit {other:?} in {s:?}; use s, min, h, d, w, mo or y (e.g. 6mo)"
            ));
        }
    };

    let total = value * seconds as f64;
    if total >= u64::MAX as f64 {
        return Err(format!("duration {s:?} is too large"));
    }
    Ok(Duration::from_secs(total.round() as u64))
}

/// Split a leading decimal number from its trailing unit.
fn split_number(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_'))
        .unwrap_or(text.len());
    text.split_at(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_without_units_are_bytes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size(" 512 ").unwrap(), 512);
        assert_eq!(parse_size("1B").unwrap(), 1);
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(parse_size("500K").unwrap(), 500 * 1024);
        assert_eq!(parse_size("100M").unwrap(), 100 * 1024 * 1024);
        assert_eq!(parse_size("1.5G").unwrap(), 1024 * 1024 * 1024 * 3 / 2);
        assert_eq!(parse_size("2TiB").unwrap(), 2 * (1u64 << 40));
        assert_eq!(parse_size("1gb").unwrap(), 1 << 30);
        assert_eq!(parse_size("1_048_576").unwrap(), 1 << 20);
    }

    #[test]
    fn sizes_reject_garbage_with_a_useful_message() {
        let err = parse_size("banana").unwrap_err();
        assert!(err.contains("expected a number"), "{err}");

        let err = parse_size("100X").unwrap_err();
        assert!(err.contains("invalid size unit"), "{err}");
        assert!(err.contains("use B, K, M, G or T"), "{err}");

        let err = parse_size("1.2.3M").unwrap_err();
        assert!(err.contains("is not a number"), "{err}");

        assert!(parse_size("").is_err());
        assert!(parse_size("-5M").is_err());
    }

    #[test]
    fn durations_cover_the_documented_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(
            parse_duration("12h").unwrap(),
            Duration::from_secs(12 * HOUR)
        );
        assert_eq!(
            parse_duration("90d").unwrap(),
            Duration::from_secs(90 * DAY)
        );
        assert_eq!(parse_duration("2w").unwrap(), Duration::from_secs(14 * DAY));
        assert_eq!(
            parse_duration("2y").unwrap(),
            Duration::from_secs(730 * DAY)
        );
        assert_eq!(
            parse_duration("1.5d").unwrap(),
            Duration::from_secs(36 * HOUR)
        );
    }

    #[test]
    fn mo_means_thirty_days_and_is_not_minutes() {
        assert_eq!(
            parse_duration("6mo").unwrap(),
            Duration::from_secs(180 * DAY)
        );
        assert_eq!(
            parse_duration("1mo").unwrap(),
            Duration::from_secs(30 * DAY)
        );
        assert_eq!(parse_duration("6min").unwrap(), Duration::from_secs(360));
        assert_ne!(
            parse_duration("6mo").unwrap(),
            parse_duration("6min").unwrap()
        );
    }

    #[test]
    fn bare_m_is_rejected_as_ambiguous() {
        let err = parse_duration("5m").unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("min"), "{err}");
        assert!(err.contains("mo"), "{err}");
    }

    #[test]
    fn durations_reject_garbage_with_a_useful_message() {
        let err = parse_duration("soon").unwrap_err();
        assert!(err.contains("expected a number"), "{err}");

        let err = parse_duration("90").unwrap_err();
        assert!(err.contains("no unit"), "{err}");

        let err = parse_duration("90q").unwrap_err();
        assert!(err.contains("invalid duration unit"), "{err}");

        assert!(parse_duration("").is_err());
        assert!(parse_duration("-1d").is_err());
    }

    #[test]
    fn categories_round_trip_through_their_labels() {
        for c in Category::ALL {
            assert_eq!(parse_category(c.label()).unwrap(), c);
        }
        let err = parse_category("venvs").unwrap_err();
        assert!(err.contains("unknown category"), "{err}");
        assert!(err.contains("venv"), "{err}");
    }

    #[test]
    fn missing_roots_are_reported_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let args = Args {
            roots: vec![missing.clone()],
            min_size: 0,
            older_than: None,
            category: Vec::new(),
            stale_after: Duration::from_secs(90 * DAY),
            json: false,
            no_tui: true,
            dry_run: false,
            all_filesystems: false,
            jobs: None,
        };
        let err = args.scan_config().unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("cannot scan"), "{text}");
        assert!(text.contains("nope"), "{text}");
    }

    #[test]
    fn scan_config_defaults_to_every_category_and_one_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let args = Args {
            roots: vec![dir.path().to_path_buf()],
            min_size: 0,
            older_than: None,
            category: Vec::new(),
            stale_after: Duration::from_secs(90 * DAY),
            json: false,
            no_tui: true,
            dry_run: false,
            all_filesystems: false,
            jobs: None,
        };
        let cfg = args.scan_config().unwrap();
        assert_eq!(cfg.categories.len(), Category::ALL.len());
        assert!(cfg.one_file_system);
        assert_eq!(cfg.roots.len(), 1);
    }

    #[test]
    fn jobs_defaults_to_the_machine_and_is_never_degenerate() {
        let dir = tempfile::tempdir().unwrap();
        let args = Args {
            roots: vec![dir.path().to_path_buf()],
            min_size: 0,
            older_than: None,
            category: Vec::new(),
            stale_after: Duration::from_secs(90 * DAY),
            json: false,
            no_tui: true,
            dry_run: false,
            all_filesystems: false,
            jobs: None,
        };
        let cfg = args.scan_config().unwrap();
        assert_eq!(cfg.jobs, default_jobs());
        // A single-threaded scan would be the slow path nobody asked for, and a
        // pool of zero means "rayon decides".
        assert!(cfg.jobs >= 2, "default of {} is not parallel", cfg.jobs);
    }

    #[test]
    fn an_explicit_job_count_is_used_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let args = Args {
            roots: vec![dir.path().to_path_buf()],
            min_size: 0,
            older_than: None,
            category: Vec::new(),
            stale_after: Duration::from_secs(90 * DAY),
            json: false,
            no_tui: true,
            dry_run: false,
            all_filesystems: false,
            jobs: Some(1),
        };
        assert_eq!(args.scan_config().unwrap().jobs, 1);
    }

    #[test]
    fn zero_jobs_is_rejected_rather_than_silently_meaning_auto() {
        assert_eq!(parse_jobs("8").unwrap(), 8);
        assert_eq!(parse_jobs(" 2 ").unwrap(), 2);

        let err = parse_jobs("0").unwrap_err();
        assert!(err.contains("at least 1"), "{err}");

        let err = parse_jobs("-1").unwrap_err();
        assert!(err.contains("whole number"), "{err}");
        assert!(parse_jobs("many").is_err());
    }

    #[test]
    fn duplicate_roots_collapse() {
        let dir = tempfile::tempdir().unwrap();
        let args = Args {
            roots: vec![dir.path().to_path_buf(), dir.path().to_path_buf()],
            min_size: 0,
            older_than: None,
            category: vec![Category::PyVenv],
            stale_after: Duration::from_secs(DAY),
            json: false,
            no_tui: true,
            dry_run: false,
            all_filesystems: true,
            jobs: Some(1),
        };
        let cfg = args.scan_config().unwrap();
        assert_eq!(cfg.roots.len(), 1);
        assert!(!cfg.one_file_system);
        assert_eq!(cfg.categories.len(), 1);
    }
}
