//! The type-DELETE-to-confirm modal, plus the modal geometry helper shared
//! with the other overlays.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};

use super::theme;

/// The word the user must type out in full before anything is removed.
pub const CONFIRM_WORD: &str = "DELETE";

/// Longer than `CONFIRM_WORD` so a typo is visible rather than silently eaten.
const MAX_INPUT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Keep the modal open.
    Pending,
    /// User backed out; nothing is deleted.
    Cancel,
    /// User typed the word and pressed Enter.
    Confirm,
}

/// Handle one key while the modal is up. Anything not explicitly handled is
/// ignored — in particular Enter on its own never confirms.
pub fn handle_key(key: KeyEvent, buf: &mut String) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::Pending;
    }
    match key.code {
        KeyCode::Esc => Action::Cancel,
        KeyCode::Backspace => {
            buf.pop();
            Action::Pending
        }
        KeyCode::Enter => {
            if buf == CONFIRM_WORD {
                Action::Confirm
            } else {
                Action::Pending
            }
        }
        KeyCode::Char(c) => {
            if buf.chars().count() < MAX_INPUT {
                buf.push(c);
            }
            Action::Pending
        }
        _ => Action::Pending,
    }
}

/// A centred rectangle `w` x `h`, clamped to `area`.
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Split a full-screen area into the modal box and render `Clear` under it.
pub fn open(frame: &mut Frame, area: Rect, w: u16, h: u16) -> Rect {
    let r = centered(area, w, h);
    frame.render_widget(Clear, r);
    r
}

/// What the modal has to state before anything is removed.
pub struct Summary {
    /// Marked artifacts, visible or not.
    pub count: usize,
    /// Sum of their `reclaim`. A floor when `measuring` is non-zero.
    pub reclaim: u64,
    /// Marked artifacts still being sized; they contribute 0 to `reclaim`.
    pub measuring: usize,
    /// Marked artifacts the current filter hides. They go too.
    pub hidden: usize,
}

pub fn render(frame: &mut Frame, area: Rect, summary: &Summary, buf: &str) {
    let extra = u16::from(summary.measuring > 0) + u16::from(summary.hidden > 0) * 2;
    let r = open(frame, area, 68, 13 + extra);
    let block = theme::modal("confirm deletion", theme::danger_bold());
    let inner = block.inner(r);
    frame.render_widget(block, r);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(inner);

    let count = summary.count;
    let plural = if count == 1 { "artifact" } else { "artifacts" };
    let mut headline = vec![
        Span::styled("About to delete ", theme::plain()),
        Span::styled(format!("{count} {plural}"), theme::danger_bold()),
        Span::styled(", freeing ", theme::plain()),
    ];
    // "about 0 B" while the scan is still measuring understates a destructive
    // action; say the number is a floor and how many are still outstanding.
    if summary.measuring > 0 {
        headline.push(Span::styled("at least ", theme::warn()));
    } else {
        headline.push(Span::styled("about ", theme::plain()));
    }
    headline.push(Span::styled(
        theme::bytes(summary.reclaim),
        theme::reclaim(),
    ));
    headline.push(Span::styled(".", theme::plain()));

    let mut body = vec![Line::default(), Line::from(headline)];
    if summary.measuring > 0 {
        body.push(Line::from(Span::styled(
            format!(
                "{} of them are still being measured and count as 0 B above.",
                summary.measuring
            ),
            theme::warn(),
        )));
    }
    body.push(Line::default());
    if summary.hidden > 0 {
        body.push(Line::from(Span::styled(
            format!(
                "{} of them are hidden by the current filter and are not on screen. \
                 Esc, then n, clears every mark.",
                summary.hidden
            ),
            theme::danger(),
        )));
        body.push(Line::default());
    }
    body.extend([
        Line::from(Span::styled(
            "This removes the directories from disk immediately. There is no undo \
             and nothing goes to a trash folder.",
            theme::warn(),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("Type ", theme::plain()),
            Span::styled(CONFIRM_WORD, theme::danger_bold()),
            Span::styled(" and press Enter to proceed  ·  Esc cancels", theme::dim()),
        ]),
    ]);
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), rows[0]);

    let ok = buf == CONFIRM_WORD;
    let field_style = if ok { theme::reclaim() } else { theme::warn() };
    let hint = if ok { "  ✓ press Enter" } else { "" };
    let input = Line::from(vec![
        Span::styled("> ", theme::dim()),
        Span::styled(buf.to_string(), field_style),
        Span::styled("_", theme::dim()),
        Span::styled(hint, theme::reclaim()),
    ]);
    frame.render_widget(
        Paragraph::new(input)
            .block(theme::panel("type to confirm"))
            .alignment(Alignment::Left),
        rows[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn type_str(s: &str, buf: &mut String) -> Action {
        let mut last = Action::Pending;
        for c in s.chars() {
            last = handle_key(key(KeyCode::Char(c)), buf);
        }
        last
    }

    #[test]
    fn enter_alone_never_confirms() {
        let mut buf = String::new();
        assert_eq!(handle_key(key(KeyCode::Enter), &mut buf), Action::Pending);
    }

    #[test]
    fn partial_or_wrong_word_does_not_confirm() {
        let mut buf = String::new();
        type_str("DELET", &mut buf);
        assert_eq!(handle_key(key(KeyCode::Enter), &mut buf), Action::Pending);

        let mut buf = String::new();
        type_str("delete", &mut buf);
        assert_eq!(handle_key(key(KeyCode::Enter), &mut buf), Action::Pending);
    }

    #[test]
    fn full_word_then_enter_confirms() {
        let mut buf = String::new();
        type_str(CONFIRM_WORD, &mut buf);
        assert_eq!(handle_key(key(KeyCode::Enter), &mut buf), Action::Confirm);
    }

    #[test]
    fn backspace_edits_and_esc_cancels() {
        let mut buf = String::new();
        type_str("DELETEX", &mut buf);
        handle_key(key(KeyCode::Backspace), &mut buf);
        assert_eq!(buf, CONFIRM_WORD);
        assert_eq!(handle_key(key(KeyCode::Esc), &mut buf), Action::Cancel);
    }

    #[test]
    fn input_is_bounded() {
        let mut buf = String::new();
        type_str(&"x".repeat(200), &mut buf);
        assert_eq!(buf.chars().count(), MAX_INPUT);
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut buf = String::from(CONFIRM_WORD);
        let mut ev = key(KeyCode::Enter);
        ev.kind = KeyEventKind::Release;
        assert_eq!(handle_key(ev, &mut buf), Action::Pending);
    }

    #[test]
    fn other_keys_do_nothing() {
        let mut buf = String::from("DEL");
        assert_eq!(handle_key(key(KeyCode::F(5)), &mut buf), Action::Pending);
        assert_eq!(handle_key(key(KeyCode::Left), &mut buf), Action::Pending);
        assert_eq!(buf, "DEL");
    }

    #[test]
    fn centered_box_fits_tiny_terminals() {
        let area = Rect::new(0, 0, 20, 5);
        let r = centered(area, 68, 13);
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 5);
        assert!(r.right() <= area.right() && r.bottom() <= area.bottom());
    }
}
