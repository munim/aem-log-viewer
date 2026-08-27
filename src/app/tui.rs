use std::io::{stdout, Stdout};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Terminal;
use regex::RegexBuilder;

use super::cli::{Level, Request};
use super::live::{GroupAggregate, LiveSession, OverflowState, ProcessState, Snapshot};
use super::rate::{SortColumn, View};
use super::Error;

const MIN_FRAME: Duration = Duration::from_millis(80);
const POLL: Duration = Duration::from_millis(50);
const MAX_SEARCH_BYTES: usize = 256;
const MIN_COLS: u16 = 80;
const MIN_ROWS: u16 = 24;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Volume,
    Detail,
}

#[derive(Clone, Debug)]
enum SearchMode {
    Inactive,
    Editing {
        buffer: String,
    },
    Applied {
        query: String,
        regex: Option<regex::Regex>,
    },
    Invalid {
        query: String,
    },
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
    sort: SortColumn,
    screen: Screen,
    selected: Option<u64>,
    last_index: usize,
    help: bool,
    snapshot: Snapshot,
    scroll_y: u16,
    scroll_x: u16,
    search: SearchMode,
    pending_mutes: Vec<u64>,
    desired_mute: Option<(u64, bool)>,
    color: bool,
}

impl App {
    fn new(snapshot: Snapshot) -> Self {
        let mut app = Self {
            view: View::Volume,
            sort: View::Volume.default_sort(),
            screen: Screen::Volume,
            selected: None,
            last_index: 0,
            help: false,
            snapshot,
            scroll_y: 0,
            scroll_x: 0,
            search: SearchMode::Inactive,
            pending_mutes: Vec::new(),
            desired_mute: None,
            color: colors_enabled(),
        };
        app.sync_selection();
        app
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
        if let Some((id, muted)) = self.desired_mute {
            if let Some(group) = self.snapshot.groups.iter_mut().find(|group| group.id == id) {
                if group.muted == muted {
                    self.desired_mute = None;
                } else {
                    group.muted = muted;
                }
            } else {
                self.desired_mute = None;
            }
        }
        self.sync_selection();
        if self.screen == Screen::Detail && self.selected_group().is_none() {
            self.close_detail();
        }
        self.clamp_scroll();
    }

    fn rows(&self) -> Vec<&GroupAggregate> {
        self.snapshot.view_rows(self.view, self.sort)
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.rows().iter().position(|group| group.id == id)
    }

    fn selected_group(&self) -> Option<&GroupAggregate> {
        let id = self.selected?;
        self.snapshot.groups.iter().find(|group| group.id == id)
    }

    fn sync_selection(&mut self) {
        let rows = self.rows();
        if rows.is_empty() {
            self.selected = None;
            return;
        }
        if let Some(id) = self.selected {
            if let Some(index) = rows.iter().position(|group| group.id == id) {
                self.last_index = index;
                return;
            }
            let index = self.last_index.min(rows.len() - 1);
            self.selected = Some(rows[index].id);
            self.last_index = index;
            return;
        }
        self.selected = Some(rows[0].id);
        self.last_index = 0;
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
        self.last_index = next;
    }

    fn cycle_view(&mut self) {
        self.view = self.view.next();
        self.sort = self.view.default_sort();
        self.sync_selection();
    }

    fn cycle_sort(&mut self) {
        self.sort = self.view.next_sort(self.sort);
        self.sync_selection();
    }

    fn toggle_mute_selected(&mut self) {
        let Some(id) = self.selected else {
            return;
        };
        let Some(group) = self.snapshot.groups.iter_mut().find(|group| group.id == id) else {
            return;
        };
        group.muted = !group.muted;
        self.pending_mutes.push(id);
        self.desired_mute = Some((id, group.muted));
        if self.screen != Screen::Detail {
            self.sync_selection();
        }
    }

    fn open_detail(&mut self) {
        if self.selected_group().is_none() {
            return;
        }
        self.screen = Screen::Detail;
        self.scroll_y = 0;
        self.scroll_x = 0;
        self.search = SearchMode::Inactive;
        self.help = false;
    }

    fn close_detail(&mut self) {
        self.screen = Screen::Volume;
        self.scroll_y = 0;
        self.scroll_x = 0;
        self.search = SearchMode::Inactive;
        self.sync_selection();
    }

    fn editing_search(&self) -> bool {
        matches!(self.search, SearchMode::Editing { .. })
    }

    fn clamp_scroll(&mut self) {
        let (max_y, max_x) = self.scroll_max();
        self.scroll_y = self.scroll_y.min(max_y);
        self.scroll_x = self.scroll_x.min(max_x);
    }

    fn scroll_max(&self) -> (u16, u16) {
        let Some(group) = self.selected_group() else {
            return (0, 0);
        };
        let lines = detail_lines(group);
        let max_y = lines.len().saturating_sub(1) as u16;
        let max_x = lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0)
            .saturating_sub(1) as u16;
        (max_y, max_x)
    }

    fn scroll_vertical(&mut self, delta: i16) {
        let (max_y, _) = self.scroll_max();
        let next = i32::from(self.scroll_y) + i32::from(delta);
        self.scroll_y = next.clamp(0, i32::from(max_y)) as u16;
    }

    fn scroll_horizontal(&mut self, delta: i16) {
        let (_, max_x) = self.scroll_max();
        let next = i32::from(self.scroll_x) + i32::from(delta);
        self.scroll_x = next.clamp(0, i32::from(max_x)) as u16;
    }

    fn begin_search(&mut self) {
        self.search = SearchMode::Editing {
            buffer: String::new(),
        };
    }

    fn push_search_char(&mut self, ch: char) {
        let SearchMode::Editing { buffer } = &mut self.search else {
            return;
        };
        if buffer.len() >= MAX_SEARCH_BYTES {
            return;
        }
        buffer.push(ch);
    }

    fn pop_search_char(&mut self) {
        let SearchMode::Editing { buffer } = &mut self.search else {
            return;
        };
        buffer.pop();
    }

    fn apply_search(&mut self) {
        let SearchMode::Editing { buffer } = &self.search else {
            return;
        };
        let query = buffer.clone();
        if let Some(pattern) = query.strip_prefix("re:") {
            if pattern.len() > MAX_SEARCH_BYTES {
                self.search = SearchMode::Invalid { query };
                return;
            }
            match RegexBuilder::new(pattern).case_insensitive(true).build() {
                Ok(compiled) => {
                    self.search = SearchMode::Applied {
                        query,
                        regex: Some(compiled),
                    };
                }
                Err(_) => {
                    self.search = SearchMode::Invalid { query };
                }
            }
            return;
        }
        self.search = SearchMode::Applied { query, regex: None };
    }

    fn cancel_search_edit(&mut self) {
        if matches!(self.search, SearchMode::Editing { .. }) {
            self.search = SearchMode::Inactive;
        }
    }

    fn search_matches(&self) -> Option<bool> {
        let group = self.selected_group()?;
        match &self.search {
            SearchMode::Applied { query, regex } => {
                if let Some(regex) = regex {
                    Some(regex.is_match(&metadata_haystack(group)))
                } else if query.is_empty() {
                    Some(true)
                } else {
                    Some(
                        metadata_haystack(group)
                            .to_lowercase()
                            .contains(&query.to_lowercase()),
                    )
                }
            }
            SearchMode::Invalid { .. } => Some(false),
            _ => None,
        }
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
        if self.editing_search() {
            return self.handle_search_edit(key);
        }
        if self.screen == Screen::Detail {
            return self.handle_detail(key);
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
                self.cycle_view();
                false
            }
            KeyCode::Char('s') => {
                self.cycle_sort();
                false
            }
            KeyCode::Char('m') => {
                self.toggle_mute_selected();
                false
            }
            KeyCode::Enter => {
                self.open_detail();
                false
            }
            _ => false,
        }
    }

    fn handle_search_edit(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => self.cancel_search_edit(),
            KeyCode::Enter => self.apply_search(),
            KeyCode::Backspace => self.pop_search_char(),
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.push_search_char(ch);
            }
            _ => {}
        }
        false
    }

    fn handle_detail(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => true,
            KeyCode::Char('?') => {
                self.help = true;
                false
            }
            KeyCode::Esc => {
                self.close_detail();
                false
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll_vertical(1);
                false
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll_vertical(-1);
                false
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.scroll_horizontal(-1);
                false
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.scroll_horizontal(1);
                false
            }
            KeyCode::Char('/') => {
                self.begin_search();
                false
            }
            KeyCode::Char('m') => {
                self.toggle_mute_selected();
                false
            }
            _ => false,
        }
    }

    fn footer(&self) -> String {
        if self.help {
            return "[q]uit  [?]close  [Esc]close".into();
        }
        if let SearchMode::Editing { buffer } = &self.search {
            return format!("/{buffer}");
        }
        match self.screen {
            Screen::Volume => format!(
                "[q]uit  [?]help  [j/k]move  [Tab]view  [s]ort  [m]ute  [Enter]detail  sort {}",
                self.sort.label()
            ),
            Screen::Detail => match &self.search {
                SearchMode::Applied { query, regex } if regex.is_some() => {
                    format!("[re:] {query}  [Esc]back")
                }
                SearchMode::Applied { query, .. } => format!("[/] {query}  [Esc]back"),
                SearchMode::Invalid { query } => format!("[re:] invalid {query}  [Esc]back"),
                _ => "[q]uit  [?]help  [j/k]scroll  [h/l]pan  [/]search  [Esc]back".into(),
            },
        }
    }

    fn too_small(&self, area: Rect) -> bool {
        area.width < MIN_COLS || area.height < MIN_ROWS
    }
}

fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

fn theme_color(enabled: bool, color: Color) -> Style {
    if enabled {
        Style::new().fg(color)
    } else {
        Style::new()
    }
}

fn role_error(enabled: bool) -> Style {
    theme_color(enabled, Color::Red)
}

fn role_warning(enabled: bool) -> Style {
    theme_color(enabled, Color::Yellow)
}

fn role_success(enabled: bool) -> Style {
    theme_color(enabled, Color::Green)
}

fn role_info(enabled: bool) -> Style {
    theme_color(enabled, Color::Cyan)
}

fn role_muted(enabled: bool) -> Style {
    theme_color(enabled, Color::DarkGray).add_modifier(Modifier::DIM)
}

fn role_emphasis() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn role_selection() -> Style {
    Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

fn level_style(level: Level, enabled: bool) -> Style {
    match level {
        Level::Fatal | Level::Error => role_error(enabled).add_modifier(Modifier::BOLD),
        Level::Warn => role_warning(enabled),
        Level::Info => role_info(enabled),
        Level::Debug | Level::Trace => role_muted(enabled),
    }
}

fn process_style(state: ProcessState, enabled: bool) -> Style {
    match state {
        ProcessState::Starting => role_warning(enabled),
        ProcessState::AioRunning => role_success(enabled),
        ProcessState::Ended => role_muted(enabled),
    }
}

fn overflow_style(state: OverflowState, enabled: bool) -> Style {
    match state {
        OverflowState::None => role_muted(enabled),
        OverflowState::Events(_) => role_error(enabled).add_modifier(Modifier::BOLD),
    }
}

fn process_cue(state: ProcessState) -> &'static str {
    match state {
        ProcessState::Starting => "...",
        ProcessState::AioRunning => "ok",
        ProcessState::Ended => "end",
    }
}

fn overflow_cue(state: OverflowState) -> &'static str {
    match state {
        OverflowState::None => "ok",
        OverflowState::Events(_) => "!",
    }
}

fn mute_cue(muted: bool) -> &'static str {
    if muted {
        "[M]"
    } else {
        "   "
    }
}

fn trend_cue(group: &GroupAggregate, snapshot: &Snapshot) -> &'static str {
    let snap = group.rate_snapshot();
    if snap.is_increasing(snapshot.now, &snapshot.rate_params) {
        "^"
    } else if snap.is_new(snapshot.now, &snapshot.rate_params) {
        "+"
    } else {
        " "
    }
}

fn level_cue(level: Level) -> &'static str {
    match level {
        Level::Fatal => "!!",
        Level::Error => "E ",
        Level::Warn => "W ",
        Level::Info => "I ",
        Level::Debug => "D ",
        Level::Trace => "T ",
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
        if !app.pending_mutes.is_empty() {
            for id in app.pending_mutes.drain(..) {
                session.toggle_mute(id);
            }
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
    if app.too_small(area) {
        render_resize_gate(frame, area, app.color);
        return;
    }
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(header_line(app)), header);
    match app.screen {
        Screen::Volume => render_table(frame, body, app),
        Screen::Detail => render_detail(frame, body, app),
    }
    frame.render_widget(Paragraph::new(app.footer()), footer);
    if app.help {
        render_help(frame, area, app);
    }
}

fn render_resize_gate(frame: &mut ratatui::Frame<'_>, area: Rect, color: bool) {
    let message = vec![
        Line::from(format!(
            "terminal too small ({}x{}).",
            area.width, area.height
        )),
        Line::from(format!("resize to {MIN_COLS}x{MIN_ROWS} or larger.")),
        Line::from("ingestion continues."),
    ];
    frame.render_widget(
        Paragraph::new(message).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("resize")
                .style(role_warning(color)),
        ),
        area,
    );
}

fn header_line(app: &App) -> Line<'static> {
    let snapshot = &app.snapshot;
    let color = app.color;
    Line::from(vec![
        Span::styled(snapshot.program_id.clone(), role_emphasis()),
        Span::raw("  "),
        Span::raw(snapshot.environment_id.clone()),
        Span::raw("  "),
        Span::raw(snapshot.service.clone()),
        Span::raw("  |  "),
        Span::styled(
            format!(
                "{} {}",
                process_cue(snapshot.process),
                snapshot.process.label()
            ),
            process_style(snapshot.process, color),
        ),
        Span::raw(format!(
            "  |  up {}  |  events {}  |  groups {}  |  overflow ",
            format_uptime(snapshot.started_at.elapsed()),
            snapshot.selected_events,
            snapshot.group_count(),
        )),
        Span::styled(
            format!(
                "{} {}",
                overflow_cue(snapshot.overflow),
                snapshot.overflow.label()
            ),
            overflow_style(snapshot.overflow, color),
        ),
        Span::raw(format!("  |  diag {}", snapshot.diagnostics)),
    ])
}

#[cfg(test)]
fn header_text(app: &App) -> String {
    header_line(app)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn view_tabs(active: View) -> String {
    View::ALL
        .iter()
        .map(|view| {
            if *view == active {
                format!("[{}]", view.label())
            } else {
                view.label().to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_uptime(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn render_table(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let plan = column_plan(area.width);
    let header = Row::new(
        plan.iter()
            .map(|col| Cell::from(sort_header(col.0, app.sort))),
    )
    .style(role_emphasis());
    let rows: Vec<Row> = app
        .rows()
        .into_iter()
        .map(|group| {
            let muted = group.muted;
            Row::new(
                plan.iter()
                    .map(|col| cell_widget(col.0, group, app))
                    .collect::<Vec<_>>(),
            )
            .style(if muted {
                role_muted(app.color)
            } else {
                Style::new()
            })
        })
        .collect();
    let widths: Vec<Constraint> = plan.iter().map(|col| col.1).collect();
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "{}  *{}",
            view_tabs(app.view),
            app.sort.label()
        )))
        .row_highlight_style(role_selection())
        .highlight_symbol("> ");
    let mut state = TableState::default();
    state.select(app.selected_index());
    frame.render_stateful_widget(table, area, &mut state);
}

fn column_plan(width: u16) -> Vec<(&'static str, Constraint)> {
    let mut cols = vec![
        ("MARK", Constraint::Length(5)),
        ("RATE", Constraint::Length(7)),
        ("COUNT", Constraint::Length(7)),
        ("LEVEL", Constraint::Length(8)),
    ];
    if width >= 90 {
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

fn sort_header(column: &str, sort: SortColumn) -> String {
    if column == "MARK" {
        return String::new();
    }
    if column == sort.label() {
        format!("*{column}")
    } else {
        column.to_owned()
    }
}

fn cell_value(column: &str, group: &super::live::GroupAggregate) -> String {
    match column {
        "RATE" => format!("{:.2}", group.fast),
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

fn cell_widget(column: &str, group: &GroupAggregate, app: &App) -> Cell<'static> {
    match column {
        "MARK" => {
            let mute = mute_cue(group.muted);
            let trend = trend_cue(group, &app.snapshot);
            Cell::from(Line::from(vec![
                Span::styled(
                    mute.to_owned(),
                    if group.muted {
                        role_muted(app.color)
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(
                    trend.to_owned(),
                    if trend == "^" {
                        role_warning(app.color)
                    } else if trend == "+" {
                        role_info(app.color)
                    } else {
                        Style::new()
                    },
                ),
            ]))
        }
        "LEVEL" => Cell::from(Line::from(vec![
            Span::styled(
                level_cue(group.level).to_owned(),
                level_style(group.level, app.color),
            ),
            Span::styled(
                group.level.as_str().to_owned(),
                level_style(group.level, app.color),
            ),
        ])),
        _ => Cell::from(cell_value(column, group)),
    }
}

fn render_detail(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let Some(group) = app.selected_group() else {
        frame.render_widget(
            Paragraph::new("group gone")
                .block(Block::default().borders(Borders::ALL).title("detail")),
            area,
        );
        return;
    };
    let lines: Vec<Line> = visible_detail_lines(group, app.scroll_y, app.scroll_x)
        .into_iter()
        .map(Line::from)
        .collect();
    let title = format!(
        "detail {}  {}{}  {}",
        group.id,
        mute_cue(group.muted).trim(),
        if group.muted { " " } else { "" },
        evidence_status(group)
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let search_line = search_status_line(app);
    let body = if search_line.is_empty() {
        inner
    } else {
        let [status, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
        frame.render_widget(Paragraph::new(search_line), status);
        body
    };
    frame.render_widget(Paragraph::new(lines), body);
}

fn visible_detail_lines(group: &GroupAggregate, scroll_y: u16, scroll_x: u16) -> Vec<String> {
    detail_lines(group)
        .into_iter()
        .skip(scroll_y as usize)
        .map(|line| line.chars().skip(scroll_x as usize).collect::<String>())
        .collect()
}

fn detail_lines(group: &GroupAggregate) -> Vec<String> {
    let mut lines = vec![
        format!("id {}", group.id),
        format!("count {}", group.count),
        format!("level {}", group.level.as_str()),
        format!("logger {}", escape_visible(&group.logger)),
        format!(
            "exception {}",
            group
                .terminal_exception
                .as_deref()
                .map(escape_visible)
                .unwrap_or_else(|| "-".into())
        ),
        format!("template {}", escape_visible(&group.template.join(" "))),
        format!("nodes {}", escape_visible(&group.nodes.join(","))),
        format!("first {}", group.first_seen.format("%Y-%m-%d %H:%M:%S")),
        format!("last {}", group.last_seen.format("%H:%M:%S")),
        format!("muted {}", group.muted),
        evidence_status(group),
        String::new(),
        "sample".into(),
    ];
    if group.sample_available {
        if group.sample.is_empty() {
            lines.push("(empty)".into());
        } else {
            for line in group.sample.split('\n') {
                lines.push(escape_visible(line));
            }
        }
    } else {
        lines.push("sample unavailable".into());
    }
    lines
}

fn evidence_status(group: &GroupAggregate) -> String {
    if !group.sample_available {
        "evidence unavailable".into()
    } else if group.sample_truncated {
        format!(
            "evidence truncated {}B {}L",
            group.sample_original_bytes, group.sample_original_lines
        )
    } else {
        format!(
            "evidence {}B {}L",
            group.sample_original_bytes, group.sample_original_lines
        )
    }
}

fn metadata_haystack(group: &GroupAggregate) -> String {
    let exception = group.terminal_exception.as_deref().unwrap_or("");
    format!(
        "{} {} {} {} {} {} {}",
        group.id,
        group.count,
        group.level.as_str(),
        group.logger,
        exception,
        group.template.join(" "),
        group.nodes.join(",")
    )
}

fn search_status_line(app: &App) -> String {
    match app.search_matches() {
        Some(true) => "match".into(),
        Some(false) => match &app.search {
            SearchMode::Invalid { .. } => "invalid regex".into(),
            _ => "no match".into(),
        },
        None => String::new(),
    }
}

fn escape_visible(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let code = ch as u32;
        if ch == '\\' {
            out.push_str("\\\\");
        } else if ch == '\t' {
            out.push_str("\\t");
        } else if ch == '\r' {
            out.push_str("\\r");
        } else if (code < 0x20 && ch != '\n') || (0x7f..=0x9f).contains(&code) {
            out.push_str(&format!("\\x{code:02x}"));
        } else {
            out.push(ch);
        }
    }
    out
}

fn render_help(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let width = area.width.clamp(40, 72);
    let height = area.height.saturating_sub(2).clamp(12, 22);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);
    let sorts = app
        .view
        .sorts()
        .iter()
        .map(|col| col.label())
        .collect::<Vec<_>>()
        .join(", ");
    let text = vec![
        Line::from(Span::styled("keys", role_emphasis())),
        Line::from("j/k, down/up  move list or scroll detail"),
        Line::from("h/l, left/right  pan detail"),
        Line::from("Tab  next view: Volume, New, Increasing, Muted"),
        Line::from("s  cycle sort for the current view"),
        Line::from("m  mute or unmute selected group"),
        Line::from("Enter  open group detail"),
        Line::from("Esc  close help, cancel search, or leave detail"),
        Line::from("/  search metadata in detail"),
        Line::from("?  toggle help"),
        Line::from("q  quit; Ctrl+C also quits and restores the terminal"),
        Line::from(Span::styled("search", role_emphasis())),
        Line::from("substring, case-insensitive, metadata only"),
        Line::from("prefix re: for regex; 256-byte cap; invalid stays on detail"),
        Line::from(Span::styled("sort and mute", role_emphasis())),
        Line::from(format!(
            "view {}  sort {}  cycle {}",
            app.view.label(),
            app.sort.label(),
            sorts
        )),
        Line::from("muted groups keep ingesting; they leave Volume/New/Increasing"),
        Line::from("and appear in Muted until unmuted"),
        Line::from(Span::styled("shutdown", role_emphasis())),
        Line::from("q or Ctrl+C request shutdown; help keys do not reach the list"),
    ];
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("help"))
            .style(role_emphasis()),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::cli::Level;
    use crate::app::live::{GroupAggregate, OverflowState, ProcessState};
    use crate::app::rate::RateParams;
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
            node_count: 1,
            nodes_capped: false,
            sample: message.into(),
            sample_available: true,
            sample_truncated: false,
            sample_original_bytes: message.len(),
            sample_original_lines: 1,
            muted: false,
            fast: 0.0,
            baseline: 0.0,
            level: Level::Error,
            logger: "com.example.Foo".into(),
            terminal_exception: None,
            is_overflow: false,
            capacity_global: 0,
            capacity_template_bucket: 0,
        }
    }

    fn snapshot_at(groups: Vec<GroupAggregate>, now: chrono::DateTime<Utc>) -> Snapshot {
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
            now,
            rate_params: RateParams::default(),
            generation: 1,
        }
    }

    fn snapshot(groups: Vec<GroupAggregate>) -> Snapshot {
        snapshot_at(groups, instant())
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
        let ids: Vec<u64> = snap
            .view_rows(View::Volume, View::Volume.default_sort())
            .into_iter()
            .map(|g| g.id)
            .collect();
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
    fn tab_cycles_fixed_views_and_enter_opens_detail() {
        let mut app = App::new(snapshot(vec![group(1, 1, "a")]));
        assert_eq!(app.view, View::Volume);
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.view, View::New);
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.view, View::Increasing);
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.view, View::Muted);
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.view, View::Volume);
        let before = app.selected;
        app.handle(press(KeyCode::Enter));
        assert_eq!(app.selected, before);
        assert_eq!(app.screen, Screen::Detail);
        assert!(!app.help);
        app.handle(press(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Volume);
        assert_eq!(app.selected, before);
    }

    #[test]
    fn footer_lists_only_current_actions() {
        let mut app = App::new(snapshot(vec![]));
        assert_eq!(
            app.footer(),
            "[q]uit  [?]help  [j/k]move  [Tab]view  [s]ort  [m]ute  [Enter]detail  sort COUNT"
        );
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
        let app = App::new(snapshot(vec![group(1, 4, "boom")]));
        let text = header_text(&app);
        assert!(text.contains("p1"));
        assert!(text.contains("e1"));
        assert!(text.contains("author"));
        assert!(text.contains("events 4"));
        assert!(text.contains("groups 1"));
        assert!(text.contains("overflow"));
        assert!(text.contains("none"));
        assert!(text.contains("diag 2"));
        assert!(!text.contains("Connected"));
        assert_eq!(view_tabs(View::Volume), "[Volume] New Increasing Muted");
        assert_eq!(view_tabs(View::New), "Volume [New] Increasing Muted");
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
        assert!(help.contains("mute or unmute"), "{help}");
        assert!(help.contains("substring, case-insensitive"), "{help}");
        assert!(help.contains("prefix re:"), "{help}");
        assert!(help.contains("Ctrl+C"), "{help}");
        assert!(help.contains("Volume, New, Increasing, Muted"), "{help}");
        assert!(text.contains("[Volume]"), "{text}");
        assert!(text.contains("*COUNT"), "{text}");
    }

    #[test]
    fn mute_moves_selected_group_to_muted_view() {
        let mut app = App::new(snapshot(vec![group(1, 4, "a"), group(2, 2, "b")]));
        assert_eq!(app.selected, Some(1));
        app.handle(press(KeyCode::Char('m')));
        assert_eq!(app.rows().iter().map(|g| g.id).collect::<Vec<_>>(), [2]);
        assert_eq!(app.selected, Some(2));
        assert_eq!(app.pending_mutes, [1]);
        app.pending_mutes.clear();
        app.handle(press(KeyCode::Tab));
        app.handle(press(KeyCode::Tab));
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.view, View::Muted);
        assert_eq!(app.rows().iter().map(|g| g.id).collect::<Vec<_>>(), [1]);
        assert_eq!(app.selected, Some(1));
        app.handle(press(KeyCode::Char('m')));
        assert!(app.rows().is_empty());
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.view, View::Volume);
        assert_eq!(app.rows().iter().map(|g| g.id).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn sort_cycles_keep_group_id_and_show_indicator() {
        let mut a = group(1, 4, "a");
        a.last_seen = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 1).unwrap();
        a.fast = 1.0;
        let mut b = group(2, 4, "b");
        b.last_seen = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 9).unwrap();
        b.fast = 3.0;
        let mut app = App::new(snapshot(vec![a, b]));
        assert_eq!(app.rows().iter().map(|g| g.id).collect::<Vec<_>>(), [1, 2]);
        app.handle(press(KeyCode::Char('j')));
        assert_eq!(app.selected, Some(2));
        app.handle(press(KeyCode::Char('s')));
        assert_eq!(app.sort, SortColumn::LastSeen);
        assert_eq!(app.rows().iter().map(|g| g.id).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(app.selected, Some(2));
        assert!(app.footer().contains("sort LAST"));
        app.handle(press(KeyCode::Char('s')));
        assert_eq!(app.sort, SortColumn::Fast);
        assert_eq!(app.rows().iter().map(|g| g.id).collect::<Vec<_>>(), [2, 1]);
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("*RATE"), "{text}");
        assert!(text.contains("[Volume]"), "{text}");
        assert!(text.contains("*RATE"), "{text}");
    }

    #[test]
    fn view_membership_follows_snapshot_clock_and_predicates() {
        let now = Utc.with_ymd_and_hms(2026, 8, 26, 12, 2, 0).unwrap();
        let mut fresh = group(1, 1, "new");
        fresh.first_seen = Utc.with_ymd_and_hms(2026, 8, 26, 12, 1, 30).unwrap();
        let mut rising = group(2, 20, "inc");
        rising.first_seen = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        rising.fast = 10.0;
        rising.baseline = 2.0;
        let mut quiet = group(3, 9, "old");
        quiet.first_seen = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        quiet.fast = 1.0;
        quiet.baseline = 1.0;
        let mut app = App::new(snapshot_at(vec![fresh, rising, quiet], now));
        assert_eq!(
            app.rows().iter().map(|g| g.id).collect::<Vec<_>>(),
            [2, 3, 1]
        );
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.view, View::New);
        assert_eq!(app.rows().iter().map(|g| g.id).collect::<Vec<_>>(), [1]);
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.view, View::Increasing);
        assert_eq!(app.rows().iter().map(|g| g.id).collect::<Vec<_>>(), [2]);
        let later = Utc.with_ymd_and_hms(2026, 8, 26, 12, 3, 0).unwrap();
        let mut aged = group(1, 1, "new");
        aged.first_seen = Utc.with_ymd_and_hms(2026, 8, 26, 12, 1, 30).unwrap();
        let mut still_rising = group(2, 20, "inc");
        still_rising.first_seen = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        still_rising.fast = 10.0;
        still_rising.baseline = 2.0;
        app.view = View::New;
        app.sort = View::New.default_sort();
        app.apply_snapshot(snapshot_at(vec![aged, still_rising], later));
        assert!(app.rows().is_empty());
        app.view = View::Increasing;
        app.sort = View::Increasing.default_sort();
        app.sync_selection();
        assert_eq!(app.rows().iter().map(|g| g.id).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn selection_follows_id_through_filter_mute_and_merge() {
        let mut app = App::new(snapshot(vec![
            group(1, 5, "a"),
            group(2, 3, "b"),
            group(3, 1, "c"),
        ]));
        app.handle(press(KeyCode::Char('j')));
        assert_eq!(app.selected, Some(2));
        let mut muted = group(2, 8, "b");
        muted.muted = true;
        app.apply_snapshot(snapshot(vec![group(1, 5, "a"), muted, group(3, 1, "c")]));
        assert_eq!(app.selected, Some(3));
        app.apply_snapshot(snapshot(vec![group(1, 9, "merged"), group(3, 1, "c")]));
        assert_eq!(app.selected, Some(3));
        app.apply_snapshot(snapshot(vec![group(1, 9, "merged")]));
        assert_eq!(app.selected, Some(1));
    }

    fn type_query(app: &mut App, query: &str) {
        app.handle(press(KeyCode::Char('/')));
        for ch in query.chars() {
            app.handle(press(KeyCode::Char(ch)));
        }
        app.handle(press(KeyCode::Enter));
    }

    #[test]
    fn enter_opens_detail_and_escape_returns_one_level() {
        let mut app = App::new(snapshot(vec![group(1, 2, "boom")]));
        app.handle(press(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Detail);
        app.handle(press(KeyCode::Char('/')));
        assert!(app.editing_search());
        app.handle(press(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Detail);
        assert!(!app.editing_search());
        app.handle(press(KeyCode::Char('?')));
        assert!(app.help);
        app.handle(press(KeyCode::Esc));
        assert!(!app.help);
        assert_eq!(app.screen, Screen::Detail);
        app.handle(press(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Volume);
    }

    #[test]
    fn detail_jk_and_arrows_scroll_vertically() {
        let mut g = group(1, 1, "line0");
        g.sample = (0..20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        g.sample_original_lines = 20;
        let mut app = App::new(snapshot(vec![g]));
        app.handle(press(KeyCode::Enter));
        assert_eq!(app.scroll_y, 0);
        app.handle(press(KeyCode::Char('j')));
        assert_eq!(app.scroll_y, 1);
        app.handle(press(KeyCode::Down));
        assert_eq!(app.scroll_y, 2);
        app.handle(press(KeyCode::Char('k')));
        assert_eq!(app.scroll_y, 1);
        app.handle(press(KeyCode::Up));
        assert_eq!(app.scroll_y, 0);
        app.handle(press(KeyCode::Up));
        assert_eq!(app.scroll_y, 0);
    }

    #[test]
    fn detail_hl_and_arrows_pan_without_wrapping() {
        let mut g = group(1, 1, "short");
        g.sample = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".into();
        let mut app = App::new(snapshot(vec![g]));
        app.handle(press(KeyCode::Enter));
        let before = visible_detail_lines(app.selected_group().unwrap(), 0, 0);
        app.handle(press(KeyCode::Char('l')));
        app.handle(press(KeyCode::Right));
        assert_eq!(app.scroll_x, 2);
        let after = visible_detail_lines(app.selected_group().unwrap(), 0, 2);
        assert_eq!(after.len(), before.len());
        let sample_before = before.iter().find(|line| line.contains("ABCDEF")).unwrap();
        let sample_after = after.iter().find(|line| line.contains("CDEF")).unwrap();
        assert!(sample_before.starts_with("ABCDEF"), "{sample_before}");
        assert!(sample_after.starts_with("CDEF"), "{sample_after}");
        assert!(!sample_after.contains("AB"), "{sample_after}");
        app.handle(press(KeyCode::Char('h')));
        app.handle(press(KeyCode::Left));
        assert_eq!(app.scroll_x, 0);
        app.handle(press(KeyCode::Left));
        assert_eq!(app.scroll_x, 0);
    }

    #[test]
    fn control_bytes_render_as_visible_escapes() {
        let mut g = group(1, 1, "plain");
        g.sample = "ok\u{01}\u{9f}\t\\\nnext".into();
        g.sample_original_bytes = g.sample.len();
        g.sample_original_lines = 2;
        let lines = detail_lines(&g);
        let joined = lines.join("\n");
        assert!(joined.contains("ok\\x01\\x9f\\t\\\\"), "{joined}");
        assert!(joined.contains("next"), "{joined}");
        assert!(!joined.contains('\u{01}'), "{joined}");
        assert!(!joined.contains('\u{9f}'), "{joined}");
    }

    #[test]
    fn slash_search_is_metadata_only_and_case_insensitive() {
        let mut g = group(1, 4, "Failed to start bundle");
        g.sample = "SECRET_BODY should not match".into();
        g.logger = "com.example.FooBar".into();
        let mut app = App::new(snapshot(vec![g]));
        app.handle(press(KeyCode::Enter));
        type_query(&mut app, "foobar");
        assert_eq!(app.search_matches(), Some(true));
        type_query(&mut app, "SECRET_BODY");
        assert_eq!(app.search_matches(), Some(false));
        type_query(&mut app, "FAILED TO START");
        assert_eq!(app.search_matches(), Some(true));
    }

    #[test]
    fn regex_search_length_limit_and_invalid_regex_are_preserved() {
        let mut app = App::new(snapshot(vec![group(1, 1, "boom")]));
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('/')));
        for _ in 0..MAX_SEARCH_BYTES {
            app.handle(press(KeyCode::Char('a')));
        }
        app.handle(press(KeyCode::Char('b')));
        match &app.search {
            SearchMode::Editing { buffer } => {
                assert_eq!(buffer.len(), MAX_SEARCH_BYTES);
                assert!(!buffer.contains('b'));
            }
            other => panic!("expected editing, got {other:?}"),
        }
        app.handle(press(KeyCode::Esc));
        type_query(&mut app, "re:[");
        match &app.search {
            SearchMode::Invalid { query } => assert_eq!(query, "re:["),
            other => panic!("expected invalid, got {other:?}"),
        }
        assert_eq!(app.search_matches(), Some(false));
        assert_eq!(app.screen, Screen::Detail);
        assert!(app.footer().contains("re:["));
        type_query(&mut app, "re:BOOM");
        assert_eq!(app.search_matches(), Some(true));
    }

    #[test]
    fn detail_shows_evidence_and_aggregate_status() {
        let mut available = group(1, 7, "Failed to start bundle");
        available.sample_truncated = true;
        available.sample_original_bytes = 4096;
        available.sample_original_lines = 40;
        available.terminal_exception = Some("java.lang.NullPointerException".into());
        let mut missing = group(2, 3, "other");
        missing.sample_available = false;
        missing.sample.clear();
        let mut app = App::new(snapshot(vec![available, missing]));
        app.handle(press(KeyCode::Enter));
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("detail 1"), "{text}");
        assert!(text.contains("count 7"), "{text}");
        assert!(text.contains("ERROR"), "{text}");
        assert!(text.contains("java.lang.NullPointerException"), "{text}");
        assert!(text.contains("evidence truncated 4096B 40L"), "{text}");
        assert!(text.contains("Failed to start bundle"), "{text}");
        app.handle(press(KeyCode::Esc));
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Enter));
        terminal.draw(|frame| render(frame, &app)).expect("missing");
        let missing_text = buffer_text(&terminal);
        assert!(
            missing_text.contains("evidence unavailable"),
            "{missing_text}"
        );
        assert!(
            missing_text.contains("sample unavailable"),
            "{missing_text}"
        );
    }

    fn draw_at(app: &App, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, app)).expect("draw");
        terminal
    }

    fn has_fg(terminal: &Terminal<TestBackend>, color: Color) -> bool {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.fg == color)
    }

    fn has_modifier(terminal: &Terminal<TestBackend>, modifier: Modifier) -> bool {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.modifier.contains(modifier))
    }

    #[test]
    fn column_plan_hides_nodes_then_exception_then_logger() {
        let wide: Vec<&str> = column_plan(120).into_iter().map(|col| col.0).collect();
        assert!(wide.contains(&"NODES"));
        assert!(wide.contains(&"EXCEPT"));
        assert!(wide.contains(&"LOGGER"));
        assert!(wide.contains(&"COUNT"));
        assert!(wide.contains(&"RATE"));
        assert!(wide.contains(&"TEMPLATE"));
        assert!(wide.contains(&"LAST"));
        let mid: Vec<&str> = column_plan(100).into_iter().map(|col| col.0).collect();
        assert!(!mid.contains(&"NODES"));
        assert!(mid.contains(&"EXCEPT"));
        assert!(mid.contains(&"LOGGER"));
        let collapsed: Vec<&str> = column_plan(80).into_iter().map(|col| col.0).collect();
        assert!(!collapsed.contains(&"NODES"));
        assert!(!collapsed.contains(&"EXCEPT"));
        assert!(!collapsed.contains(&"LOGGER"));
        assert!(collapsed.contains(&"COUNT"));
        assert!(collapsed.contains(&"RATE"));
        assert!(collapsed.contains(&"TEMPLATE"));
        assert!(collapsed.contains(&"LAST"));
        let mid_logger: Vec<&str> = column_plan(90).into_iter().map(|col| col.0).collect();
        assert!(mid_logger.contains(&"LOGGER"));
        assert!(!mid_logger.contains(&"EXCEPT"));
    }

    #[test]
    fn pty_sizes_keep_list_and_detail_usable() {
        let mut rising = group(1, 9, "Failed to start bundle");
        rising.fast = 10.0;
        rising.baseline = 1.0;
        rising.logger = "com.example.LoggerName".into();
        rising.terminal_exception = Some("java.lang.Boom".into());
        rising.nodes = vec!["author-0".into(), "author-1".into()];
        let mut app = App::new(snapshot(vec![rising]));
        for (w, h) in [(80, 24), (120, 40), (200, 60)] {
            let list = draw_at(&app, w, h);
            let text = buffer_text(&list);
            assert!(text.contains("Failed to start bundle"), "{w}x{h} {text}");
            assert!(text.contains("COUNT"), "{w}x{h} {text}");
            assert!(text.contains("[Volume]"), "{w}x{h} {text}");
            assert!(!text.contains("too small"), "{w}x{h} {text}");
            if w >= 120 {
                assert!(text.contains("author-0"), "{w}x{h} {text}");
            } else {
                assert!(!text.contains("author-1"), "{w}x{h} {text}");
            }
        }
        app.handle(press(KeyCode::Enter));
        let detail = draw_at(&app, 80, 24);
        let text = buffer_text(&detail);
        assert!(text.contains("detail 1"), "{text}");
        assert!(text.contains("count 9"), "{text}");
    }

    #[test]
    fn below_minimum_shows_resize_gate_and_keeps_state() {
        let mut app = App::new(snapshot(vec![group(1, 4, "keep me")]));
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('/')));
        app.handle(press(KeyCode::Char('k')));
        app.handle(press(KeyCode::Char('e')));
        app.handle(press(KeyCode::Char('e')));
        app.handle(press(KeyCode::Char('p')));
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('j')));
        let selected = app.selected;
        let view = app.view;
        let query = match &app.search {
            SearchMode::Applied { query, .. } => query.clone(),
            other => panic!("expected applied search, got {other:?}"),
        };
        let scroll = app.scroll_y;
        let small = draw_at(&app, 40, 10);
        let text = buffer_text(&small);
        assert!(text.contains("too small"), "{text}");
        assert!(text.contains("ingestion continues"), "{text}");
        assert!(!text.contains("keep me"), "{text}");
        let restored = draw_at(&app, 120, 40);
        let restored_text = buffer_text(&restored);
        assert!(restored_text.contains("detail 1"), "{restored_text}");
        assert!(restored_text.contains("keep"), "{restored_text}");
        assert_eq!(app.selected, selected);
        assert_eq!(app.view, view);
        assert_eq!(app.scroll_y, scroll);
        match &app.search {
            SearchMode::Applied { query: again, .. } => assert_eq!(again, &query),
            other => panic!("search lost after resize, got {other:?}"),
        }
        let again_small = draw_at(&app, 79, 23);
        assert!(buffer_text(&again_small).contains("too small"));
        let wide = draw_at(&app, 200, 60);
        assert!(buffer_text(&wide).contains("detail 1"));
        assert_eq!(app.selected, selected);
        assert_eq!(app.scroll_y, scroll);
    }

    #[test]
    fn no_color_keeps_cues_without_foreground() {
        let mut rising = group(1, 8, "hot path");
        rising.fast = 12.0;
        rising.baseline = 1.0;
        rising.muted = true;
        rising.level = Level::Error;
        let mut app = App::new(snapshot(vec![rising]));
        app.color = false;
        app.view = View::Muted;
        app.sort = View::Muted.default_sort();
        app.sync_selection();
        let terminal = draw_at(&app, 120, 24);
        let text = buffer_text(&terminal);
        assert!(text.contains("[M]"), "{text}");
        assert!(text.contains("^") || text.contains("E "), "{text}");
        assert!(text.contains("ERROR"), "{text}");
        assert!(has_modifier(&terminal, Modifier::REVERSED));
        assert!(has_modifier(&terminal, Modifier::BOLD));
        assert!(!has_fg(&terminal, Color::Red));
        assert!(!has_fg(&terminal, Color::Yellow));
        assert!(!has_fg(&terminal, Color::Cyan));
        assert!(!has_fg(&terminal, Color::Green));
        assert!(!has_fg(&terminal, Color::DarkGray));
    }

    #[test]
    fn color_roles_use_terminal_native_slots() {
        let mut rising = group(1, 8, "hot path");
        rising.fast = 12.0;
        rising.baseline = 1.0;
        rising.level = Level::Error;
        let mut ended = snapshot(vec![rising]);
        ended.process = ProcessState::Ended;
        ended.overflow = OverflowState::Events(3);
        let mut app = App::new(ended);
        app.color = true;
        let terminal = draw_at(&app, 120, 24);
        let text = buffer_text(&terminal);
        assert!(text.contains("E "), "{text}");
        assert!(
            text.contains("ok") || text.contains("end") || text.contains("!"),
            "{text}"
        );
        assert!(has_fg(&terminal, Color::Red));
        assert!(has_modifier(&terminal, Modifier::REVERSED));
        assert!(has_modifier(&terminal, Modifier::BOLD));
    }

    #[test]
    fn help_traps_keys_and_documents_current_view() {
        let mut app = App::new(snapshot(vec![group(1, 1, "a"), group(2, 1, "b")]));
        let selected = app.selected;
        app.handle(press(KeyCode::Char('?')));
        assert!(app.help);
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Tab));
        app.handle(press(KeyCode::Char('s')));
        app.handle(press(KeyCode::Char('m')));
        app.handle(press(KeyCode::Enter));
        assert_eq!(app.selected, selected);
        assert_eq!(app.view, View::Volume);
        assert_eq!(app.screen, Screen::Volume);
        assert!(app.pending_mutes.is_empty());
        let help = draw_at(&app, 120, 40);
        let text = buffer_text(&help);
        assert!(
            text.contains("cycle COUNT, LAST, RATE") || text.contains("COUNT, LAST, RATE"),
            "{text}"
        );
        assert!(text.contains("muted groups keep ingesting"), "{text}");
        assert!(text.contains("q or Ctrl+C"), "{text}");
        assert!(!app.handle(press(KeyCode::Char('x'))));
        assert!(app.handle(press(KeyCode::Char('q'))));
    }

    #[test]
    fn run_ui_does_not_enable_mouse_capture() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/app/tui.rs"));
        let live = src.split("mod tests").next().expect("source");
        assert!(!live.contains("EnableMouseCapture"));
        assert!(!live.contains("DisableMouseCapture"));
        assert!(!live.contains("MouseEvent"));
    }
}
