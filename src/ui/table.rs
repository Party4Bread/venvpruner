//! The artifact table: one row per prunable directory.

use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Cell, Row, Table, TableState};

use super::theme;
use crate::model::Artifact;

const W_MARK: u16 = 3;
const W_BADGE: u16 = 6;
const W_CAT: u16 = 7;
const W_RECLAIM: u16 = 10;
const W_SIZE: u16 = 10;
const W_AGE: u16 = 6;
const COLUMN_SPACING: u16 = 1;

/// Rows visible in `area`, for PgUp/PgDn.
pub fn page_size(area: Rect) -> usize {
    // two border rows plus the header row.
    (area.height.saturating_sub(3)).max(1) as usize
}

/// Width left over for the path column once the fixed columns are laid out.
fn path_width(area: Rect) -> usize {
    let fixed = W_MARK + W_BADGE + W_CAT + W_RECLAIM + W_SIZE + W_AGE;
    let gaps = COLUMN_SPACING * 6;
    let inner = area.width.saturating_sub(2); // block borders
    inner.saturating_sub(fixed + gaps).max(6) as usize
}

/// Shorten `s` to `max` display cells by dropping characters from the LEFT, so
/// the tail — the part that actually identifies the directory — survives.
pub fn truncate_left(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let skip = count - (max - 1);
    let mut out = String::with_capacity(max * 4);
    out.push('…');
    out.extend(s.chars().skip(skip));
    out
}

fn age_cell(a: &Artifact, now: SystemTime) -> String {
    match a.signals.artifact_mtime {
        Some(t) => match now.duration_since(t) {
            Ok(d) => theme::age(d),
            Err(_) => theme::age(std::time::Duration::ZERO),
        },
        None => theme::UNKNOWN.to_string(),
    }
}

fn row_for(a: &Artifact, now: SystemTime, path_w: usize) -> Row<'static> {
    let mark = if a.marked { "[x]" } else { "[ ]" };
    let mark_style = if a.marked {
        theme::marked_row()
    } else {
        theme::dim()
    };

    let reclaim = format!(
        "{:>width$}",
        theme::bytes_opt(a.sizes.reclaim, a.sized),
        width = W_RECLAIM as usize
    );
    let size = format!(
        "{:>width$}",
        theme::bytes_opt(a.sizes.apparent, a.sized),
        width = W_SIZE as usize
    );
    let age = format!("{:>width$}", age_cell(a, now), width = W_AGE as usize);

    let path = truncate_left(&a.path.display().to_string(), path_w);

    let reclaim_style = if a.sized {
        theme::reclaim()
    } else {
        theme::dim()
    };
    let size_style = if a.sized {
        theme::plain()
    } else {
        theme::dim()
    };

    Row::new(vec![
        Cell::new(Span::styled(mark, mark_style)),
        Cell::new(Span::styled(
            theme::badge_text(a.badge),
            theme::badge_style(a.badge),
        )),
        Cell::new(Span::styled(
            a.category.label(),
            theme::category_style(a.category),
        )),
        Cell::new(Span::styled(reclaim, reclaim_style)),
        Cell::new(Span::styled(size, size_style)),
        Cell::new(Span::styled(age, theme::dim())),
        Cell::new(Span::styled(path, theme::plain())),
    ])
}

/// Render the table. `view` holds indices into `artifacts`, already filtered
/// and sorted; `state.selected()` indexes into `view`.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    artifacts: &[Artifact],
    view: &[usize],
    state: &mut TableState,
    now: SystemTime,
    title: &str,
) {
    let path_w = path_width(area);

    let rows: Vec<Row<'static>> = view
        .iter()
        .filter_map(|&i| artifacts.get(i))
        .map(|a| row_for(a, now, path_w))
        .collect();

    let header = Row::new(vec![
        Cell::new("   "),
        Cell::new("BADGE "),
        Cell::new("TYPE"),
        Cell::new(format!("{:>width$}", "RECLAIM", width = W_RECLAIM as usize)),
        Cell::new(format!("{:>width$}", "SIZE", width = W_SIZE as usize)),
        Cell::new(format!("{:>width$}", "AGE", width = W_AGE as usize)),
        Cell::new("PATH"),
    ])
    .style(theme::table_header());

    let widths = [
        Constraint::Length(W_MARK),
        Constraint::Length(W_BADGE),
        Constraint::Length(W_CAT),
        Constraint::Length(W_RECLAIM),
        Constraint::Length(W_SIZE),
        Constraint::Length(W_AGE),
        Constraint::Min(6),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(COLUMN_SPACING)
        .block(theme::panel(title))
        .row_highlight_style(theme::selected_row());

    frame.render_stateful_widget(table, area, state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_paths_are_untouched() {
        assert_eq!(truncate_left("/a/b", 10), "/a/b");
        assert_eq!(truncate_left("/a/b", 4), "/a/b");
    }

    #[test]
    fn long_paths_keep_their_tail() {
        let s = "/home/user/projects/deeply/nested/thing/.venv";
        let out = truncate_left(s, 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.starts_with('…'));
        assert!(out.ends_with("thing/.venv"));
    }

    #[test]
    fn degenerate_widths_do_not_panic() {
        assert_eq!(truncate_left("/very/long/path", 0), "");
        assert_eq!(truncate_left("/very/long/path", 1), "…");
    }

    #[test]
    fn multibyte_paths_are_not_split_mid_character() {
        let s = "/home/пользователь/проект/.venv";
        let out = truncate_left(s, 12);
        assert_eq!(out.chars().count(), 12);
        assert!(out.ends_with(".venv"));
    }

    #[test]
    fn path_column_stays_usable_at_eighty_columns() {
        let area = Rect::new(0, 0, 80, 24);
        assert!(path_width(area) >= 20, "got {}", path_width(area));
    }
}
