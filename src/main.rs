mod cli;
mod delete;
mod filter;
mod model;
mod scan;
mod ui;

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use clap::Parser;
use humansize::{DECIMAL, format_size};

use crate::cli::Args;
use crate::filter::{FilterState, SortKey};
use crate::model::{Artifact, ArtifactId, Badge, Category, ScanEvent, ScanStats};

fn main() {
    if let Err(err) = run() {
        eprintln!("venvpruner: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let cfg = args.scan_config()?;

    if args.headless() {
        headless(&args, cfg)
    } else {
        ui::run(cfg)
    }
}

fn headless(args: &Args, cfg: scan::ScanConfig) -> Result<()> {
    let (artifacts, stats) = collect(cfg);
    let now = SystemTime::now();

    let categories: HashSet<Category> = if args.category.is_empty() {
        Category::ALL.into_iter().collect()
    } else {
        args.category.iter().copied().collect()
    };
    let filter = FilterState {
        query: String::new(),
        categories,
        min_size: args.min_size,
        min_age: args.older_than,
        sort: SortKey::Reclaim,
        descending: true,
    };

    let mut selected = filter::apply(&artifacts, &filter, now);
    if args.dry_run {
        selected.retain(|&i| matches!(artifacts[i].badge, Badge::Dead | Badge::Safe));
    }

    if args.json {
        print_json(args, &artifacts, &selected, &stats, now)
    } else if args.dry_run {
        print_dry_run(&artifacts, &selected, &stats, now)
    } else {
        print_table(&artifacts, &selected, &stats, now)
    }
}

/// Drain the scanner to completion, folding events into a flat artifact list.
fn collect(cfg: scan::ScanConfig) -> (Vec<Artifact>, ScanStats) {
    let rx = scan::spawn(cfg);
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut index: HashMap<ArtifactId, usize> = HashMap::new();
    let mut stats = ScanStats::default();
    let mut progress = Progress::new();
    let mut dirs = 0u64;

    for event in rx {
        match event {
            ScanEvent::Found(artifact) => {
                index.insert(artifact.id, artifacts.len());
                artifacts.push(*artifact);
                progress.tick(dirs, artifacts.len());
            }
            ScanEvent::Sized(id, sizes) => {
                if let Some(&i) = index.get(&id) {
                    artifacts[i].sizes = sizes;
                    artifacts[i].sized = true;
                }
            }
            ScanEvent::Analyzed(id, signals, badge, reasons) => {
                if let Some(&i) = index.get(&id) {
                    artifacts[i].signals = *signals;
                    artifacts[i].badge = badge;
                    artifacts[i].badge_reasons = reasons;
                    artifacts[i].analyzed = true;
                }
            }
            ScanEvent::Progress { dirs_visited } => {
                dirs = dirs_visited;
                progress.tick(dirs, artifacts.len());
            }
            ScanEvent::Error(message) => {
                progress.clear();
                eprintln!("warning: {message}");
            }
            ScanEvent::Done(done) => stats = done,
        }
    }
    progress.clear();

    if stats.artifacts_found == 0 {
        stats.artifacts_found = artifacts.len() as u64;
    }
    (artifacts, stats)
}

/// A single rewritten status line on stderr, so stdout stays pipeable.
struct Progress {
    enabled: bool,
    last: Option<Instant>,
    width: usize,
}

impl Progress {
    fn new() -> Self {
        Progress {
            enabled: std::io::stderr().is_terminal(),
            last: None,
            width: 0,
        }
    }

    fn tick(&mut self, dirs: u64, found: usize) {
        if !self.enabled {
            return;
        }
        if let Some(last) = self.last
            && last.elapsed() < Duration::from_millis(80)
        {
            return;
        }
        self.last = Some(Instant::now());

        let line = format!("scanning... {dirs} dirs, {found} artifacts");
        let len = line.chars().count();
        let pad = self.width.saturating_sub(len);
        let mut err = std::io::stderr();
        let _ = write!(err, "\r{line}{:pad$}\r", "", pad = pad);
        let _ = err.flush();
        self.width = len;
    }

    fn clear(&mut self) {
        if !self.enabled || self.width == 0 {
            return;
        }
        let mut err = std::io::stderr();
        let _ = write!(err, "\r{:width$}\r", "", width = self.width);
        let _ = err.flush();
        self.width = 0;
    }
}

fn print_json(
    args: &Args,
    artifacts: &[Artifact],
    selected: &[usize],
    stats: &ScanStats,
    now: SystemTime,
) -> Result<()> {
    let mut items = Vec::with_capacity(selected.len());
    for &i in selected {
        let artifact = &artifacts[i];
        let mut value = serde_json::to_value(artifact)?;
        if let serde_json::Value::Object(map) = &mut value {
            map.insert(
                "category_label".into(),
                serde_json::Value::from(artifact.category.label()),
            );
            map.insert(
                "badge_label".into(),
                serde_json::Value::from(artifact.badge.label()),
            );
            map.insert(
                "age_secs".into(),
                json_secs(artifact.signals.artifact_mtime, now),
            );
            map.insert("project_age_secs".into(), json_secs(newest(artifact), now));
        }
        items.push(value);
    }

    let reclaim: u64 = selected.iter().map(|&i| artifacts[i].sizes.reclaim).sum();
    let output = serde_json::json!({
        "artifacts": items,
        "stats": stats,
        "selected": { "count": selected.len(), "reclaim": reclaim },
        "dry_run": args.dry_run,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn print_table(
    artifacts: &[Artifact],
    selected: &[usize],
    stats: &ScanStats,
    now: SystemTime,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if selected.is_empty() {
        writeln!(out, "No artifacts matched.")?;
    } else {
        write_rows(&mut out, artifacts, selected, now)?;
    }
    writeln!(out)?;
    write_summary(&mut out, artifacts, selected, stats)?;
    Ok(())
}

fn print_dry_run(
    artifacts: &[Artifact],
    selected: &[usize],
    stats: &ScanStats,
    now: SystemTime,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if selected.is_empty() {
        writeln!(
            out,
            "Nothing would be removed: no artifact matched the filters with badge DEAD or SAFE."
        )?;
    } else {
        writeln!(
            out,
            "Would remove {} artifact(s) badged DEAD or SAFE:",
            selected.len()
        )?;
        writeln!(out)?;
        write_rows(&mut out, artifacts, selected, now)?;
    }

    let reclaim: u64 = selected.iter().map(|&i| artifacts[i].sizes.reclaim).sum();
    let shared: u64 = selected.iter().map(|&i| artifacts[i].sizes.shared).sum();
    writeln!(out)?;
    writeln!(
        out,
        "Would free {} ({} of the bytes listed are hardlinked elsewhere and would survive).",
        format_size(reclaim, DECIMAL),
        format_size(shared, DECIMAL)
    )?;
    writeln!(out, "Dry run: nothing was deleted.")?;
    writeln!(out)?;
    write_summary(&mut out, artifacts, selected, stats)?;
    Ok(())
}

fn write_rows(
    out: &mut impl Write,
    artifacts: &[Artifact],
    selected: &[usize],
    now: SystemTime,
) -> Result<()> {
    struct Row {
        badge: &'static str,
        category: &'static str,
        reclaim: String,
        size: String,
        age: String,
        path: String,
    }

    let rows: Vec<Row> = selected
        .iter()
        .map(|&i| {
            let a = &artifacts[i];
            Row {
                badge: a.badge.label(),
                category: a.category.label(),
                reclaim: format_size(a.sizes.reclaim, DECIMAL),
                size: format_size(a.sizes.apparent, DECIMAL),
                age: fmt_age(newest(a), now),
                path: a.path.display().to_string(),
            }
        })
        .collect();

    let w_badge = width(rows.iter().map(|r| r.badge), "BADGE");
    let w_cat = width(rows.iter().map(|r| r.category), "CATEGORY");
    let w_reclaim = width(rows.iter().map(|r| r.reclaim.as_str()), "RECLAIM");
    let w_size = width(rows.iter().map(|r| r.size.as_str()), "SIZE");
    let w_age = width(rows.iter().map(|r| r.age.as_str()), "AGE");

    writeln!(
        out,
        "{:<w_badge$}  {:<w_cat$}  {:>w_reclaim$}  {:>w_size$}  {:>w_age$}  PATH",
        "BADGE", "CATEGORY", "RECLAIM", "SIZE", "AGE"
    )?;
    for r in &rows {
        writeln!(
            out,
            "{:<w_badge$}  {:<w_cat$}  {:>w_reclaim$}  {:>w_size$}  {:>w_age$}  {}",
            r.badge, r.category, r.reclaim, r.size, r.age, r.path
        )?;
    }
    Ok(())
}

fn write_summary(
    out: &mut impl Write,
    artifacts: &[Artifact],
    selected: &[usize],
    stats: &ScanStats,
) -> Result<()> {
    let shown_apparent: u64 = selected.iter().map(|&i| artifacts[i].sizes.apparent).sum();
    let shown_reclaim: u64 = selected.iter().map(|&i| artifacts[i].sizes.reclaim).sum();
    let shown_shared: u64 = selected.iter().map(|&i| artifacts[i].sizes.shared).sum();
    let cross_linked = stats.total_apparent.saturating_sub(stats.total_unique);

    writeln!(
        out,
        "shown:   {} of {} artifacts - apparent {}, reclaimable {}, hardlinked elsewhere {}",
        selected.len(),
        artifacts.len(),
        format_size(shown_apparent, DECIMAL),
        format_size(shown_reclaim, DECIMAL),
        format_size(shown_shared, DECIMAL),
    )?;
    writeln!(
        out,
        "scanned: {} artifacts - apparent {}, session-unique {}, shared between artifacts {}",
        stats.artifacts_found,
        format_size(stats.total_apparent, DECIMAL),
        format_size(stats.total_unique, DECIMAL),
        format_size(cross_linked, DECIMAL),
    )?;
    writeln!(
        out,
        "walked:  {} directories, {} error(s)",
        stats.dirs_visited, stats.errors
    )?;
    Ok(())
}

fn width<'a>(values: impl Iterator<Item = &'a str>, header: &str) -> usize {
    values
        .map(|v| v.chars().count())
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or_else(|| header.chars().count())
}

/// The most recent activity timestamp known for an artifact.
fn newest(a: &Artifact) -> Option<SystemTime> {
    [
        a.signals.artifact_mtime,
        a.signals.project_mtime,
        a.signals.git_mtime,
    ]
    .into_iter()
    .flatten()
    .max()
}

fn age_secs(when: Option<SystemTime>, now: SystemTime) -> Option<u64> {
    when.map(|t| now.duration_since(t).map(|d| d.as_secs()).unwrap_or(0))
}

fn json_secs(when: Option<SystemTime>, now: SystemTime) -> serde_json::Value {
    match age_secs(when, now) {
        Some(secs) => serde_json::Value::from(secs),
        None => serde_json::Value::Null,
    }
}

fn fmt_age(when: Option<SystemTime>, now: SystemTime) -> String {
    const HOUR: u64 = 3600;
    const DAY: u64 = 24 * HOUR;
    match age_secs(when, now) {
        None => "-".to_string(),
        Some(secs) if secs < HOUR => format!("{}m", secs / 60),
        Some(secs) if secs < DAY => format!("{}h", secs / HOUR),
        Some(secs) if secs < 365 * DAY => format!("{}d", secs / DAY),
        Some(secs) => format!("{:.1}y", secs as f64 / (365.0 * DAY as f64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Signals, Sizes};
    use std::path::PathBuf;

    fn artifact(id: u64, mtime_secs_ago: u64) -> Artifact {
        let mut a = Artifact::new(id, PathBuf::from("/tmp/x"), Category::PyVenv, None);
        a.signals = Signals {
            artifact_mtime: Some(SystemTime::now() - Duration::from_secs(mtime_secs_ago)),
            ..Signals::default()
        };
        a.sizes = Sizes {
            apparent: 2048,
            reclaim: 1024,
            shared: 1024,
            ..Sizes::default()
        };
        a
    }

    #[test]
    fn age_formatting_scales_with_magnitude() {
        let now = SystemTime::now();
        assert_eq!(fmt_age(None, now), "-");
        assert_eq!(fmt_age(Some(now - Duration::from_secs(120)), now), "2m");
        assert_eq!(fmt_age(Some(now - Duration::from_secs(7200)), now), "2h");
        assert_eq!(
            fmt_age(Some(now - Duration::from_secs(86400 * 3)), now),
            "3d"
        );
        assert_eq!(
            fmt_age(Some(now - Duration::from_secs(86400 * 730)), now),
            "2.0y"
        );
    }

    #[test]
    fn future_timestamps_report_zero_age() {
        let now = SystemTime::now();
        assert_eq!(age_secs(Some(now + Duration::from_secs(600)), now), Some(0));
    }

    #[test]
    fn newest_picks_the_latest_known_signal() {
        let now = SystemTime::now();
        let mut a = artifact(1, 86400);
        a.signals.project_mtime = Some(now - Duration::from_secs(60));
        a.signals.git_mtime = Some(now - Duration::from_secs(3600));
        let picked = newest(&a).unwrap();
        assert!(now.duration_since(picked).unwrap() < Duration::from_secs(120));
    }

    #[test]
    fn rows_render_a_header_and_one_line_per_artifact() {
        let now = SystemTime::now();
        let artifacts = vec![artifact(1, 86400), artifact(2, 86400 * 400)];
        let mut buf: Vec<u8> = Vec::new();
        write_rows(&mut buf, &artifacts, &[0, 1], now).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("BADGE"));
        assert!(lines[0].ends_with("PATH"));
        assert!(lines[1].contains("/tmp/x"));
    }

    #[test]
    fn summary_reports_shown_and_scanned_totals() {
        let artifacts = vec![artifact(1, 0)];
        let stats = ScanStats {
            dirs_visited: 10,
            artifacts_found: 1,
            total_apparent: 2048,
            total_unique: 1024,
            errors: 0,
        };
        let mut buf: Vec<u8> = Vec::new();
        write_summary(&mut buf, &artifacts, &[0], &stats).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("shown:"));
        assert!(text.contains("session-unique"));
        assert!(text.contains("10 directories"));
    }
}
