//! Single place where colours, styles and value formatting live.
//!
//! Nothing here sets a background colour for the whole app: the terminal's own
//! background shows through, which keeps the palette usable on light and dark
//! themes alike.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders};

use crate::model::{Badge, Category};

/// Primary accent, used for borders of the focused area and for headings.
pub const ACCENT: Color = Color::Cyan;
/// Colour for numbers the user is likely to act on (reclaimable bytes).
pub const RECLAIM: Color = Color::LightGreen;
/// Warnings, pending confirmation, "review this" states.
pub const WARN: Color = Color::Yellow;
/// Destructive / active-project states.
pub const DANGER: Color = Color::Red;

pub fn plain() -> Style {
    Style::default()
}

pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub fn heading() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn label() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub fn reclaim() -> Style {
    Style::default().fg(RECLAIM)
}

pub fn warn() -> Style {
    Style::default().fg(WARN)
}

pub fn danger() -> Style {
    Style::default().fg(DANGER)
}

pub fn danger_bold() -> Style {
    Style::default().fg(DANGER).add_modifier(Modifier::BOLD)
}

/// Table header row.
pub fn table_header() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Selected table row. Reversed video works on any terminal palette.
pub fn selected_row() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// A marked (checked) row, so marks stay visible while scrolling past them.
pub fn marked_row() -> Style {
    Style::default().fg(RECLAIM).add_modifier(Modifier::BOLD)
}

pub fn badge_style(b: Badge) -> Style {
    match b {
        Badge::Dead => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
        Badge::Safe => Style::default().fg(Color::Green),
        Badge::Review => Style::default().fg(WARN),
        Badge::Active => Style::default().fg(DANGER),
    }
}

/// Fixed-width badge text so the column never jitters.
pub fn badge_text(b: Badge) -> &'static str {
    match b {
        Badge::Dead => "DEAD  ",
        Badge::Safe => "SAFE  ",
        Badge::Review => "REVIEW",
        Badge::Active => "ACTIVE",
    }
}

pub fn category_style(c: Category) -> Style {
    let colour = match c {
        Category::PyVenv => Color::Blue,
        Category::UvTool => Color::LightBlue,
        Category::Pipx => Color::Magenta,
        Category::CondaEnv => Color::LightMagenta,
        Category::NodeModules => Color::LightYellow,
        Category::CargoTarget => Color::LightRed,
        Category::Pycache => Color::LightCyan,
        Category::Cache => Color::Gray,
    };
    Style::default().fg(colour)
}

/// Style for the category toggles in the header: enabled ones stand out.
pub fn category_toggle(c: Category, enabled: bool) -> Style {
    if enabled {
        category_style(c)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM)
    }
}

pub fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(ratatui::text::Span::styled(format!(" {title} "), heading()))
}

/// Modal frame: heavier border so it clearly floats above the app.
pub fn modal(title: &str, style: Style) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(style)
        .title(ratatui::text::Span::styled(format!(" {title} "), style))
}

/// Byte formatting. One implementation, used everywhere.
pub fn bytes(n: u64) -> String {
    humansize::format_size(n, humansize::DECIMAL)
}

/// Byte formatting for a value that has not been measured yet.
pub fn bytes_opt(n: u64, known: bool) -> String {
    if known { bytes(n) } else { "…".to_string() }
}

/// Age formatting. Delegates to the scanner so the CLI and the TUI agree.
pub fn age(d: Duration) -> String {
    crate::scan::signals::humanize_age(d)
}

/// Placeholder for a value we could not determine.
pub const UNKNOWN: &str = "—";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_are_padded_to_the_same_width() {
        let w = badge_text(Badge::Review).len();
        for b in [Badge::Dead, Badge::Safe, Badge::Review, Badge::Active] {
            assert_eq!(badge_text(b).len(), w, "{:?} badge width", b);
        }
    }

    #[test]
    fn unsized_rows_do_not_render_as_zero() {
        assert_eq!(bytes_opt(0, false), "…");
        assert_ne!(bytes_opt(0, true), "…");
    }
}
