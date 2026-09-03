//! Interactive terminal UI. `run` owns the terminal from setup to teardown.

mod confirm;
mod detail;
mod table;
mod theme;

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Once;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, TryRecvError};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Gauge, Paragraph, TableState, Wrap};

use crate::filter::{self, FilterState};
use crate::model::{Artifact, ArtifactId, Category, ScanEvent, ScanStats};
use crate::scan::ScanConfig;

const TICK: Duration = Duration::from_millis(50);
const FRAME_MIN: Duration = Duration::from_millis(50);
const SPINNER_STEP: Duration = Duration::from_millis(100);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_ERRORS: usize = 500;
/// Cap on scan events consumed per tick so input never starves during a flood.
const EVENT_BUDGET: usize = 4096;

/// Short enough to survive an 80-column terminal intact: the footer row is one
/// line with no wrapping, so a longer string loses its qualifier mid-sentence
/// and reads as a promise. The full version lives in the help overlay.
const BTRFS_NOTE: &str =
    "reclaim = freed by deleting this alone · on btrfs/zfs the real gain differs";

/// The unabridged form of [`BTRFS_NOTE`], shown in the help overlay where
/// there is room to wrap it.
const RECLAIM_HELP: [&str; 2] = [
    "reclaim is the bytes freed by deleting that artifact alone: inodes whose every hardlink \
     lives inside it. Bytes it shares with something else are listed separately and survive.",
    "On btrfs/zfs the space the filesystem hands back can still differ: compression, snapshots \
     and reflinked copies (cp --reflink, the default of cp on btrfs) all share extents that \
     st_blocks reports in full for each copy.",
];

/// Restore the terminal to a sane state. Safe to call more than once.
fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

static PANIC_HOOK: Once = Once::new();

/// Make sure a panic leaves a usable shell behind: restore first, then let the
/// previous hook print its message onto the normal screen.
fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
    });
}

/// Owns the raw-mode / alternate-screen state so every exit path — return,
/// error, unwind — puts the terminal back.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Constructed before anything else can fail: from here on every exit
        // path runs Drop. Building it after the `execute!` below would leave
        // raw mode enabled with nothing to turn it off if that write failed.
        let guard = TerminalGuard;
        // Mouse capture is deliberately not enabled: it would steal
        // click-drag selection and scrollback from the user's terminal.
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
    Confirm,
    Deleting,
    Results,
}

enum DeleteMsg {
    Item {
        id: ArtifactId,
        path: PathBuf,
        result: Result<crate::delete::Outcome, String>,
    },
    Finished,
}

pub fn run(cfg: ScanConfig) -> anyhow::Result<()> {
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;
    terminal.clear()?;

    let scan_rx = crate::scan::spawn(cfg.clone());
    let mut app = App::new(&cfg);

    let mut last_draw = Instant::now() - FRAME_MIN;
    let mut last_spin = Instant::now();

    while !app.quit {
        if event::poll(TICK)? {
            // Drain everything already buffered so held keys do not lag.
            loop {
                match event::read()? {
                    Event::Key(k) => app.on_key(k),
                    Event::Resize(_, _) => app.dirty = true,
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }

        app.drain_scan(&scan_rx);
        app.drain_deletes();

        if (app.scanning || app.mode == Mode::Deleting) && last_spin.elapsed() >= SPINNER_STEP {
            app.spinner = (app.spinner + 1) % SPINNER.len();
            last_spin = Instant::now();
            app.dirty = true;
        }

        if app.dirty && last_draw.elapsed() >= FRAME_MIN {
            app.now = SystemTime::now();
            app.refresh_view();
            terminal.draw(|f| app.render(f))?;
            app.dirty = false;
            last_draw = Instant::now();
        }
    }

    Ok(())
}

struct App {
    roots: Vec<PathBuf>,
    artifacts: Vec<Artifact>,
    by_id: HashMap<ArtifactId, usize>,
    filter: FilterState,
    /// Indices into `artifacts`, filtered and sorted. Recomputed only when
    /// `view_dirty` is set, never per frame.
    view: Vec<usize>,
    table: TableState,
    /// The selected ARTIFACT, not the selected row: rows re-sort underneath us
    /// as sizes stream in, and the cursor must not drift.
    selected: Option<ArtifactId>,
    scanning: bool,
    stats: ScanStats,
    dirs_visited: u64,
    errors: Vec<String>,
    mode: Mode,
    confirm_buf: String,
    show_help: bool,
    spinner: usize,
    dirty: bool,
    view_dirty: bool,
    /// Rows visible in the table on the last frame, for PgUp/PgDn.
    page: usize,
    now: SystemTime,
    flash: Option<String>,
    delete_rx: Option<Receiver<DeleteMsg>>,
    del_total: usize,
    del_done: usize,
    del_freed: u64,
    del_failures: Vec<(PathBuf, String)>,
    session_freed: u64,
    quit: bool,
}

impl App {
    fn new(cfg: &ScanConfig) -> Self {
        let mut filter = FilterState::default();
        if !cfg.categories.is_empty() {
            filter.categories = cfg.categories.clone();
        }
        App {
            roots: cfg.roots.clone(),
            artifacts: Vec::new(),
            by_id: HashMap::new(),
            filter,
            view: Vec::new(),
            table: TableState::default(),
            selected: None,
            scanning: true,
            stats: ScanStats::default(),
            dirs_visited: 0,
            errors: Vec::new(),
            mode: Mode::Normal,
            confirm_buf: String::new(),
            show_help: false,
            spinner: 0,
            dirty: true,
            view_dirty: true,
            page: 10,
            now: SystemTime::now(),
            flash: None,
            delete_rx: None,
            del_total: 0,
            del_done: 0,
            del_freed: 0,
            del_failures: Vec::new(),
            session_freed: 0,
            quit: false,
        }
    }

    // ---- streaming input -------------------------------------------------

    fn drain_scan(&mut self, rx: &Receiver<ScanEvent>) {
        let mut budget = EVENT_BUDGET;
        while budget > 0 {
            budget -= 1;
            match rx.try_recv() {
                Ok(ev) => self.apply_scan(ev),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.scanning {
                        self.scanning = false;
                        self.dirty = true;
                    }
                    break;
                }
            }
        }
    }

    fn apply_scan(&mut self, ev: ScanEvent) {
        match ev {
            ScanEvent::Found(a) => {
                let id = a.id;
                self.by_id.insert(id, self.artifacts.len());
                self.artifacts.push(*a);
                self.view_dirty = true;
            }
            ScanEvent::Sized(id, sizes) => {
                if let Some(&i) = self.by_id.get(&id) {
                    let a = &mut self.artifacts[i];
                    a.sizes = sizes;
                    a.sized = true;
                    self.view_dirty = true;
                }
            }
            ScanEvent::Analyzed(id, signals, badge, reasons) => {
                if let Some(&i) = self.by_id.get(&id) {
                    let a = &mut self.artifacts[i];
                    a.signals = *signals;
                    a.badge = badge;
                    a.badge_reasons = reasons;
                    a.analyzed = true;
                    self.view_dirty = true;
                }
            }
            ScanEvent::Progress { dirs_visited } => self.dirs_visited = dirs_visited,
            ScanEvent::Error(e) => {
                if self.errors.len() < MAX_ERRORS {
                    self.errors.push(e);
                }
            }
            ScanEvent::Done(stats) => {
                self.stats = stats;
                self.dirs_visited = stats.dirs_visited;
                self.scanning = false;
            }
        }
        self.dirty = true;
    }

    fn drain_deletes(&mut self) {
        let Some(rx) = self.delete_rx.clone() else {
            return;
        };
        let mut removed: Vec<ArtifactId> = Vec::new();
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(DeleteMsg::Item { id, path, result }) => {
                    self.del_done += 1;
                    match result {
                        Ok(outcome) => {
                            // A partial removal freed real bytes and left a
                            // real remainder: credit the one, report the other,
                            // and keep the row so the leftovers stay visible.
                            self.del_freed += outcome.freed;
                            if let Some(why) = outcome.error {
                                self.del_failures.push((path, why));
                            }
                            if outcome.complete {
                                removed.push(id);
                            } else {
                                self.shrink_artifact(id, outcome.freed);
                            }
                        }
                        Err(e) => self.del_failures.push((path, e)),
                    }
                    self.dirty = true;
                }
                Ok(DeleteMsg::Finished) => {
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }

        if !removed.is_empty() {
            let gone: std::collections::HashSet<ArtifactId> = removed.into_iter().collect();
            self.artifacts.retain(|a| !gone.contains(&a.id));
            self.reindex();
            self.view_dirty = true;
        }

        if finished {
            self.session_freed += self.del_freed;
            self.delete_rx = None;
            self.mode = Mode::Results;
            self.dirty = true;
        }
    }

    /// A partly-deleted artifact stays in the list, so its row must stop
    /// advertising the bytes that are already gone.
    fn shrink_artifact(&mut self, id: ArtifactId, freed: u64) {
        let Some(&i) = self.by_id.get(&id) else {
            return;
        };
        let Some(a) = self.artifacts.get_mut(i) else {
            return;
        };
        a.sizes.apparent = a.sizes.apparent.saturating_sub(freed);
        a.sizes.reclaim = a.sizes.reclaim.saturating_sub(freed);
        // What is left was measured before the removal, not after it.
        a.sizes.partial = true;
        a.marked = false;
    }

    fn reindex(&mut self) {
        self.by_id.clear();
        for (i, a) in self.artifacts.iter().enumerate() {
            self.by_id.insert(a.id, i);
        }
    }

    // ---- view / selection ------------------------------------------------

    fn refresh_view(&mut self) {
        if !self.view_dirty {
            return;
        }
        let prev_row = self.table.selected();
        self.view = filter::apply(&self.artifacts, &self.filter, self.now);

        let row = self
            .selected
            .and_then(|id| self.view.iter().position(|&i| self.artifacts[i].id == id))
            .or_else(|| {
                if self.view.is_empty() {
                    None
                } else {
                    Some(prev_row.unwrap_or(0).min(self.view.len() - 1))
                }
            });
        self.table.select(row);
        self.sync_selected();
        self.view_dirty = false;
    }

    fn sync_selected(&mut self) {
        self.selected = self
            .table
            .selected()
            .and_then(|r| self.view.get(r))
            .and_then(|&i| self.artifacts.get(i))
            .map(|a| a.id);
    }

    fn selected_artifact(&self) -> Option<&Artifact> {
        self.table
            .selected()
            .and_then(|r| self.view.get(r))
            .and_then(|&i| self.artifacts.get(i))
    }

    fn move_by(&mut self, delta: isize) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() - 1;
        let cur = self.table.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, last as isize) as usize;
        self.table.select(Some(next));
        self.sync_selected();
    }

    fn select_row(&mut self, row: usize) {
        if self.view.is_empty() {
            return;
        }
        self.table.select(Some(row.min(self.view.len() - 1)));
        self.sync_selected();
    }

    fn marked(&self) -> impl Iterator<Item = &Artifact> {
        self.artifacts.iter().filter(|a| a.marked)
    }

    fn marked_count(&self) -> usize {
        self.marked().count()
    }

    fn marked_reclaim(&self) -> u64 {
        self.marked().map(|a| a.sizes.reclaim).sum()
    }

    /// Marked artifacts whose sizing has not finished. Their bytes count as
    /// zero in [`marked_reclaim`](Self::marked_reclaim), so any total built
    /// from it is a lower bound while this is non-zero — and saying "0 B" in a
    /// deletion prompt because the scan is still running understates the act.
    fn marked_unsized(&self) -> usize {
        self.marked().filter(|a| !a.sized).count()
    }

    /// Marked artifacts the current filter hides. They are deleted along with
    /// the visible ones, so the confirmation has to admit they exist.
    fn marked_hidden(&self) -> usize {
        let visible: std::collections::HashSet<ArtifactId> = self
            .view
            .iter()
            .filter_map(|&i| self.artifacts.get(i))
            .map(|a| a.id)
            .collect();
        self.marked().filter(|a| !visible.contains(&a.id)).count()
    }

    fn total_apparent(&self) -> u64 {
        self.artifacts.iter().map(|a| a.sizes.apparent).sum()
    }

    // ---- keys ------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.dirty = true;

        // `view` holds indices into `artifacts`, and a delete run or a filter
        // change can invalidate them before the next frame is due. Keys are
        // drained in batches ahead of the draw, so refresh here rather than
        // acting on indices the last frame left behind.
        self.refresh_view();

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Not while a deletion is in flight: quitting kills the worker
            // wherever it is, which can be halfway through a `remove_dir_all`,
            // and the user never sees which artifact that was.
            if self.mode == Mode::Deleting {
                self.flash = Some("deletion in progress — cannot quit until it finishes".into());
                return;
            }
            self.quit = true;
            return;
        }

        if self.show_help {
            self.show_help = false;
            return;
        }

        match self.mode {
            Mode::Normal => self.key_normal(key),
            Mode::Search => self.key_search(key),
            Mode::Confirm => self.key_confirm(key),
            // A deletion is in flight on another thread; there is nothing safe
            // to do with a keystroke here.
            Mode::Deleting => {}
            Mode::Results => self.key_results(key),
        }
    }

    fn key_normal(&mut self, key: KeyEvent) {
        let page = self.page;
        self.flash = None;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(page as isize)),
            KeyCode::PageDown => self.move_by(page as isize),
            KeyCode::Home | KeyCode::Char('g') => self.select_row(0),
            KeyCode::End | KeyCode::Char('G') => self.select_row(self.view.len().saturating_sub(1)),
            KeyCode::Char(' ') => {
                if let Some(&i) = self.table.selected().and_then(|r| self.view.get(r))
                    && let Some(a) = self.artifacts.get_mut(i)
                {
                    a.marked = !a.marked;
                }
            }
            KeyCode::Char('a') => {
                for row in 0..self.view.len() {
                    let i = self.view[row];
                    if let Some(a) = self.artifacts.get_mut(i) {
                        a.marked = true;
                    }
                }
            }
            KeyCode::Char('n') => {
                for a in &mut self.artifacts {
                    a.marked = false;
                }
            }
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Char('s') => {
                self.filter.sort = self.filter.sort.next();
                self.view_dirty = true;
            }
            KeyCode::Char('r') => {
                self.filter.descending = !self.filter.descending;
                self.view_dirty = true;
            }
            KeyCode::Char(c @ '1'..='8') => {
                let idx = (c as u8 - b'1') as usize;
                let cat = Category::ALL[idx];
                if !self.filter.categories.remove(&cat) {
                    self.filter.categories.insert(cat);
                }
                self.view_dirty = true;
            }
            KeyCode::Char('d') => {
                if self.marked_count() == 0 {
                    self.flash = Some("nothing marked — press space to mark a row".into());
                } else {
                    self.confirm_buf.clear();
                    self.mode = Mode::Confirm;
                }
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
    }

    fn key_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.query.clear();
                self.mode = Mode::Normal;
                self.view_dirty = true;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.query.pop();
                self.view_dirty = true;
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.page as isize)),
            KeyCode::PageDown => self.move_by(self.page as isize),
            KeyCode::Char(c) => {
                self.filter.query.push(c);
                self.view_dirty = true;
            }
            _ => {}
        }
    }

    fn key_confirm(&mut self, key: KeyEvent) {
        let mut buf = std::mem::take(&mut self.confirm_buf);
        let action = confirm::handle_key(key, &mut buf);
        self.confirm_buf = buf;
        match action {
            confirm::Action::Pending => {}
            confirm::Action::Cancel => {
                self.confirm_buf.clear();
                self.mode = Mode::Normal;
            }
            confirm::Action::Confirm => {
                self.confirm_buf.clear();
                self.start_delete();
            }
        }
    }

    fn key_results(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => self.mode = Mode::Normal,
            _ => {}
        }
    }

    // ---- deletion --------------------------------------------------------

    fn start_delete(&mut self) {
        let targets: Vec<Artifact> = self.marked().cloned().collect();
        if targets.is_empty() {
            self.mode = Mode::Normal;
            return;
        }

        let total = targets.len();
        let (tx, rx) = crossbeam_channel::unbounded();
        let roots = self.roots.clone();
        // Off the UI thread: removing an 11 GB tree must not freeze redraws.
        std::thread::spawn(move || {
            for a in targets {
                let result = crate::delete::delete(&a, &roots);
                let msg = DeleteMsg::Item {
                    id: a.id,
                    path: a.path.clone(),
                    result,
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
            let _ = tx.send(DeleteMsg::Finished);
        });

        self.del_total = total;
        self.del_done = 0;
        self.del_freed = 0;
        self.del_failures.clear();
        self.delete_rx = Some(rx);
        self.mode = Mode::Deleting;
    }

    // ---- rendering -------------------------------------------------------

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        let rows = Layout::vertical([
            // Four lines plus two border rows. At 5 the category-toggle line
            // was clipped at every terminal size, hiding the only readout of
            // which types the 1-8 keys have switched off.
            Constraint::Length(6),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);

        self.render_header(f, rows[0]);
        self.render_body(f, rows[1]);
        self.render_footer(f, rows[2]);

        if self.show_help {
            self.render_help(f, area);
        }
        match self.mode {
            Mode::Confirm => confirm::render(
                f,
                area,
                &confirm::Summary {
                    count: self.marked_count(),
                    reclaim: self.marked_reclaim(),
                    measuring: self.marked_unsized(),
                    hidden: self.marked_hidden(),
                },
                &self.confirm_buf,
            ),
            Mode::Deleting => self.render_deleting(f, area),
            Mode::Results => self.render_results(f, area),
            _ => {}
        }
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let roots = self
            .roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("  ");

        let status = if self.scanning {
            Span::styled(
                format!(
                    "{} scanning · {} dirs · {} found",
                    SPINNER[self.spinner],
                    self.dirs_visited,
                    self.artifacts.len()
                ),
                theme::accent(),
            )
        } else {
            Span::styled(
                format!(
                    "scan complete · {} dirs · {} found",
                    self.dirs_visited,
                    self.artifacts.len()
                ),
                theme::dim(),
            )
        };

        let mut status_line = vec![
            Span::styled(format!("{:<8}", "status"), theme::label()),
            status,
        ];
        if !self.errors.is_empty() {
            status_line.push(Span::styled(
                format!(" · {} unreadable", self.errors.len()),
                theme::warn(),
            ));
        }

        let unique = if self.stats.total_unique > 0 {
            theme::bytes(self.stats.total_unique)
        } else {
            "…".to_string()
        };
        let mut totals = vec![
            Span::styled(format!("{:<8}", "totals"), theme::label()),
            Span::styled(theme::bytes(self.total_apparent()), theme::plain()),
            Span::styled(" on disk · ", theme::dim()),
            Span::styled(unique, theme::plain()),
            Span::styled(" unique · marked ", theme::dim()),
            Span::styled(self.marked_count().to_string(), theme::marked_row()),
            Span::styled(" → ", theme::dim()),
        ];
        // A marked row that is still being sized contributes 0 to the sum, so
        // the number is a floor until every one of them has landed.
        let pending = self.marked_unsized();
        if pending > 0 {
            totals.push(Span::styled("≥ ", theme::dim()));
        }
        totals.push(Span::styled(
            theme::bytes(self.marked_reclaim()),
            theme::reclaim(),
        ));
        if pending > 0 {
            totals.push(Span::styled(
                format!(" ({pending} still measuring)"),
                theme::warn(),
            ));
        }
        if self.session_freed > 0 {
            totals.push(Span::styled(" · freed ", theme::dim()));
            totals.push(Span::styled(
                theme::bytes(self.session_freed),
                theme::reclaim(),
            ));
        }

        let mut cats = vec![Span::styled(format!("{:<8}", "types"), theme::label())];
        for (i, c) in Category::ALL.iter().enumerate() {
            let on = self.filter.categories.contains(c);
            cats.push(Span::styled(
                format!("{}{}", i + 1, if on { ":" } else { " " }),
                theme::dim(),
            ));
            cats.push(Span::styled(c.label(), theme::category_toggle(*c, on)));
            cats.push(Span::raw(" "));
        }

        let lines = vec![
            Line::from(vec![
                Span::styled(format!("{:<8}", "roots"), theme::label()),
                Span::styled(roots, theme::plain()),
            ]),
            Line::from(status_line),
            Line::from(totals),
            Line::from(cats),
        ];

        f.render_widget(
            Paragraph::new(lines).block(theme::panel("venvpruner")),
            area,
        );
    }

    fn render_body(&mut self, f: &mut Frame, area: Rect) {
        // Wide terminals get the detail pane beside the table; narrow ones get
        // it underneath, so the path column stays readable at 80 columns.
        let (table_area, detail_area) = if area.width >= 112 {
            let parts =
                Layout::horizontal([Constraint::Min(58), Constraint::Length(46)]).split(area);
            (parts[0], parts[1])
        } else {
            let detail_h = (area.height / 2)
                .clamp(6, 14)
                .min(area.height.saturating_sub(4));
            let parts =
                Layout::vertical([Constraint::Min(3), Constraint::Length(detail_h)]).split(area);
            (parts[0], parts[1])
        };

        self.page = table::page_size(table_area);

        if self.view.is_empty() {
            let msg = if self.artifacts.is_empty() && self.scanning {
                "scanning…"
            } else if self.artifacts.is_empty() {
                "nothing prunable found under the given roots"
            } else {
                "no artifact matches the current filters — press / or 1-8 to widen them"
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(msg, theme::dim())))
                    .block(theme::panel("artifacts"))
                    .wrap(Wrap { trim: true }),
                table_area,
            );
        } else {
            let title = format!(
                "artifacts {}/{}  ·  sort {}{}",
                self.view.len(),
                self.artifacts.len(),
                self.filter.sort.label(),
                if self.filter.descending {
                    " ↓"
                } else {
                    " ↑"
                }
            );
            let (artifacts, view, state) = (&self.artifacts, &self.view, &mut self.table);
            table::render(f, table_area, artifacts, view, state, self.now, &title);
        }

        detail::render(f, detail_area, self.selected_artifact(), self.now);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

        let top = if self.mode == Mode::Search {
            Line::from(vec![
                Span::styled("search  /", theme::accent()),
                Span::styled(self.filter.query.clone(), theme::bold()),
                Span::styled("_", theme::dim()),
                Span::styled("   Enter accept · Esc clear", theme::dim()),
            ])
        } else if let Some(msg) = &self.flash {
            Line::from(Span::styled(msg.clone(), theme::warn()))
        } else {
            Line::from(Span::styled(
                "j/k move  PgUp/PgDn page  g/G ends  space mark  a all  n none  \
                 / search  s sort  r reverse  1-8 types  d delete  ? help  q quit",
                theme::dim(),
            ))
        };

        f.render_widget(Paragraph::new(top).wrap(Wrap { trim: true }), parts[0]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(BTRFS_NOTE, theme::dim()))),
            parts[1],
        );
    }

    fn render_help(&self, f: &mut Frame, area: Rect) {
        let entries: [(&str, &str); 15] = [
            ("↑ ↓ / k j", "move the cursor"),
            ("PgUp PgDn", "move a page"),
            ("g / G, Home / End", "jump to first / last row"),
            ("space", "toggle the mark on the selected artifact"),
            ("a", "mark every artifact matching the current filter"),
            ("n", "clear all marks"),
            ("/", "search paths; Enter accepts, Esc cancels and clears"),
            ("s", "cycle the sort key"),
            ("r", "reverse the sort order"),
            ("1 … 8", "toggle a category on or off"),
            ("d", "delete marked artifacts (asks you to type DELETE)"),
            ("?", "show or hide this help"),
            ("q / Esc", "quit"),
            ("Ctrl-C", "quit immediately"),
            ("", ""),
        ];

        // Entries, the badge legend, the wrapped reclaim notes and the borders.
        let h = (entries.len() + 16) as u16;
        let r = confirm::open(f, area, 74, h);
        let block = theme::modal("keys", theme::accent());
        let inner = block.inner(r);
        f.render_widget(block, r);

        let mut lines: Vec<Line<'static>> = Vec::new();
        for (k, d) in entries {
            if k.is_empty() {
                lines.push(Line::default());
                continue;
            }
            lines.push(Line::from(vec![
                Span::styled(format!("  {k:<19}"), theme::accent()),
                Span::styled(d, theme::plain()),
            ]));
        }
        lines.push(Line::from(Span::styled(
            "  badges: DEAD unusable · SAFE stale and reinstallable · REVIEW look first · ACTIVE recent",
            theme::dim(),
        )));
        lines.push(Line::default());
        for note in RECLAIM_HELP {
            lines.push(Line::from(Span::styled(format!("  {note}"), theme::dim())));
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            "  press any key to close",
            theme::dim(),
        )));

        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_deleting(&self, f: &mut Frame, area: Rect) {
        let r = confirm::open(f, area, 64, 8);
        let block = theme::modal("deleting", theme::danger_bold());
        let inner = block.inner(r);
        f.render_widget(block, r);

        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

        let ratio = if self.del_total == 0 {
            0.0
        } else {
            (self.del_done as f64 / self.del_total as f64).clamp(0.0, 1.0)
        };

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(SPINNER[self.spinner], theme::accent()),
                Span::styled(
                    format!(" {} of {} · freed ", self.del_done, self.del_total),
                    theme::plain(),
                ),
                Span::styled(theme::bytes(self.del_freed), theme::reclaim()),
            ])),
            rows[0],
        );
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(theme::reclaim())
                .label(format!("{:.0}%", ratio * 100.0)),
            rows[1],
        );
        if !self.del_failures.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("{} failed so far", self.del_failures.len()),
                    theme::danger(),
                ))),
                rows[2],
            );
        }
    }

    fn render_results(&self, f: &mut Frame, area: Rect) {
        let shown = self.del_failures.len().min(8);
        let h = (9 + shown) as u16;
        let w = 76u16;
        let r = confirm::open(f, area, w, h);
        let ok = self.del_failures.is_empty();
        let style = if ok {
            theme::accent()
        } else {
            theme::danger_bold()
        };
        let block = theme::modal("deletion results", style);
        let inner = block.inner(r);
        f.render_widget(block, r);

        let succeeded = self.del_done.saturating_sub(self.del_failures.len());
        let mut lines: Vec<Line<'static>> = vec![
            Line::default(),
            Line::from(vec![
                Span::styled("Removed ", theme::plain()),
                Span::styled(format!("{succeeded}"), theme::bold()),
                Span::styled(format!(" of {} · freed ", self.del_total), theme::plain()),
                Span::styled(theme::bytes(self.del_freed), theme::reclaim()),
            ]),
            Line::default(),
        ];

        if ok {
            lines.push(Line::from(Span::styled("No failures.", theme::plain())));
        } else {
            lines.push(Line::from(Span::styled(
                format!("{} failed and were left in place:", self.del_failures.len()),
                theme::danger(),
            )));
            let path_w = inner.width.saturating_sub(4) as usize;
            for (p, e) in self.del_failures.iter().take(shown) {
                lines.push(Line::from(vec![
                    Span::styled("  ", theme::plain()),
                    Span::styled(
                        table::truncate_left(&p.display().to_string(), path_w),
                        theme::plain(),
                    ),
                ]));
                lines.push(Line::from(Span::styled(format!("    {e}"), theme::warn())));
            }
            if self.del_failures.len() > shown {
                lines.push(Line::from(Span::styled(
                    format!("  … and {} more", self.del_failures.len() - shown),
                    theme::dim(),
                )));
            }
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "press Enter to continue",
            theme::dim(),
        )));

        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Category, Sizes};
    use crossterm::event::KeyEventState;
    use std::collections::HashSet;

    fn cfg() -> ScanConfig {
        ScanConfig {
            roots: vec![PathBuf::from("/tmp")],
            one_file_system: false,
            categories: Category::ALL.into_iter().collect::<HashSet<_>>(),
            stale_after: Duration::from_secs(60 * 60 * 24 * 30),
            jobs: 4,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn app_with(n: u64) -> App {
        let mut app = App::new(&cfg());
        for i in 0..n {
            let a = Artifact::new(
                i,
                PathBuf::from(format!("/tmp/p{i}/.venv")),
                Category::PyVenv,
                Some(PathBuf::from(format!("/tmp/p{i}"))),
            );
            app.apply_scan(ScanEvent::Found(Box::new(a)));
        }
        app.refresh_view();
        app
    }

    #[test]
    fn selection_follows_the_artifact_not_the_row() {
        let mut app = app_with(3);
        // Select the artifact currently in the middle.
        app.select_row(1);
        let picked = app.selected.expect("something selected");

        // A size arrives for the last artifact and it jumps to the top.
        app.apply_scan(ScanEvent::Sized(
            2,
            Sizes {
                apparent: 9_000_000,
                reclaim: 9_000_000,
                ..Default::default()
            },
        ));
        app.refresh_view();

        assert_eq!(
            app.selected,
            Some(picked),
            "cursor drifted onto another row"
        );
        let row = app.table.selected().unwrap();
        assert_eq!(app.artifacts[app.view[row]].id, picked);
    }

    #[test]
    fn marks_survive_refiltering_and_drive_the_totals() {
        let mut app = app_with(3);
        app.apply_scan(ScanEvent::Sized(
            0,
            Sizes {
                apparent: 100,
                reclaim: 70,
                shared: 30,
                ..Default::default()
            },
        ));
        app.refresh_view();
        app.select_row(
            app.view
                .iter()
                .position(|&i| app.artifacts[i].id == 0)
                .unwrap(),
        );
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(app.marked_count(), 1);
        assert_eq!(app.marked_reclaim(), 70);

        app.on_key(key(KeyCode::Char('/')));
        for c in "nomatch".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.refresh_view();
        assert!(app.view.is_empty());
        assert_eq!(app.marked_count(), 1, "filtering must not drop marks");
    }

    #[test]
    fn mark_all_and_clear_all() {
        let mut app = app_with(4);
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.marked_count(), 4);
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.marked_count(), 0);
    }

    #[test]
    fn delete_key_without_marks_does_not_open_the_modal() {
        let mut app = app_with(2);
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.flash.is_some());

        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Confirm);
    }

    #[test]
    fn escape_in_search_clears_the_query_but_esc_in_normal_quits() {
        let mut app = app_with(2);
        app.on_key(key(KeyCode::Char('/')));
        app.on_key(key(KeyCode::Char('p')));
        assert_eq!(app.filter.query, "p");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.filter.query.is_empty());
        assert!(!app.quit);

        app.on_key(key(KeyCode::Esc));
        assert!(app.quit);
    }

    #[test]
    fn number_keys_toggle_categories() {
        let mut app = app_with(1);
        assert!(app.filter.categories.contains(&Category::ALL[0]));
        app.on_key(key(KeyCode::Char('1')));
        assert!(!app.filter.categories.contains(&Category::ALL[0]));
        app.on_key(key(KeyCode::Char('1')));
        assert!(app.filter.categories.contains(&Category::ALL[0]));
    }

    #[test]
    fn navigation_stays_in_bounds() {
        let mut app = app_with(3);
        app.select_row(0);
        app.move_by(-50);
        assert_eq!(app.table.selected(), Some(0));
        app.move_by(50);
        assert_eq!(app.table.selected(), Some(2));

        let mut empty = App::new(&cfg());
        empty.refresh_view();
        empty.move_by(1);
        assert_eq!(empty.table.selected(), None);
        assert!(empty.selected.is_none());
    }

    #[test]
    fn help_overlay_swallows_the_next_key() {
        let mut app = app_with(2);
        app.on_key(key(KeyCode::Char('?')));
        assert!(app.show_help);
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.show_help);
        assert!(!app.quit, "the dismissing key must not also act");
    }

    #[test]
    fn sized_events_land_on_the_right_artifact() {
        let mut app = app_with(5);
        app.apply_scan(ScanEvent::Sized(
            3,
            Sizes {
                apparent: 42,
                reclaim: 42,
                ..Default::default()
            },
        ));
        let i = app.by_id[&3];
        assert!(app.artifacts[i].sized);
        assert_eq!(app.artifacts[i].sizes.apparent, 42);
        assert_eq!(app.total_apparent(), 42);
        assert!(app.artifacts.iter().filter(|a| a.sized).count() == 1);
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        let mut app = app_with(1);
        app.mode = Mode::Confirm;
        app.on_key(ctrl_c());
        assert!(app.quit);
    }

    fn ctrl_c() -> KeyEvent {
        let mut ev = key(KeyCode::Char('c'));
        ev.modifiers = KeyModifiers::CONTROL;
        ev
    }

    fn done(freed: u64) -> crate::delete::Outcome {
        crate::delete::Outcome {
            freed,
            complete: true,
            error: None,
        }
    }

    /// Run a deletion of `ids` to completion through the real `drain_deletes`,
    /// exactly as the worker thread would report it, and stop before any redraw.
    fn run_delete(app: &mut App, ids: &[ArtifactId]) {
        let (tx, rx) = crossbeam_channel::unbounded();
        for &id in ids {
            tx.send(DeleteMsg::Item {
                id,
                path: PathBuf::from(format!("/tmp/p{id}/.venv")),
                result: Ok(done(10)),
            })
            .unwrap();
        }
        tx.send(DeleteMsg::Finished).unwrap();
        app.del_total = ids.len();
        app.delete_rx = Some(rx);
        app.mode = Mode::Deleting;
        app.drain_deletes();
    }

    #[test]
    fn keys_after_a_delete_do_not_index_through_the_stale_view() {
        // `drain_deletes` shrinks `artifacts` but `view` still holds
        // pre-deletion indices until the next frame, and a whole batch of keys
        // is dispatched before that frame is due.
        let mut app = app_with(6);
        run_delete(&mut app, &[0, 1, 2, 3, 4]);
        assert_eq!(app.artifacts.len(), 1);
        assert_eq!(app.view, vec![0, 1, 2, 3, 4, 5], "view is stale by design");
        assert_eq!(app.mode, Mode::Results);

        // Same input batch, no redraw in between: Results → Normal, then mark.
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.marked_count(), 1);
        assert_eq!(app.artifacts[0].id, 5);

        app.on_key(key(KeyCode::Char('n')));
        app.select_row(0);
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(app.marked_count(), 1);
    }

    #[test]
    fn mark_all_uses_the_filter_in_force_not_the_last_drawn_one() {
        let mut app = app_with(10);
        app.on_key(key(KeyCode::Char('/')));
        for c in "zzzz".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        // No frame has been drawn since the query changed.
        app.on_key(key(KeyCode::Char('a')));

        app.refresh_view();
        assert!(app.view.is_empty());
        assert_eq!(
            app.marked_count(),
            0,
            "`a` must not arm artifacts the filter excludes"
        );
    }

    #[test]
    fn ctrl_c_does_not_quit_while_a_deletion_is_running() {
        // Quitting terminates the worker wherever it is, possibly halfway
        // through a `remove_dir_all`, with no report of what was in flight.
        let mut app = app_with(2);
        app.mode = Mode::Deleting;
        app.on_key(ctrl_c());
        assert!(!app.quit);
        assert!(app.flash.is_some());

        app.mode = Mode::Normal;
        app.on_key(ctrl_c());
        assert!(app.quit);
    }

    #[test]
    fn marked_but_unmeasured_artifacts_are_counted_as_such() {
        let mut app = app_with(3);
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.marked_count(), 3);
        assert_eq!(app.marked_unsized(), 3);
        assert_eq!(app.marked_reclaim(), 0, "nothing has been sized yet");

        app.apply_scan(ScanEvent::Sized(
            0,
            Sizes {
                apparent: 500,
                reclaim: 500,
                ..Default::default()
            },
        ));
        assert_eq!(app.marked_unsized(), 2, "a 0 B total must not look final");
    }

    #[test]
    fn marks_hidden_by_the_filter_are_counted_for_the_confirmation() {
        let mut app = app_with(4);
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.marked_hidden(), 0);

        app.on_key(key(KeyCode::Char('/')));
        for c in "p0/".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        app.refresh_view();
        assert_eq!(app.view.len(), 1);
        assert_eq!(app.marked_count(), 4);
        assert_eq!(
            app.marked_hidden(),
            3,
            "marks off screen still get deleted and must be declared"
        );
    }

    #[test]
    fn a_partly_deleted_artifact_stays_listed_with_the_bytes_that_went_removed() {
        let mut app = app_with(2);
        app.apply_scan(ScanEvent::Sized(
            0,
            Sizes {
                apparent: 1000,
                reclaim: 900,
                ..Default::default()
            },
        ));

        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(DeleteMsg::Item {
            id: 0,
            path: PathBuf::from("/tmp/p0/.venv"),
            result: Ok(crate::delete::Outcome {
                freed: 400,
                complete: false,
                error: Some("removed 1 of 2 __pycache__ directories: nope".into()),
            }),
        })
        .unwrap();
        tx.send(DeleteMsg::Finished).unwrap();
        app.del_total = 1;
        app.delete_rx = Some(rx);
        app.drain_deletes();

        assert_eq!(app.del_freed, 400, "bytes that went must be credited");
        assert_eq!(app.del_failures.len(), 1, "the remainder must be reported");
        assert_eq!(app.artifacts.len(), 2, "the leftovers stay visible");
        let a = &app.artifacts[app.by_id[&0]];
        assert_eq!(
            a.sizes.reclaim, 500,
            "the row must not re-advertise the 400"
        );
        assert_eq!(a.sizes.apparent, 600);
        assert!(!a.marked);
    }

    /// Render at a spread of sizes and return the drawn lines.
    fn draw_at(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_category_toggle_row_is_actually_rendered() {
        // The footer and the help both tell the user to press 1-8; the header
        // is the only readout of what those keys did.
        let mut app = app_with(3);
        for (w, h) in [(80, 30), (120, 40), (200, 50)] {
            let lines = draw_at(&mut app, w, h);
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("types") && l.contains("venv")),
                "no category toggle row at {w}x{h}: {lines:#?}"
            );
        }
    }

    #[test]
    fn the_reclaim_note_is_not_cut_off_at_eighty_columns() {
        let mut app = app_with(3);
        for (w, h) in [(80, 30), (120, 40), (200, 50)] {
            let lines = draw_at(&mut app, w, h);
            assert!(
                lines.iter().any(|l| l.contains(BTRFS_NOTE)),
                "the note is truncated at {w}x{h}"
            );
        }
    }

    #[test]
    fn every_mode_renders_at_a_spread_of_sizes() {
        let mut app = app_with(4);
        app.on_key(key(KeyCode::Char('a')));
        for mode in [
            Mode::Normal,
            Mode::Search,
            Mode::Confirm,
            Mode::Deleting,
            Mode::Results,
        ] {
            app.mode = mode;
            for (w, h) in [(1, 1), (20, 3), (80, 24), (112, 30), (200, 50)] {
                draw_at(&mut app, w, h);
            }
        }
    }
}
