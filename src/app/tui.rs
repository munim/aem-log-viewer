use std::io::{stdout, Stdout};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::Terminal;

use super::cli::Request;
use super::live::{LiveSession, Snapshot};
use super::Error;

const MIN_FRAME: Duration = Duration::from_millis(80);
const POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum View {
    Volume,
}

const VIEWS: [View; 1] = [View::Volume];

impl View {
    fn label(self) -> &'static str {
        match self {
            Self::Volume => "Volume",
        }
    }

    fn next(self) -> Self {
        let idx = VIEWS.iter().position(|view| *view == self).unwrap_or(0);
        VIEWS[(idx + 1) % VIEWS.len()]
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    restored: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self, Error> {
        enable_raw_mode().map_err(io_err)?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen).map_err(|err| {
            let _ = disable_raw_mode();
            io_err(err)
        })?;
        let backend = CrosstermBackend::new(out);
        let terminal = Terminal::new(backend).map_err(|err| {
            let _ = restore_terminal();
            io_err(err)
        })?;
        Ok(Self {
            terminal,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<(), Error> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        restore_terminal()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn restore_terminal() -> Result<(), Error> {
    let mut out = stdout();
    execute!(out, LeaveAlternateScreen).map_err(io_err)?;
    disable_raw_mode().map_err(io_err)
}

fn io_err(err: impl ToString) -> Error {
    Error::Io(err.to_string())
}

#[derive(Debug)]
struct App {
    view: View,
    selected: Option<u64>,
    help: bool,
    snapshot: Snapshot,
}

impl App {
    fn new(snapshot: Snapshot) -> Self {
        let mut app = Self {
            view: View::Volume,
            selected: None,
            help: false,
            snapshot,
        };
        app.sync_selection();
        app
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
        self.sync_selection();
    }

    fn rows(&self) -> Vec<&super::live::GroupAggregate> {
        match self.view {
            View::Volume => self.snapshot.volume_rows(),
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.rows().iter().position(|group| group.id == id)
    }

    fn sync_selection(&mut self) {
        let rows = self.rows();
        if rows.is_empty() {
            self.selected = None;
            return;
        }
        if let Some(id) = self.selected {
            if rows.iter().any(|group| group.id == id) {
                return;
            }
        }
        self.selected = Some(rows[0].id);
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|id| rows.iter().position(|group| group.id == id))
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.selected = Some(rows[next].id);
    }

    fn handle(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        if self.help {
            return match key.code {
                KeyCode::Char('q') => true,
                KeyCode::Char('?') | KeyCode::Esc => {
                    self.help = false;
                    false
                }
                _ => false,
            };
        }
        match key.code {
            KeyCode::Char('q') => true,
            KeyCode::Char('?') => {
                self.help = true;
                false
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                false
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                false
            }
            KeyCode::Tab => {
                self.view = self.view.next();
                self.sync_selection();
                false
            }
            KeyCode::Enter => false,
            _ => false,
        }
    }

    fn footer(&self) -> String {
        if self.help {
            return "[q]uit  [?]close  [Esc]close".into();
        }
        match self.view {
            View::Volume => "[q]uit  [?]help  [j/k]move  [Tab]view".into(),
        }
    }
}

pub(super) fn run(request: &Request) -> Result<(), Error> {
    let session = LiveSession::start(request.clone())?;
    let result = run_ui(&session);
    session.request_stop();
    match session.join() {
        Ok(()) => result,
        Err(Error::UnexpectedEnd(_)) if result.is_ok() => result,
        Err(err) => {
            let _ = result;
            Err(err)
        }
    }
}

fn run_ui(session: &LiveSession) -> Result<(), Error> {
    let mut guard = TerminalGuard::enter()?;
    let mut app = App::new(session.snapshot());
    let mut last_gen = app.snapshot.generation;
    let mut last_draw = Instant::now()
        .checked_sub(MIN_FRAME)
        .unwrap_or_else(Instant::now);
    let mut dirty = true;
    loop {
        let snap = session.snapshot();
        if snap.generation != last_gen {
            last_gen = snap.generation;
            app.apply_snapshot(snap);
            dirty = true;
        }
        if dirty && last_draw.elapsed() >= MIN_FRAME {
            guard
                .terminal
                .draw(|frame| render(frame, &app))
                .map_err(io_err)?;
            last_draw = Instant::now();
            dirty = false;
        }
        if session.finished() || last_draw.elapsed() >= Duration::from_secs(1) {
            dirty = true;
        }
        let wait = if dirty {
            MIN_FRAME.saturating_sub(last_draw.elapsed())
        } else {
            POLL
        };
        if event::poll(wait).map_err(io_err)? {
            match event::read().map_err(io_err)? {
                Event::Key(key) => {
                    if app.handle(key) {
                        break;
                    }
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
    }
    guard.restore()
}

fn render(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(header_text(&app.snapshot)), header);
    render_table(frame, body, app);
    frame.render_widget(Paragraph::new(app.footer()), footer);
    if app.help {
        render_help(frame, area);
    }
}

fn header_text(snapshot: &Snapshot) -> String {
    format!(
        "{}  {}  {}  |  {}  |  up {}  |  events {}  |  groups {}  |  overflow {}  |  diag {}",
        snapshot.program_id,
        snapshot.environment_id,
        snapshot.service,
        snapshot.process.label(),
        format_uptime(snapshot.started_at.elapsed()),
        snapshot.selected_events,
        snapshot.group_count(),
        snapshot.overflow.label(),
        snapshot.diagnostics,
    )
}

fn format_uptime(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn render_table(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let plan = column_plan(area.width);
    let header = Row::new(plan.iter().map(|col| Cell::from(col.0)))
        .style(Style::new().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app
        .rows()
        .into_iter()
        .map(|group| {
            Row::new(
                plan.iter()
                    .map(|col| Cell::from(cell_value(col.0, group)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let widths: Vec<Constraint> = plan.iter().map(|col| col.1).collect();
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.view.label()),
        )
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("> ");
    let mut state = TableState::default();
    state.select(app.selected_index());
    frame.render_stateful_widget(table, area, &mut state);
}

fn column_plan(width: u16) -> Vec<(&'static str, Constraint)> {
    let mut cols = vec![
        ("RATE", Constraint::Length(6)),
        ("COUNT", Constraint::Length(6)),
        ("LEVEL", Constraint::Length(5)),
    ];
    if width >= 80 {
        cols.push(("LOGGER", Constraint::Length(18)));
    }
    if width >= 100 {
        cols.push(("EXCEPT", Constraint::Length(16)));
    }
    cols.push(("TEMPLATE", Constraint::Min(12)));
    if width >= 120 {
        cols.push(("NODES", Constraint::Length(14)));
    }
    cols.push(("LAST", Constraint::Length(8)));
    cols
}

fn cell_value(column: &str, group: &super::live::GroupAggregate) -> String {
    match column {
        "RATE" => "-".into(),
        "COUNT" => group.count.to_string(),
        "LEVEL" => group.level.as_str().to_owned(),
        "LOGGER" => group.logger.clone(),
        "EXCEPT" => group
            .terminal_exception
            .clone()
            .unwrap_or_else(|| "-".into()),
        "TEMPLATE" => group.template.join(" "),
        "NODES" => group.nodes.join(","),
        "LAST" => group.last_seen.format("%H:%M:%S").to_string(),
        _ => String::new(),
    }
}

fn render_help(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let width = area.width.clamp(24, 56);
    let height = area.height.saturating_sub(2).clamp(6, 10);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);
    let text = vec![
        Line::from(Span::styled(
            "keys",
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::from("j/k, ↓/↑  move selection"),
        Line::from("Tab       next available view"),
        Line::from("Enter     detail unavailable"),
        Line::from("?         toggle help"),
        Line::from("q         quit"),
    ];
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("help")),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::cli::Level;
    use crate::app::live::{GroupAggregate, OverflowState, ProcessState};
    use chrono::{TimeZone, Utc};
    use ratatui::backend::TestBackend;

    fn instant() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 5).unwrap()
    }

    fn group(id: u64, count: u64, message: &str) -> GroupAggregate {
        GroupAggregate {
            id,
            count,
            first_seen: instant(),
            last_seen: instant(),
            template: message.split_whitespace().map(str::to_owned).collect(),
            nodes: vec!["author-0".into()],
            sample: message.into(),
            muted: false,
            level: Level::Error,
            logger: "com.example.Foo".into(),
            terminal_exception: None,
            is_overflow: false,
            capacity_global: 0,
            capacity_template_bucket: 0,
        }
    }

    fn snapshot(groups: Vec<GroupAggregate>) -> Snapshot {
        Snapshot {
            program_id: "p1".into(),
            environment_id: "e1".into(),
            service: "author".into(),
            process: ProcessState::AioRunning,
            started_at: Instant::now(),
            selected_events: groups.iter().map(|group| group.count).sum(),
            diagnostics: 2,
            overflow: OverflowState::None,
            groups,
            generation: 1,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_owned())
            .collect::<String>()
    }

    #[test]
    fn volume_sorts_by_count_then_group_id() {
        let snap = snapshot(vec![
            group(2, 4, "older"),
            group(1, 4, "tie"),
            group(3, 9, "hot"),
        ]);
        let ids: Vec<u64> = snap.volume_rows().into_iter().map(|g| g.id).collect();
        assert_eq!(ids, [3, 1, 2]);
    }

    #[test]
    fn selection_tracks_group_id_across_reorder() {
        let mut app = App::new(snapshot(vec![group(1, 5, "a"), group(2, 1, "b")]));
        assert_eq!(app.selected, Some(1));
        app.move_selection(1);
        assert_eq!(app.selected, Some(2));
        app.apply_snapshot(snapshot(vec![group(2, 8, "b"), group(1, 5, "a")]));
        assert_eq!(app.selected, Some(2));
        assert_eq!(app.selected_index(), Some(0));
    }

    #[test]
    fn arrows_and_jk_navigate() {
        let mut app = App::new(snapshot(vec![
            group(1, 3, "a"),
            group(2, 2, "b"),
            group(3, 1, "c"),
        ]));
        assert_eq!(app.selected, Some(1));
        app.handle(press(KeyCode::Char('j')));
        assert_eq!(app.selected, Some(2));
        app.handle(press(KeyCode::Down));
        assert_eq!(app.selected, Some(3));
        app.handle(press(KeyCode::Char('k')));
        assert_eq!(app.selected, Some(2));
        app.handle(press(KeyCode::Up));
        assert_eq!(app.selected, Some(1));
    }

    #[test]
    fn tab_stays_on_only_available_view_and_enter_is_inert() {
        let mut app = App::new(snapshot(vec![group(1, 1, "a")]));
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.view, View::Volume);
        let before = app.selected;
        app.handle(press(KeyCode::Enter));
        assert_eq!(app.selected, before);
        assert!(!app.help);
    }

    #[test]
    fn footer_lists_only_current_actions() {
        let mut app = App::new(snapshot(vec![]));
        assert_eq!(app.footer(), "[q]uit  [?]help  [j/k]move  [Tab]view");
        assert!(!app.footer().contains("Enter"));
        assert!(!app.footer().contains("Muted"));
        app.handle(press(KeyCode::Char('?')));
        assert!(app.help);
        assert_eq!(app.footer(), "[q]uit  [?]close  [Esc]close");
        app.handle(press(KeyCode::Esc));
        assert!(!app.help);
    }

    #[test]
    fn header_uses_observable_process_states() {
        for state in [
            ProcessState::Starting,
            ProcessState::AioRunning,
            ProcessState::Ended,
        ] {
            let label = state.label();
            assert_ne!(label, "Connected");
            assert!(!label.contains("Connected"));
        }
        assert_eq!(ProcessState::Starting.label(), "Starting");
        assert_eq!(
            ProcessState::AioRunning.label(),
            "AIO running / awaiting logs"
        );
        assert_eq!(ProcessState::Ended.label(), "Ended");
        let text = header_text(&snapshot(vec![group(1, 4, "boom")]));
        assert!(text.contains("p1"));
        assert!(text.contains("e1"));
        assert!(text.contains("author"));
        assert!(text.contains("events 4"));
        assert!(text.contains("groups 1"));
        assert!(text.contains("overflow none"));
        assert!(text.contains("diag 2"));
        assert!(!text.contains("Connected"));
    }

    #[test]
    fn q_requests_shutdown() {
        let mut app = App::new(snapshot(vec![]));
        assert!(app.handle(press(KeyCode::Char('q'))));
    }

    #[test]
    fn widget_renders_volume_rows_and_help() {
        let mut app = App::new(snapshot(vec![
            group(1, 4, "Failed to start bundle"),
            group(2, 1, "other error"),
        ]));
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("Volume"), "{text}");
        assert!(text.contains("Failed to start bundle"), "{text}");
        assert!(text.contains("other error"), "{text}");
        assert!(text.contains("COUNT"), "{text}");
        assert!(text.contains("AIO running / awaiting logs"), "{text}");
        assert!(text.contains("[q]uit"), "{text}");
        app.handle(press(KeyCode::Char('?')));
        terminal.draw(|frame| render(frame, &app)).expect("help");
        let help = buffer_text(&terminal);
        assert!(help.contains("toggle help"), "{help}");
        assert!(help.contains("quit"), "{help}");
    }
}
