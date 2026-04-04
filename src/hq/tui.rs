use std::{
    env, fs,
    io::{self, Stdout},
    path::{Path, PathBuf},
};

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Flex, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame, Terminal, TerminalOptions, Viewport,
};
use tokio::sync::{mpsc, oneshot, watch};
use tui_big_text::{BigText, PixelSize};

use crate::state::machine::StateData;

const MAX_HISTORY: usize = 100;
const MAX_COMPLETIONS: usize = 8;
const COMMANDS: &[(&str, &str)] = &[
    ("/plan", "spawn supervisor, plan a task"),
    ("/execute", "start executor manually"),
    ("/review", "spawn supervisor in review mode"),
    ("/status", "show task state and agents"),
    ("/attach", "attach terminal to session"),
    ("/stop", "stop all running sessions"),
    ("/reset", "reset state to Idle"),
    ("/init", "initialize ferrus in current directory"),
    ("/register", "register agent configs"),
    ("/help", "list all commands"),
    ("/quit", "exit HQ"),
];

pub enum UiMessage {
    Info(String),
    Error(String),
    Transition {
        from: String,
        to: String,
    },
    StatusUpdate(StatusSnapshot),
    Suspend {
        ack: oneshot::Sender<()>,
    },
    Resume,
    ConfirmationRequest {
        prompt: String,
        reply: oneshot::Sender<bool>,
    },
}

#[derive(Clone, Default)]
pub struct StatusSnapshot {
    pub task_state: String,
    pub claimed_by: Option<String>,
    pub retries: u32,
    pub cycles: u32,
    pub supervisor_status: String,
    pub executor_status: String,
}

impl StatusSnapshot {
    pub fn from_state_data(state: &StateData) -> StatusSnapshot {
        StatusSnapshot {
            task_state: format!("{:?}", state.state),
            claimed_by: state.claimed_by.clone(),
            retries: state.check_retries,
            cycles: state.review_cycles,
            supervisor_status: "none".to_string(),
            executor_status: "none".to_string(),
        }
    }
}

struct ConfirmationState {
    prompt: String,
    reply: oneshot::Sender<bool>,
}

pub struct App {
    version: String,
    current_dir: String,
    supervisor_type: String,
    executor_type: String,
    supervisor_version: String,
    executor_version: String,
    status: StatusSnapshot,
    messages: Vec<Line<'static>>,
    scroll_offset: usize,
    new_messages_while_scrolled: usize,
    input: String,
    cursor_pos: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    history_saved: String,
    completion_candidates: Vec<(&'static str, &'static str)>,
    completion_selected: usize,
    completion_active: bool,
    confirmation: Option<ConfirmationState>,
    suspended: bool,
    should_quit: bool,
}

impl App {
    fn new(
        supervisor_type: String,
        executor_type: String,
        supervisor_version: String,
        executor_version: String,
    ) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            current_dir: current_dir_label(),
            supervisor_type,
            executor_type,
            supervisor_version,
            executor_version,
            status: StatusSnapshot::default(),
            messages: Vec::new(),
            scroll_offset: 0,
            new_messages_while_scrolled: 0,
            input: String::new(),
            cursor_pos: 0,
            history: load_history(),
            history_idx: None,
            history_saved: String::new(),
            completion_candidates: Vec::new(),
            completion_selected: 0,
            completion_active: false,
            confirmation: None,
            suspended: false,
            should_quit: false,
        }
    }

    fn push_message(&mut self, line: Line<'static>) {
        self.messages.push(line);
        if self.scroll_offset == 0 {
            self.new_messages_while_scrolled = 0;
        } else {
            self.new_messages_while_scrolled += 1;
        }
    }

    fn clear_completion(&mut self) {
        self.completion_candidates.clear();
        self.completion_selected = 0;
        self.completion_active = false;
    }

    fn insert_char(&mut self, ch: char) {
        let idx = byte_index_for_char(&self.input, self.cursor_pos);
        self.input.insert(idx, ch);
        self.cursor_pos += 1;
        self.history_idx = None;
        self.update_command_context();
    }

    fn delete_before_cursor(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let end = byte_index_for_char(&self.input, self.cursor_pos);
        let start = byte_index_for_char(&self.input, self.cursor_pos - 1);
        self.input.replace_range(start..end, "");
        self.cursor_pos -= 1;
        self.history_idx = None;
        self.update_command_context();
    }

    fn delete_after_cursor(&mut self) {
        if self.cursor_pos >= self.input.chars().count() {
            return;
        }
        let start = byte_index_for_char(&self.input, self.cursor_pos);
        let end = byte_index_for_char(&self.input, self.cursor_pos + 1);
        self.input.replace_range(start..end, "");
        self.history_idx = None;
        self.update_command_context();
    }

    fn move_left(&mut self) {
        self.cursor_pos = self.cursor_pos.saturating_sub(1);
    }

    fn move_right(&mut self) {
        let len = self.input.chars().count();
        self.cursor_pos = (self.cursor_pos + 1).min(len);
    }

    fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    fn move_end(&mut self) {
        self.cursor_pos = self.input.chars().count();
        self.scroll_offset = 0;
        self.new_messages_while_scrolled = 0;
    }

    fn history_up(&mut self) {
        if self.has_command_context() {
            self.previous_completion();
            return;
        }
        if self.history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.history_saved = self.input.clone();
                self.history_idx = Some(self.history.len() - 1);
            }
            Some(0) => {}
            Some(idx) => self.history_idx = Some(idx - 1),
        }
        if let Some(idx) = self.history_idx {
            self.input = self.history[idx].clone();
            self.cursor_pos = self.input.chars().count();
        }
        self.update_command_context();
    }

    fn history_down(&mut self) {
        if self.has_command_context() {
            self.next_completion();
            return;
        }
        match self.history_idx {
            None => {}
            Some(idx) if idx + 1 < self.history.len() => {
                self.history_idx = Some(idx + 1);
                self.input = self.history[idx + 1].clone();
                self.cursor_pos = self.input.chars().count();
            }
            Some(_) => {
                self.history_idx = None;
                self.input = self.history_saved.clone();
                self.cursor_pos = self.input.chars().count();
            }
        }
        self.update_command_context();
    }

    fn page_up(&mut self, height: usize) {
        self.scroll_offset += (height.max(2)) / 2;
    }

    fn page_down(&mut self, height: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub((height.max(2)) / 2);
        if self.scroll_offset == 0 {
            self.new_messages_while_scrolled = 0;
        }
    }

    fn clear_messages(&mut self) {
        self.messages.clear();
        self.scroll_offset = 0;
        self.new_messages_while_scrolled = 0;
    }

    fn completion_prefix(&self) -> &str {
        self.input.trim()
    }

    fn has_command_context(&self) -> bool {
        self.completion_prefix().starts_with('/') && !self.completion_candidates.is_empty()
    }

    fn compute_completions(&mut self) {
        let prefix = self.completion_prefix();
        self.completion_candidates = COMMANDS
            .iter()
            .copied()
            .filter(|(cmd, _)| cmd.starts_with(prefix))
            .take(MAX_COMPLETIONS)
            .collect();
        self.completion_selected = 0;
    }

    fn refresh_completions(&mut self) {
        let prefix = self.completion_prefix();
        let needs_refresh = self.completion_candidates.is_empty()
            || self
                .completion_candidates
                .iter()
                .any(|(cmd, _)| !cmd.starts_with(prefix));
        if needs_refresh {
            self.compute_completions();
        }
    }

    fn update_command_context(&mut self) {
        if self.completion_prefix().starts_with('/') {
            self.compute_completions();
            if self.completion_candidates.is_empty() {
                self.completion_active = false;
            } else if self.completion_selected >= self.completion_candidates.len() {
                self.completion_selected = 0;
            }
        } else {
            self.clear_completion();
        }
    }

    fn accept_completion(&mut self) {
        if let Some((cmd, _)) = self.completion_candidates.get(self.completion_selected) {
            self.input = (*cmd).to_string();
            self.cursor_pos = self.input.chars().count();
        }
        self.clear_completion();
    }

    fn next_completion(&mut self) {
        self.refresh_completions();
        if self.completion_candidates.is_empty() {
            self.completion_active = false;
            return;
        }

        let prefix = self.completion_prefix().to_string();
        let shared_prefix = longest_common_prefix(&self.completion_candidates);
        if shared_prefix.len() > prefix.len() {
            self.input = shared_prefix.to_string();
            self.cursor_pos = self.input.chars().count();
            self.compute_completions();
            if self.completion_candidates.len() == 1 {
                self.accept_completion();
            } else {
                self.completion_active = true;
            }
            return;
        }

        if self.completion_candidates.len() == 1 {
            self.accept_completion();
            return;
        }
        if !self.completion_active {
            self.completion_active = true;
            self.completion_selected = 0;
            return;
        }
        if self.completion_active {
            self.completion_selected =
                (self.completion_selected + 1) % self.completion_candidates.len();
        }
    }

    fn previous_completion(&mut self) {
        self.refresh_completions();
        if !self.completion_candidates.is_empty() {
            self.completion_active = true;
            self.completion_selected = if self.completion_selected == 0 {
                self.completion_candidates.len() - 1
            } else {
                self.completion_selected - 1
            };
        }
    }

    fn submit_input(&mut self, cmd_tx: &mpsc::UnboundedSender<String>) {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return;
        }
        if line == "/quit" {
            self.should_quit = true;
        }
        let _ = cmd_tx.send(line.clone());
        if self.history.last() != Some(&line) {
            self.history.push(line);
            if self.history.len() > MAX_HISTORY {
                let extra = self.history.len() - MAX_HISTORY;
                self.history.drain(0..extra);
            }
        }
        self.input.clear();
        self.cursor_pos = 0;
        self.history_idx = None;
        self.history_saved.clear();
        self.clear_completion();
    }
}

pub async fn run_tui(
    mut msg_rx: mpsc::UnboundedReceiver<UiMessage>,
    cmd_tx: mpsc::UnboundedSender<String>,
    mut state_rx: watch::Receiver<Option<StateData>>,
    supervisor_type: String,
    executor_type: String,
    supervisor_version: String,
    executor_version: String,
) -> Result<()> {
    let mut app = App::new(
        supervisor_type,
        executor_type,
        supervisor_version,
        executor_version,
    );
    if let Some(state) = state_rx.borrow().clone() {
        app.status = StatusSnapshot::from_state_data(&state);
    }

    let mut terminal = enter_tui()?;
    let mut event_stream = EventStream::new();
    terminal.draw(|f| draw(f, &app))?;

    loop {
        tokio::select! {
            maybe_event = event_stream.next(), if !app.suspended => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let terminal_height = terminal.size()?.height;
                        let popup_height = completion_popup_height(&app, terminal_height);
                        let scroll_height = terminal_height
                            .saturating_sub(11 + popup_height)
                            as usize;
                        handle_event(event, &mut app, &cmd_tx, scroll_height)
                    }
                    Some(Err(err)) => app.push_message(error_line(format!("Event error: {err}"))),
                    None => app.should_quit = true,
                }
            }
            maybe_msg = msg_rx.recv() => {
                match maybe_msg {
                    Some(msg) => handle_message(msg, &mut app, &mut terminal)?,
                    None => app.should_quit = true,
                }
            }
            changed = state_rx.changed() => {
                if changed.is_ok() {
                    if let Some(state) = state_rx.borrow_and_update().clone() {
                        let supervisor_status = app.status.supervisor_status.clone();
                        let executor_status = app.status.executor_status.clone();
                        app.status = StatusSnapshot::from_state_data(&state);
                        app.status.supervisor_status = supervisor_status;
                        app.status.executor_status = executor_status;
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
        if !app.suspended {
            terminal.draw(|f| draw(f, &app))?;
        }
    }

    save_history(&app.history);
    leave_tui(&mut terminal);
    Ok(())
}

fn handle_event(
    event: Event,
    app: &mut App,
    cmd_tx: &mpsc::UnboundedSender<String>,
    scroll_height: usize,
) {
    if app.suspended {
        return;
    }

    let Event::Key(key) = event else {
        return;
    };
    if key.kind != KeyEventKind::Press {
        return;
    }

    if app.confirmation.is_some() {
        handle_confirmation_key(key, app);
        return;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
        (KeyCode::Char('l'), KeyModifiers::CONTROL) => app.clear_messages(),
        (KeyCode::Char('a'), KeyModifiers::CONTROL) | (KeyCode::Home, _) => app.move_home(),
        (KeyCode::Char('e'), KeyModifiers::CONTROL) | (KeyCode::End, _) => app.move_end(),
        (KeyCode::Left, _) => app.move_left(),
        (KeyCode::Right, _) => app.move_right(),
        (KeyCode::Up, _) => app.history_up(),
        (KeyCode::Down, _) => app.history_down(),
        (KeyCode::PageUp, _) => app.page_up(scroll_height),
        (KeyCode::PageDown, _) => app.page_down(scroll_height),
        (KeyCode::Backspace, _) => app.delete_before_cursor(),
        (KeyCode::Delete, _) => app.delete_after_cursor(),
        (KeyCode::Esc, _) => {
            if app.completion_active {
                app.clear_completion();
            } else {
                app.input.clear();
                app.cursor_pos = 0;
                app.history_idx = None;
                app.history_saved.clear();
            }
        }
        (KeyCode::Tab, _) => app.next_completion(),
        (KeyCode::BackTab, _) => app.previous_completion(),
        (KeyCode::Enter, _) => {
            if app.completion_active {
                app.accept_completion();
            } else {
                app.submit_input(cmd_tx);
            }
        }
        (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) => app.insert_char(ch),
        _ => {}
    }
}

fn handle_confirmation_key(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => confirm(app, true),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => confirm(app, false),
        _ => {}
    }
}

fn confirm(app: &mut App, accepted: bool) {
    if let Some(confirm) = app.confirmation.take() {
        let _ = confirm.reply.send(accepted);
    }
}

fn handle_message(
    msg: UiMessage,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    match msg {
        UiMessage::Info(text) => app.push_message(Line::from(text)),
        UiMessage::Error(text) => app.push_message(error_line(text)),
        UiMessage::Transition { from, to } => app.push_message(Line::styled(
            format!("── {from} → {to} ──"),
            Style::default()
                .fg(Color::Rgb(210, 100, 10))
                .add_modifier(Modifier::BOLD),
        )),
        UiMessage::StatusUpdate(status) => app.status = status,
        UiMessage::Suspend { ack } => {
            leave_tui(terminal);
            app.suspended = true;
            let _ = ack.send(());
        }
        UiMessage::Resume => {
            reenter_tui(terminal)?;
            app.suspended = false;
            terminal.clear()?;
        }
        UiMessage::ConfirmationRequest { prompt, reply } => {
            app.confirmation = Some(ConfirmationState { prompt, reply });
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame, app: &App) {
    let popup_height = completion_popup_height(app, frame.area().height);
    let scrollback_height = scrollback_height(app, frame.area().height, popup_height);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(1),
            Constraint::Length(scrollback_height),
            Constraint::Length(3),
            Constraint::Length(popup_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_header(frame, layout[0], app);
    draw_tip(frame, layout[1]);
    draw_scrollback(frame, layout[2], app);
    draw_input(frame, layout[3], app);
    if popup_height > 0 {
        draw_completion_popup(frame, layout[4], app);
    }
    draw_state_line(frame, layout[5], app);

    if app.confirmation.is_some() {
        draw_confirmation_popup(frame, app);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(24)])
        .split(area);

    let logo_area = chunks[0].inner(Margin {
        vertical: 1,
        horizontal: 3,
    });
    let logo = BigText::builder()
        .pixel_size(PixelSize::Quadrant)
        .lines(vec![Line::from(vec![
            Span::styled("F", Style::default().fg(Color::Rgb(204, 85, 0))),
            Span::styled("E", Style::default().fg(Color::Rgb(189, 63, 0))),
            Span::styled("R", Style::default().fg(Color::Rgb(160, 40, 0))),
            Span::styled("R", Style::default().fg(Color::Rgb(139, 25, 0))),
            Span::styled("U", Style::default().fg(Color::Rgb(180, 55, 0))),
            Span::styled("S", Style::default().fg(Color::Rgb(210, 100, 10))),
        ])])
        .build();
    frame.render_widget(logo, logo_area);

    let info = vec![
        header_line(
            "version:",
            format!("v{}", app.version),
            Style::default().fg(Color::Rgb(180, 180, 160)),
            None,
        ),
        header_line(
            "directory:",
            app.current_dir.clone(),
            Style::default(),
            None,
        ),
        header_line(
            "supervisor:",
            app.supervisor_type.clone(),
            Style::default().fg(Color::Rgb(210, 100, 10)),
            (!app.supervisor_version.is_empty()).then_some(app.supervisor_version.clone()),
        ),
        header_line(
            "executor:",
            app.executor_type.clone(),
            Style::default().fg(Color::Rgb(210, 100, 10)),
            (!app.executor_version.is_empty()).then_some(app.executor_version.clone()),
        ),
    ];
    let paragraph = Paragraph::new(info).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(paragraph, chunks[1]);
}

fn draw_tip(frame: &mut Frame, area: Rect) {
    let tip = Line::from(vec![
        Span::styled("Tip: ", Style::default().fg(Color::DarkGray)),
        Span::styled("/plan", Style::default().fg(Color::Rgb(200, 140, 50))),
        Span::styled(" to start a task · ", Style::default().fg(Color::DarkGray)),
        Span::styled("/status", Style::default().fg(Color::Rgb(200, 140, 50))),
        Span::styled(" to check state · ", Style::default().fg(Color::DarkGray)),
        Span::styled("/help", Style::default().fg(Color::Rgb(200, 140, 50))),
        Span::styled(" for all commands", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(tip), area);
}

fn draw_scrollback(frame: &mut Frame, area: Rect, app: &App) {
    let inner_height = area.height as usize;
    let total_lines = app.messages.len();
    let base_scroll = total_lines.saturating_sub(inner_height);
    let vertical_scroll = base_scroll.saturating_sub(app.scroll_offset) as u16;

    let paragraph = Paragraph::new(Text::from(app.messages.clone())).scroll((vertical_scroll, 0));
    frame.render_widget(paragraph, area);
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(format!(" > {}", app.input))),
        inner,
    );

    if inner.width >= 3 {
        let cursor_x = inner.x + (3 + app.cursor_pos as u16).min(inner.width.saturating_sub(1));
        frame.set_cursor_position((cursor_x, inner.y));
    }
}

fn draw_state_line(frame: &mut Frame, area: Rect, app: &App) {
    let task_state = if app.status.task_state.is_empty() {
        "Idle"
    } else {
        app.status.task_state.as_str()
    };
    let claimed_by = app.status.claimed_by.as_deref().unwrap_or("—");
    let line = Line::from(vec![
        Span::styled(task_state, task_state_style(task_state)),
        Span::styled(
            format!(
                "  ·  claimed_by: {claimed_by}  ·  retries: {}  ·  cycles: {}",
                app.status.retries, app.status.cycles
            ),
            Style::default().fg(Color::DarkGray),
        ),
        if app.new_messages_while_scrolled > 0 {
            Span::styled(
                format!("  ·  {} new below", app.new_messages_while_scrolled),
                Style::default().fg(Color::Rgb(210, 100, 10)),
            )
        } else {
            Span::raw("")
        },
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_completion_popup(frame: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line<'static>> = app
        .completion_candidates
        .iter()
        .enumerate()
        .map(|(idx, (cmd, desc))| {
            let style = if app.completion_active && idx == app.completion_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(210, 100, 10))
                    .add_modifier(Modifier::BOLD)
            } else if !app.completion_active && idx == 0 {
                Style::default().fg(Color::Rgb(210, 100, 10))
            } else {
                Style::default()
            };
            Line::styled(format!("  {cmd:<12}  {desc}"), style)
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(" Commands "),
        ),
        area,
    );
}

fn draw_confirmation_popup(frame: &mut Frame, app: &App) {
    let Some(confirm) = app.confirmation.as_ref() else {
        return;
    };

    let area = centered_rect(50, 5, frame.area());
    let lines = vec![
        Line::from(confirm.prompt.as_str()),
        Line::from("[y] confirm    [n / Esc] cancel"),
    ];
    let dialog = Paragraph::new(lines)
        .alignment(Alignment::Left)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(Clear, area);
    frame.render_widget(dialog, area);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal.inner(Margin {
        vertical: 0,
        horizontal: 0,
    })
}

/// Height of the inline TUI block (header + tip + scrollback + input + command context + state).
/// Fixed so the block never grows to fill the whole terminal with empty space.
const TUI_HEIGHT: u16 = 22;

fn enter_tui() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(TUI_HEIGHT),
        },
    )
    .map_err(Into::into)
}

fn reenter_tui(_terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    enable_raw_mode()?;
    Ok(())
}

fn leave_tui(terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
    let _ = disable_raw_mode();
    let _ = terminal.show_cursor();
}

fn history_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_default()
        .join("ferrus")
        .join("history")
}

fn load_history() -> Vec<String> {
    let Ok(contents) = fs::read_to_string(history_path()) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = contents.lines().map(ToOwned::to_owned).collect();
    if lines.len() > MAX_HISTORY {
        lines = lines.split_off(lines.len() - MAX_HISTORY);
    }
    lines
}

fn save_history(history: &[String]) {
    let path = history_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let keep_from = history.len().saturating_sub(MAX_HISTORY);
    let data = history[keep_from..].join("\n");
    let _ = fs::write(path, data);
}

fn current_dir_label() -> String {
    env::current_dir()
        .ok()
        .map(|path| abbreviate_home(&path))
        .unwrap_or_else(|| ".".to_string())
}

fn task_state_style(task_state: &str) -> Style {
    Style::default().fg(match task_state {
        "Idle" => Color::DarkGray,
        "Executing" | "Addressing" | "Checking" => Color::Yellow,
        "Reviewing" => Color::Cyan,
        "Complete" => Color::Green,
        "Failed" => Color::Red,
        "AwaitingHuman" => Color::Magenta,
        _ => Color::White,
    })
}

fn completion_popup_height(app: &App, terminal_height: u16) -> u16 {
    if !app.has_command_context() {
        return 0;
    }
    let max_height = terminal_height.saturating_sub(12).min(8);
    (app.completion_candidates.len() as u16 + 2)
        .min(max_height)
        .max(3)
}

fn scrollback_height(app: &App, terminal_height: u16, popup_height: u16) -> u16 {
    if app.messages.is_empty() {
        return 0;
    }
    terminal_height.saturating_sub(11 + popup_height)
}

fn header_line(
    label: &'static str,
    value: String,
    value_style: Style,
    suffix: Option<String>,
) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(label, Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(value, value_style),
    ];
    if let Some(suffix) = suffix {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(suffix, Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

fn longest_common_prefix(candidates: &[(&'static str, &'static str)]) -> &'static str {
    let Some((first, _)) = candidates.first() else {
        return "";
    };

    let mut end = first.len();
    for (candidate, _) in candidates.iter().skip(1) {
        end = first
            .bytes()
            .zip(candidate.bytes())
            .take_while(|(a, b)| a == b)
            .count()
            .min(end);
    }
    &first[..end]
}

fn abbreviate_home(path: &Path) -> String {
    let full = path.display().to_string();
    let Some(home) = dirs::home_dir() else {
        return full;
    };
    let home = home.display().to_string();
    if full == home {
        "~".to_string()
    } else if let Some(suffix) = full.strip_prefix(&(home + "/")) {
        format!("~/{suffix}")
    } else {
        full
    }
}

fn byte_index_for_char(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or_else(|| s.len())
}

fn error_line(text: impl Into<String>) -> Line<'static> {
    Line::styled(
        text.into(),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tui_tests {
    use super::*;

    #[test]
    fn first_tab_on_multiple_matches_selects_first_candidate() {
        let mut app = App::new(
            "claude-code".into(),
            "codex".into(),
            String::new(),
            String::new(),
        );
        app.input = "/".into();
        app.cursor_pos = app.input.len();

        app.next_completion();

        assert!(app.completion_active);
        assert_eq!(app.completion_selected, 0);
        assert_eq!(app.completion_candidates[0].0, "/plan");
    }

    #[test]
    fn tab_extends_to_shared_prefix_before_cycling() {
        let mut app = App::new(
            "claude-code".into(),
            "codex".into(),
            String::new(),
            String::new(),
        );
        app.input = "/r".into();
        app.cursor_pos = app.input.len();

        app.next_completion();

        assert_eq!(app.input, "/re");
        assert!(app.completion_active);
        assert_eq!(
            app.completion_candidates
                .iter()
                .map(|(cmd, _)| *cmd)
                .collect::<Vec<_>>(),
            vec!["/review", "/reset", "/register"]
        );
    }

    #[test]
    fn abbreviate_home_replaces_home_prefix() {
        let path = Path::new("/home/user/Repos/ferrus");
        assert_eq!(abbreviate_home(path), "~/Repos/ferrus");
    }

    #[test]
    fn typing_slash_command_updates_context_without_tab() {
        let mut app = App::new(
            "claude-code".into(),
            "codex".into(),
            String::new(),
            String::new(),
        );

        app.insert_char('/');
        app.insert_char('s');

        assert!(app.has_command_context());
        assert_eq!(
            app.completion_candidates
                .iter()
                .map(|(cmd, _)| *cmd)
                .collect::<Vec<_>>(),
            vec!["/status", "/stop"]
        );
        assert!(!app.completion_active);
    }
}
