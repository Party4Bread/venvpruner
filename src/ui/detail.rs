//! Detail pane for the currently selected artifact.

use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::theme;
use crate::model::Artifact;

fn field<'a>(name: &'a str, value: impl Into<String>) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{name:<12}"), theme::label()),
        Span::styled(value.into(), theme::plain()),
    ])
}

fn ago(t: Option<SystemTime>, now: SystemTime) -> String {
    match t {
        Some(t) => match now.duration_since(t) {
            Ok(d) => format!("{} ago", theme::age(d)),
            Err(_) => "just now".to_string(),
        },
        None => theme::UNKNOWN.to_string(),
    }
}

pub fn lines(a: &Artifact, now: SystemTime) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    out.push(Line::from(Span::styled(
        a.path.display().to_string(),
        theme::bold(),
    )));

    out.push(Line::from(vec![
        Span::styled(format!("{:<12}", "kind"), theme::label()),
        Span::styled(a.category.label(), theme::category_style(a.category)),
        Span::raw("   "),
        Span::styled(
            theme::badge_text(a.badge).trim_end(),
            theme::badge_style(a.badge),
        ),
    ]));

    if let Some(p) = &a.project {
        out.push(field("project", p.display().to_string()));
    }

    out.push(Line::default());

    if a.sized {
        out.push(field(
            "size",
            format!(
                "{} on disk  ·  {} files  ·  {} dirs{}",
                theme::bytes(a.sizes.apparent),
                a.sizes.files,
                a.sizes.dirs,
                if a.sizes.partial { "  (partial)" } else { "" }
            ),
        ));
        out.push(Line::from(vec![
            Span::styled(format!("{:<12}", "reclaim"), theme::label()),
            Span::styled(theme::bytes(a.sizes.reclaim), theme::reclaim()),
            Span::styled(" freed by deleting this", theme::dim()),
        ]));
        if a.sizes.shared > 0 {
            out.push(Line::from(vec![
                Span::styled(format!("{:<12}", "shared"), theme::label()),
                Span::styled(theme::bytes(a.sizes.shared), theme::warn()),
                Span::styled(
                    " is hardlinked elsewhere and will NOT be freed",
                    theme::warn(),
                ),
            ]));
        }
        if a.sizes.partial {
            out.push(Line::from(Span::styled(
                "            some entries were unreadable; sizes are a lower bound",
                theme::dim(),
            )));
        }
    } else {
        out.push(field("size", "measuring…"));
    }

    out.push(Line::default());

    if let Some(v) = &a.signals.python_version {
        out.push(field("python", v.clone()));
    }
    match a.signals.interpreter_missing {
        Some(true) => out.push(Line::from(vec![
            Span::styled(format!("{:<12}", "interpreter"), theme::label()),
            Span::styled("missing — this env cannot be activated", theme::danger()),
        ])),
        Some(false) => out.push(field("interpreter", "present")),
        None => {}
    }

    if a.signals.reinstall_from.is_empty() {
        out.push(field(
            "reinstall",
            "no lockfile or manifest found next to it",
        ));
    } else {
        out.push(field("reinstall", a.signals.reinstall_from.join(", ")));
    }

    out.push(field("modified", ago(a.signals.artifact_mtime, now)));
    out.push(field("project act", ago(a.signals.project_mtime, now)));
    out.push(field("git act", ago(a.signals.git_mtime, now)));

    if !a.badge_reasons.is_empty() {
        out.push(Line::default());
        out.push(Line::from(Span::styled("why:", theme::label())));
        for r in &a.badge_reasons {
            out.push(Line::from(vec![
                Span::styled("  · ", theme::dim()),
                Span::styled(r.clone(), theme::plain()),
            ]));
        }
    } else if !a.analyzed {
        out.push(Line::default());
        out.push(Line::from(Span::styled("analysing…", theme::dim())));
    }

    out
}

pub fn render(frame: &mut Frame, area: Rect, selected: Option<&Artifact>, now: SystemTime) {
    let body: Vec<Line<'static>> = match selected {
        Some(a) => lines(a, now),
        None => vec![Line::from(Span::styled(
            "no artifact selected",
            theme::dim(),
        ))],
    };

    let p = Paragraph::new(body)
        .block(theme::panel("details"))
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Artifact, Category, Sizes};
    use std::path::PathBuf;
    use std::time::Duration;

    fn artifact() -> Artifact {
        Artifact::new(1, PathBuf::from("/tmp/x/.venv"), Category::PyVenv, None)
    }

    fn text_of(ls: &[Line<'static>]) -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shared_bytes_are_called_out_explicitly() {
        let mut a = artifact();
        a.sized = true;
        a.sizes = Sizes {
            apparent: 1000,
            reclaim: 400,
            shared: 600,
            files: 3,
            dirs: 1,
            partial: false,
        };
        let t = text_of(&lines(&a, SystemTime::now()));
        assert!(t.contains("hardlinked elsewhere"), "{t}");
        assert!(t.contains("NOT be freed"), "{t}");
    }

    #[test]
    fn no_shared_line_when_nothing_is_shared() {
        let mut a = artifact();
        a.sized = true;
        a.sizes = Sizes {
            apparent: 1000,
            reclaim: 1000,
            ..Default::default()
        };
        let t = text_of(&lines(&a, SystemTime::now()));
        assert!(!t.contains("hardlinked"), "{t}");
    }

    #[test]
    fn unsized_artifact_does_not_claim_to_be_empty() {
        let t = text_of(&lines(&artifact(), SystemTime::now()));
        assert!(t.contains("measuring"), "{t}");
        assert!(!t.contains("0 B"), "{t}");
    }

    #[test]
    fn missing_interpreter_is_reported() {
        let mut a = artifact();
        a.signals.interpreter_missing = Some(true);
        let t = text_of(&lines(&a, SystemTime::now()));
        assert!(t.contains("cannot be activated"), "{t}");
    }

    #[test]
    fn future_mtimes_do_not_panic() {
        let mut a = artifact();
        a.signals.artifact_mtime = Some(SystemTime::now() + Duration::from_secs(3600));
        let t = text_of(&lines(&a, SystemTime::now()));
        assert!(t.contains("just now"), "{t}");
    }
}
