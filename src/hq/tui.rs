use std::{
    env, fs,
    io::{self, Stdout, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    cursor::{MoveDown, MoveToColumn, MoveUp},
    event::{
        self, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    queue,
    style::{style, Attribute, Color, Print, PrintStyledContent, Stylize},
    terminal::{disable_raw_mode, enable_raw_mode, size, Clear, ClearType},
};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot, watch};

use super::state_watcher::{format_elapsed, WatchedState};

const MAX_HISTORY: usize = 100;
const MAX_COMPLETIONS: usize = 8;
const COMMANDS: &[(&str, &str)] = &[
    ("/plan", "spawn supervisor, plan a task"),
    ("/task", "define a task and run executor then review"),
    ("/supervisor", "open an interactive supervisor session"),
    ("/executor", "open an interactive executor session"),
    (
        "/resume",
        "resume the executor headlessly or recover consultation",
    ),
    ("/review", "spawn supervisor in review mode"),
    ("/status", "show task state and agents"),
    (
        "/attach",
        "show log path for a running headless session",
    ),
    ("/stop", "stop all running sessions"),
    ("/reset", "reset state to Idle"),
    ("/init", "initialize ferrus in current directory"),
    ("/register", "register agent configs"),
    ("/model", "set or clear a role model override"),
    ("/help", "list all commands"),
    ("/quit", "exit HQ"),
];

pub enum UiMessage {
    Info(String),
    Error(String),
    Transition {
        from: Option<String>,
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
    pub task_state_detail: String,
    #[allow(dead_code)]
    pub claimed_by: Option<String>,
    pub directory: String,
    pub branch: Option<String>,
    pub retries: u32,
    pub cycles: u32,
    pub supervisor_status: String,
    pub executor_status: String,
}

impl StatusSnapshot {
    pub fn from_watched_state(watched: &WatchedState) -> StatusSnapshot {
        let state = &watched.state;
        let task_state_detail = if matches!(
            state.state,
            crate::state::machine::TaskState::Executing
                | crate::state::machine::TaskState::Checking
                | crate::state::machine::TaskState::Addressing
                | crate::state::machine::TaskState::Consultation
                | crate::state::machine::TaskState::Reviewing
        ) {
            format!(
                "{:?} ({})",
                state.state,
                format_elapsed(watched.state_elapsed)
            )
        } else {
            format!("{:?}", state.state)
        };
        StatusSnapshot {
            task_state: format!("{:?}", state.state),
            task_state_detail,
            claimed_by: state.claimed_by.clone(),
            directory: String::new(),
            branch: None,
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

#[derive(Clone)]
struct TranscriptLine {
    text: String,
    kind: TranscriptKind,
}

#[derive(Clone, Copy)]
enum TranscriptKind {
    Info,
    Error,
    Transition,
}

pub struct App {
    status: StatusSnapshot,
    debug: bool,
    messages: Vec<TranscriptLine>,
    input: String,
    cursor_pos: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    history_saved: String,
    completion_candidates: Vec<(&'static str, &'static str)>,
    completion_selected: usize,
    completion_active: bool,
    completion_hidden: bool,
    confirmation: Option<ConfirmationState>,
    suspended: bool,
    should_quit: bool,
    ctrl_c_pending: bool,
    ctrl_c_at: Option<Instant>,
}

impl App {
    fn new() -> Self {
        Self {
            status: StatusSnapshot::default(),
            debug: false,
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            history: load_history(),
            history_idx: None,
            history_saved: String::new(),
            completion_candidates: Vec::new(),
            completion_selected: 0,
            completion_active: false,
            completion_hidden: false,
            confirmation: None,
            suspended: false,
            should_quit: false,
            ctrl_c_pending: false,
            ctrl_c_at: None,
        }
    }

    fn clear_completion(&mut self) {
        self.completion_candidates.clear();
        self.completion_selected = 0;
        self.completion_active = false;
        self.completion_hidden = false;
    }

    fn hide_completion_popup(&mut self) {
        self.completion_active = false;
        self.completion_hidden = true;
    }

    fn insert_char(&mut self, ch: char) {
        let idx = byte_index_for_char(&self.input, self.cursor_pos);
        self.input.insert(idx, ch);
        self.cursor_pos += 1;
        self.history_idx = None;
        self.update_command_context();
    }

    fn insert_newline(&mut self) {
        let idx = byte_index_for_char(&self.input, self.cursor_pos);
        self.input.insert(idx, '\n');
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
    }

    fn history_up(&mut self) {
        if self.completion_popup_visible() {
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
        if self.completion_popup_visible() {
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

    fn completion_prefix(&self) -> &str {
        self.input.trim()
    }

    fn has_command_context(&self) -> bool {
        self.completion_prefix().starts_with('/') && !self.completion_candidates.is_empty()
    }

    fn completion_popup_visible(&self) -> bool {
        self.confirmation.is_none() && self.has_command_context() && !self.completion_hidden
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
                self.completion_hidden = false;
            } else if self.completion_selected >= self.completion_candidates.len() {
                self.completion_selected = 0;
                self.completion_hidden = false;
            } else {
                self.completion_hidden = false;
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

    fn accept_completion_and_submit(&mut self, cmd_tx: &mpsc::UnboundedSender<String>) {
        self.accept_completion();
        self.submit_input(cmd_tx);
    }

    fn next_completion(&mut self) {
        self.refresh_completions();
        if self.completion_candidates.is_empty() {
            self.completion_active = false;
            return;
        }
        self.completion_hidden = false;

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
        self.completion_selected =
            (self.completion_selected + 1) % self.completion_candidates.len();
    }

    fn previous_completion(&mut self) {
        self.refresh_completions();
        if !self.completion_candidates.is_empty() {
            self.completion_hidden = false;
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
        if !line.contains('\n') && self.history.last() != Some(&line) {
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

struct StartupHeader {
    version: String,
    supervisor_type: String,
    supervisor_version: String,
    executor_type: String,
    executor_version: String,
}

struct TerminalUi {
    prompt_visible: bool,
    prompt_lines: u16,
    prompt_cursor_row: u16,
    lower_lines: u16,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tui(
    mut msg_rx: mpsc::UnboundedReceiver<UiMessage>,
    cmd_tx: mpsc::UnboundedSender<String>,
    mut state_rx: watch::Receiver<Option<WatchedState>>,
    debug: bool,
    supervisor_type: String,
    executor_type: String,
    supervisor_version: String,
    executor_version: String,
) -> Result<()> {
    let directory = current_dir_label();
    let branch = current_git_branch();
    let startup = StartupHeader {
        version: format!("v{}", env!("CARGO_PKG_VERSION")),
        supervisor_type,
        supervisor_version,
        executor_type,
        executor_version,
    };
    let mut app = App::new();
    app.debug = debug;
    app.status.directory = directory.clone();
    app.status.branch = branch.clone();
    if let Some(watched) = state_rx.borrow().clone() {
        let mut status = StatusSnapshot::from_watched_state(&watched);
        status.directory = directory.clone();
        status.branch = branch.clone();
        app.status = status;
    }

    let mut stdout = io::stdout();
    enter_tui()?;
    print_startup_header(&mut stdout, &startup)?;

    let mut ui = TerminalUi {
        prompt_visible: false,
        prompt_lines: 0,
        prompt_cursor_row: 0,
        lower_lines: 0,
    };
    redraw_live_area(&mut stdout, &app, &mut ui)?;

    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            maybe_event = event_stream.next(), if !app.suspended => {
                match maybe_event {
                    Some(Ok(event)) => handle_event(event, &mut app, &cmd_tx, &mut stdout, &mut ui)?,
                    Some(Err(err)) => {
                        print_message_and_restore_prompt(
                            &mut stdout,
                            &app,
                            &mut ui,
                            vec![TranscriptLine {
                                text: format!("Event error: {err}"),
                                kind: TranscriptKind::Error,
                            }],
                        )?;
                    }
                    None => app.should_quit = true,
                }
            }
            maybe_msg = msg_rx.recv() => {
                match maybe_msg {
                    Some(msg) => {
                        let refreshed_events =
                            handle_message(msg, &mut app, &mut stdout, &mut ui)?;
                        if refreshed_events {
                            event_stream = EventStream::new();
                        }
                    }
                    None => app.should_quit = true,
                }
            }
            _ = tick.tick() => {
                if app.ctrl_c_pending
                    && app
                        .ctrl_c_at
                        .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2))
                {
                    app.ctrl_c_pending = false;
                    app.ctrl_c_at = None;
                    if !app.suspended {
                        clear_live_area(&mut stdout, &ui)?;
                        redraw_live_area(&mut stdout, &app, &mut ui)?;
                    }
                }
            }
            changed = state_rx.changed() => {
                if changed.is_ok() {
                    if let Some(watched) = state_rx.borrow_and_update().clone() {
                        let supervisor_status = app.status.supervisor_status.clone();
                        let executor_status = app.status.executor_status.clone();
                        let directory = app.status.directory.clone();
                        let branch = app.status.branch.clone();
                        let mut next = StatusSnapshot::from_watched_state(&watched);
                        next.supervisor_status = supervisor_status;
                        next.executor_status = executor_status;
                        next.directory = directory;
                        next.branch = branch;
                        app.status = next;
                        if !app.suspended {
                            clear_live_area(&mut stdout, &ui)?;
                            redraw_live_area(&mut stdout, &app, &mut ui)?;
                        }
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    clear_live_area(&mut stdout, &ui)?;
    queue!(stdout, MoveToColumn(0))?;
    crlf(&mut stdout)?;
    stdout.flush()?;
    save_history(&app.history);
    leave_tui()?;
    Ok(())
}

fn handle_event(
    event: Event,
    app: &mut App,
    cmd_tx: &mpsc::UnboundedSender<String>,
    stdout: &mut Stdout,
    ui: &mut TerminalUi,
) -> Result<()> {
    if app.suspended {
        return Ok(());
    }

    match event {
        Event::Resize(_, _) => {
            clear_live_area(stdout, ui)?;
            redraw_live_area(stdout, app, ui)?;
        }
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }

            if app.confirmation.is_some() {
                handle_confirmation_key(key, app);
            } else {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        if app.ctrl_c_pending {
                            app.should_quit = true;
                        } else {
                            app.ctrl_c_pending = true;
                            app.ctrl_c_at = Some(std::time::Instant::now());
                        }
                    }
                    (KeyCode::Char('l'), KeyModifiers::CONTROL) => {}
                    (KeyCode::Char('a'), KeyModifiers::CONTROL) | (KeyCode::Home, _) => {
                        app.move_home()
                    }
                    (KeyCode::Char('e'), KeyModifiers::CONTROL) | (KeyCode::End, _) => {
                        app.move_end()
                    }
                    (KeyCode::Left, _) => app.move_left(),
                    (KeyCode::Right, _) => app.move_right(),
                    (KeyCode::Up, _) => app.history_up(),
                    (KeyCode::Down, _) => app.history_down(),
                    (KeyCode::Backspace, _) => app.delete_before_cursor(),
                    (KeyCode::Delete, _) => app.delete_after_cursor(),
                    (KeyCode::Esc, _) => {
                        if app.completion_popup_visible() {
                            app.hide_completion_popup();
                        } else {
                            app.input.clear();
                            app.cursor_pos = 0;
                            app.history_idx = None;
                            app.history_saved.clear();
                        }
                    }
                    (KeyCode::Tab, _) => app.next_completion(),
                    (KeyCode::BackTab, _) => app.previous_completion(),
                    // Some Linux terminals report Shift+Enter as an ESC-prefixed Enter
                    // sequence, which crossterm surfaces as Alt+Enter via its fallback parser.
                    (KeyCode::Enter, modifiers) if is_multiline_enter(modifiers) => {
                        app.insert_newline()
                    }
                    (KeyCode::Enter, _) => {
                        if app.completion_popup_visible() {
                            app.accept_completion_and_submit(cmd_tx);
                        } else {
                            app.submit_input(cmd_tx);
                        }
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        app.insert_char(ch)
                    }
                    _ => {}
                }
            }

            if !app.should_quit {
                clear_live_area(stdout, ui)?;
                redraw_live_area(stdout, app, ui)?;
            }
        }
        _ => {}
    }

    Ok(())
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
    stdout: &mut Stdout,
    ui: &mut TerminalUi,
) -> Result<bool> {
    match msg {
        UiMessage::Info(text) => {
            let lines = split_transcript(&text, TranscriptKind::Info);
            app.messages.extend(lines.clone());
            print_message_and_restore_prompt(stdout, app, ui, lines)?;
        }
        UiMessage::Error(text) => {
            let lines = split_transcript(&text, TranscriptKind::Error);
            app.messages.extend(lines.clone());
            print_message_and_restore_prompt(stdout, app, ui, lines)?;
        }
        UiMessage::Transition { from, to } => {
            let line = TranscriptLine {
                text: match from {
                    Some(from) => format!("── {from} → {to} ──"),
                    None => format!("── {to} ──"),
                },
                kind: TranscriptKind::Transition,
            };
            app.messages.push(line.clone());
            print_message_and_restore_prompt(stdout, app, ui, vec![line])?;
        }
        UiMessage::StatusUpdate(status) => {
            let mut next = status;
            if next.directory.is_empty() {
                next.directory = app.status.directory.clone();
            }
            if next.branch.is_none() {
                next.branch = app.status.branch.clone();
            }
            app.status = next;
            if !app.suspended {
                clear_live_area(stdout, ui)?;
                redraw_live_area(stdout, app, ui)?;
            }
        }
        UiMessage::Suspend { ack } => {
            clear_live_area(stdout, ui)?;
            queue!(stdout, MoveToColumn(0))?;
            stdout.flush()?;
            leave_tui()?;
            app.suspended = true;
            let _ = ack.send(());
            return Ok(false);
        }
        UiMessage::Resume => {
            enter_tui()?;
            flush_stdin_input_buffer();
            app.suspended = false;
            ui.prompt_visible = false;
            ui.prompt_lines = 0;
            ui.prompt_cursor_row = 0;
            ui.lower_lines = 0;
            redraw_live_area(stdout, app, ui)?;
            return Ok(true);
        }
        UiMessage::ConfirmationRequest { prompt, reply } => {
            app.confirmation = Some(ConfirmationState { prompt, reply });
            if !app.suspended {
                clear_live_area(stdout, ui)?;
                redraw_live_area(stdout, app, ui)?;
            }
        }
    }
    Ok(false)
}

// The transcript is real terminal output; only the prompt area is ephemeral.
fn print_startup_header(stdout: &mut Stdout, startup: &StartupHeader) -> Result<()> {
    queue!(stdout, Print("\r\n"))?;
    let width = terminal_width() as usize;

    for line in ferrus_logo_lines() {
        print_logo_line(stdout, line, width)?;
        crlf(stdout)?;
    }
    crlf(stdout)?;

    let meta_lines = startup_metadata_lines(startup);
    print_metadata_box(stdout, &meta_lines, width)?;
    crlf(stdout)?;
    crlf(stdout)?;

    print_tip_line(
        stdout,
        "Tip: /plan to start a task · /status to check state · /help for all commands",
        width,
    )?;
    crlf(stdout)?;
    stdout.flush()?;
    Ok(())
}

fn print_metadata_box(stdout: &mut Stdout, lines: &[TranscriptLine], width: usize) -> Result<()> {
    let inner_width = metadata_inner_width(lines, width);
    let border = "─".repeat(inner_width + 2);
    queue!(
        stdout,
        Print("  "),
        PrintStyledContent(style(format!("╭{border}╮")).with(Color::DarkGrey))
    )?;
    crlf(stdout)?;

    for line in lines {
        queue!(
            stdout,
            Print("  "),
            PrintStyledContent(style("│ ").with(Color::DarkGrey))
        )?;
        print_meta_line(stdout, line, inner_width)?;
        let visible = truncate_to_width(&line.text, inner_width);
        let padding = inner_width.saturating_sub(visible.chars().count());
        if padding > 0 {
            queue!(stdout, Print(" ".repeat(padding)))?;
        }
        queue!(
            stdout,
            PrintStyledContent(style(" │").with(Color::DarkGrey))
        )?;
        crlf(stdout)?;
    }

    queue!(
        stdout,
        Print("  "),
        PrintStyledContent(style(format!("╰{border}╯")).with(Color::DarkGrey))
    )?;
    Ok(())
}

fn clear_live_area(stdout: &mut Stdout, ui: &TerminalUi) -> Result<()> {
    if !ui.prompt_visible || ui.lower_lines == 0 {
        return Ok(());
    }
    let lower_lines = ui.lower_lines;
    let total_lines = lower_lines + ui.prompt_lines + 2;

    queue!(stdout, MoveUp(ui.prompt_cursor_row + 1), MoveToColumn(0))?;
    for idx in 0..total_lines {
        queue!(stdout, Clear(ClearType::UntilNewLine), MoveToColumn(0))?;
        if idx + 1 < total_lines {
            queue!(stdout, MoveDown(1), MoveToColumn(0))?;
        }
    }
    queue!(stdout, MoveUp(total_lines - 1), MoveToColumn(0))?;
    stdout.flush()?;
    Ok(())
}

fn redraw_live_area(stdout: &mut Stdout, app: &App, ui: &mut TerminalUi) -> Result<()> {
    let width = terminal_width() as usize;
    let lower_lines = render_lower_live_area(app, width);
    print_live_area_border(stdout, width)?;
    let (prompt_lines, prompt_cursor_row, prompt_cursor_col) =
        if let Some(confirm) = app.confirmation.as_ref() {
            crlf(stdout)?;
            let prompt_text = truncate_to_width(&confirm.prompt, width.max(1));
            queue!(
                stdout,
                MoveToColumn(0),
                Clear(ClearType::UntilNewLine),
                Print(prompt_text.clone()),
                Print(" [y/N]")
            )?;
            (1, 0, prompt_text.chars().count() as u16 + 6)
        } else {
            let prompt = render_prompt(app, width);
            for (idx, line) in prompt.lines.iter().enumerate() {
                crlf(stdout)?;
                queue!(stdout, MoveToColumn(0), Clear(ClearType::UntilNewLine))?;
                if idx == 0 {
                    queue!(stdout, Print("> "), Print(line.clone()))?;
                } else {
                    queue!(stdout, Print("  "), Print(line.clone()))?;
                }
            }
            (
                prompt.lines.len() as u16,
                prompt.cursor_row,
                prompt.cursor_col,
            )
        };

    crlf(stdout)?;
    queue!(stdout, MoveToColumn(0), Clear(ClearType::UntilNewLine))?;
    print_live_area_border(stdout, width)?;
    for line in &lower_lines {
        crlf(stdout)?;
        queue!(stdout, MoveToColumn(0), Clear(ClearType::UntilNewLine))?;
        print_live_area_line(
            stdout,
            line,
            app.ctrl_c_pending,
            &app.status,
            app.debug,
            width,
        )?;
    }
    queue!(
        stdout,
        MoveUp(lower_lines.len() as u16 + 1 + (prompt_lines - 1 - prompt_cursor_row)),
        MoveToColumn(prompt_cursor_col)
    )?;
    ui.prompt_visible = true;
    ui.prompt_lines = prompt_lines;
    ui.prompt_cursor_row = prompt_cursor_row;
    ui.lower_lines = lower_lines.len() as u16;
    stdout.flush()?;
    Ok(())
}

fn print_message_and_restore_prompt(
    stdout: &mut Stdout,
    app: &App,
    ui: &mut TerminalUi,
    lines: Vec<TranscriptLine>,
) -> Result<()> {
    clear_live_area(stdout, ui)?;
    queue!(stdout, MoveToColumn(0), Clear(ClearType::UntilNewLine))?;
    for line in &lines {
        print_transcript_line(stdout, line)?;
    }
    ui.prompt_visible = false;
    ui.prompt_lines = 0;
    ui.prompt_cursor_row = 0;
    ui.lower_lines = 0;
    redraw_live_area(stdout, app, ui)
}

fn enter_tui() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let _ = queue!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    );
    let _ = stdout.flush();
    Ok(())
}

fn leave_tui() -> Result<()> {
    let mut stdout = io::stdout();
    let _ = queue!(stdout, PopKeyboardEnhancementFlags);
    let _ = stdout.flush();
    disable_raw_mode()?;
    Ok(())
}

fn flush_stdin_input_buffer() {
    #[cfg(unix)]
    // SAFETY: tcflush is a libc call that discards bytes queued on stdin. We ignore
    // errors because some environments do not expose a flushable TTY.
    unsafe {
        let _ = libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }

    // Some agents restore the terminal by writing ANSI sequences as they exit.
    // Those bytes can already be decoded into crossterm events, or arrive just
    // after raw mode is re-enabled. Drain until the terminal stays quiet briefly.
    const QUIET_WINDOW: Duration = Duration::from_millis(20);
    const MAX_DRAIN_TIME: Duration = Duration::from_millis(150);

    let deadline = Instant::now() + MAX_DRAIN_TIME;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }

        let timeout = deadline.saturating_duration_since(now).min(QUIET_WINDOW);
        match event::poll(timeout) {
            Ok(true) => {
                while matches!(event::poll(Duration::ZERO), Ok(true)) {
                    if event::read().is_err() {
                        break;
                    }
                }
            }
            Ok(false) | Err(_) => break,
        }
    }
}

fn print_transcript_line(stdout: &mut Stdout, line: &TranscriptLine) -> Result<()> {
    match line.kind {
        TranscriptKind::Info => {
            queue!(stdout, MoveToColumn(0), Print(&line.text))?;
            crlf(stdout)?;
        }
        TranscriptKind::Error => {
            queue!(
                stdout,
                MoveToColumn(0),
                PrintStyledContent(
                    style(&line.text)
                        .with(Color::Red)
                        .attribute(Attribute::Bold)
                ),
            )?;
            crlf(stdout)?;
        }
        TranscriptKind::Transition => {
            queue!(
                stdout,
                MoveToColumn(0),
                PrintStyledContent(
                    style(&line.text)
                        .with(Color::Rgb {
                            r: 210,
                            g: 100,
                            b: 10,
                        })
                        .attribute(Attribute::Bold)
                ),
            )?;
            crlf(stdout)?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn print_logo_line(stdout: &mut Stdout, line: &str, width: usize) -> Result<()> {
    let line = truncate_to_width(line, width.max(1));
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len().max(1);
    queue!(stdout, Print("  "))?;
    for (idx, ch) in chars.into_iter().enumerate() {
        queue!(
            stdout,
            PrintStyledContent(
                style(ch.to_string())
                    .with(logo_gradient_color(idx, len))
                    .attribute(Attribute::Bold)
            )
        )?;
    }
    Ok(())
}

fn print_meta_line(stdout: &mut Stdout, line: &TranscriptLine, width: usize) -> Result<()> {
    let content = truncate_to_width(&line.text, width.max(1));
    let label_end = content.find(' ').unwrap_or(content.len());
    let (label, rest) = content.split_at(label_end.min(content.len()));

    queue!(
        stdout,
        PrintStyledContent(style(label).with(Color::DarkGrey))
    )?;

    if let Some(agent) = rest.strip_prefix(" claude-code ") {
        queue!(
            stdout,
            PrintStyledContent(style(" claude-code").with(Color::Rgb {
                r: 210,
                g: 100,
                b: 10,
            })),
            Print(" "),
            PrintStyledContent(style(agent).with(Color::Grey)),
        )?;
    } else if let Some(agent) = rest.strip_prefix(" codex ") {
        queue!(
            stdout,
            PrintStyledContent(style(" codex").with(Color::Rgb {
                r: 210,
                g: 100,
                b: 10,
            })),
            Print(" "),
            PrintStyledContent(style(agent).with(Color::Grey)),
        )?;
    } else if label == "version:" {
        queue!(
            stdout,
            PrintStyledContent(style(rest).with(Color::Rgb {
                r: 198,
                g: 190,
                b: 176,
            }))
        )?;
    } else {
        queue!(stdout, PrintStyledContent(style(rest).with(Color::White)))?;
    }

    Ok(())
}

fn print_tip_line(stdout: &mut Stdout, tip: &str, width: usize) -> Result<()> {
    let mut remaining = width.max(1);
    let mut first = true;

    for part in tip.split(' ') {
        let sep = usize::from(!first);
        if remaining <= sep {
            break;
        }
        let part = truncate_to_width(part, remaining - sep);
        if part.is_empty() {
            break;
        }
        let part_len = part.chars().count();
        if !first {
            queue!(stdout, Print(" "))?;
            remaining = remaining.saturating_sub(1);
        }
        first = false;
        if part.starts_with('/') {
            queue!(
                stdout,
                PrintStyledContent(style(part).with(Color::Rgb {
                    r: 210,
                    g: 100,
                    b: 10,
                }))
            )?;
        } else if part == "Tip:" {
            queue!(
                stdout,
                PrintStyledContent(style(part).with(Color::DarkGrey))
            )?;
        } else {
            queue!(stdout, PrintStyledContent(style(part).with(Color::Grey)))?;
        }
        remaining = remaining.saturating_sub(part_len);
    }

    crlf(stdout)?;
    Ok(())
}

fn crlf(stdout: &mut Stdout) -> Result<()> {
    queue!(stdout, Print("\r\n"))?;
    Ok(())
}

fn ferrus_logo_lines() -> &'static [&'static str] {
    &[
        "███████  ███████  █████   █████   ██   ██  ███████",
        "██       ██       ██  ██  ██  ██  ██   ██  ██",
        "█████    █████    █████   █████   ██   ██  ███████",
        "██       ██       ██  ██  ██  ██  ██   ██       ██",
        "██       ███████  ██  ██  ██  ██   █████   ███████",
    ]
}

fn metadata_inner_width(lines: &[TranscriptLine], width: usize) -> usize {
    let max_visible = width.saturating_sub(6).max(1);
    lines
        .iter()
        .map(|line| truncate_to_width(&line.text, max_visible).chars().count())
        .max()
        .unwrap_or(1)
}

fn logo_gradient_color(idx: usize, len: usize) -> Color {
    let start = (148u8, 36u8, 20u8);
    let end = (226u8, 128u8, 18u8);
    let t = if len <= 1 {
        0.0
    } else {
        idx as f32 / (len.saturating_sub(1)) as f32
    };
    let mix = |a: u8, b: u8| -> u8 { (a as f32 + (b as f32 - a as f32) * t).round() as u8 };
    Color::Rgb {
        r: mix(start.0, end.0),
        g: mix(start.1, end.1),
        b: mix(start.2, end.2),
    }
}

fn startup_metadata_lines(startup: &StartupHeader) -> Vec<TranscriptLine> {
    vec![
        TranscriptLine {
            text: format!("version: {}", startup.version),
            kind: TranscriptKind::Info,
        },
        TranscriptLine {
            text: startup_agent_line(
                "supervisor:",
                &startup.supervisor_type,
                &startup.supervisor_version,
            ),
            kind: TranscriptKind::Info,
        },
        TranscriptLine {
            text: startup_agent_line(
                "executor:",
                &startup.executor_type,
                &startup.executor_version,
            ),
            kind: TranscriptKind::Info,
        },
    ]
}

fn startup_agent_line(label: &str, agent_type: &str, version: &str) -> String {
    if version.is_empty() {
        format!("{label} {agent_type}")
    } else {
        format!("{label} {agent_type} {version}")
    }
}

struct PromptLine {
    lines: Vec<String>,
    cursor_row: u16,
    cursor_col: u16,
}

fn render_prompt(app: &App, width: usize) -> PromptLine {
    let available = width.saturating_sub(2).max(1);
    let chars: Vec<char> = app.input.chars().collect();
    let mut lines = Vec::new();
    let mut cursor_row = 0u16;
    let mut cursor_col = 2u16;
    let mut line = String::new();
    let mut line_width = 0usize;
    let mut row = 0u16;

    for (idx, ch) in chars.iter().enumerate() {
        if idx == app.cursor_pos {
            cursor_row = row;
            cursor_col = 2 + line_width as u16;
        }

        if *ch == '\n' {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
            row += 1;
            continue;
        }

        if line_width == available {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
            row += 1;
            if idx == app.cursor_pos {
                cursor_row = row;
                cursor_col = 2;
            }
        }

        line.push(*ch);
        line_width += 1;
    }

    if app.cursor_pos == chars.len() {
        cursor_row = row;
        cursor_col = 2 + line_width as u16;
    }

    lines.push(line);
    PromptLine {
        lines,
        cursor_row,
        cursor_col,
    }
}

fn print_status_line(
    stdout: &mut Stdout,
    status: &StatusSnapshot,
    ctrl_c_pending: bool,
    debug: bool,
    width: usize,
) -> Result<()> {
    let max_width = width.max(1);
    if ctrl_c_pending {
        let warning = truncate_to_width("Press Ctrl+C again to exit", max_width);
        queue!(
            stdout,
            PrintStyledContent(style(warning).with(Color::Yellow))
        )?;
        return Ok(());
    }

    let state = if status.task_state.is_empty() {
        "Idle".to_string()
    } else {
        status.task_state.clone()
    };
    let mut segments = vec![(state, task_state_color(&status.task_state))];

    if let Some(elapsed) = status
        .task_state_detail
        .strip_prefix(&format!("{} (", status.task_state))
        .and_then(|detail| detail.strip_suffix(')'))
    {
        segments.push((format!(" ({elapsed})"), Color::DarkGrey));
    }

    if !status.directory.is_empty() {
        segments.push((" | ".to_string(), Color::DarkGrey));
        segments.push(("directory: ".to_string(), Color::DarkGrey));
        segments.push((status.directory.clone(), Color::Grey));
    }

    if let Some(branch) = status.branch.as_deref() {
        segments.push((" | ".to_string(), Color::DarkGrey));
        segments.push(("branch: ".to_string(), Color::DarkGrey));
        segments.push((branch.to_string(), Color::Grey));
    }

    segments.push((" | ".to_string(), Color::DarkGrey));
    segments.push(("retries: ".to_string(), Color::DarkGrey));
    segments.push((status.retries.to_string(), Color::Grey));
    segments.push((" | ".to_string(), Color::DarkGrey));
    segments.push(("cycles: ".to_string(), Color::DarkGrey));
    segments.push((status.cycles.to_string(), Color::Grey));

    let mut remaining = max_width;
    let mut left_width = 0;
    for (text, color) in segments {
        if remaining == 0 {
            break;
        }
        let visible = truncate_to_width(&text, remaining);
        if visible.is_empty() {
            break;
        }
        queue!(
            stdout,
            PrintStyledContent(style(visible.clone()).with(color))
        )?;
        let visible_width = display_width(&visible);
        left_width += visible_width;
        remaining = remaining.saturating_sub(visible_width);
    }

    // When the executor is waiting for a human answer, show a prominent hint.
    if status.task_state == "AwaitingHuman" {
        let hint = "  ← type your answer and press Enter";
        let hint_text = truncate_to_width(hint, remaining);
        if !hint_text.is_empty() {
            queue!(
                stdout,
                PrintStyledContent(
                    style(hint_text.clone())
                        .with(Color::Magenta)
                        .attribute(Attribute::Bold)
                )
            )?;
            let hint_width = display_width(&hint_text);
            left_width += hint_width;
            remaining = remaining.saturating_sub(hint_width);
        }
    } else if status.task_state == "Consultation" {
        let hint = "  ← consulting supervisor";
        let hint_text = truncate_to_width(hint, remaining);
        if !hint_text.is_empty() {
            queue!(
                stdout,
                PrintStyledContent(style(hint_text.clone()).with(Color::Cyan))
            )?;
            let hint_width = display_width(&hint_text);
            left_width += hint_width;
            remaining = remaining.saturating_sub(hint_width);
        }
    }

    if debug && remaining >= 7 {
        let pad = max_width.saturating_sub(left_width + 5);
        if pad > 0 {
            queue!(stdout, Print(" ".repeat(pad)))?;
        }
        queue!(
            stdout,
            PrintStyledContent(style("debug").with(Color::DarkBlue))
        )?;
    }

    Ok(())
}

fn print_live_area_border(stdout: &mut Stdout, width: usize) -> Result<()> {
    let border_width = width.max(1);
    queue!(
        stdout,
        PrintStyledContent(style("─".repeat(border_width)).with(Color::DarkGrey))
    )?;
    Ok(())
}

enum LiveAreaLine {
    Status,
    Completion {
        selected: bool,
        command: String,
        description: String,
    },
}

fn render_lower_live_area(app: &App, width: usize) -> Vec<LiveAreaLine> {
    if app.completion_popup_visible() {
        visible_completion_rows(app)
            .into_iter()
            .map(
                |(selected, command, description)| LiveAreaLine::Completion {
                    selected,
                    command: truncate_to_width(command, width.max(1)),
                    description: truncate_to_width(description, width.max(1)),
                },
            )
            .collect()
    } else {
        vec![LiveAreaLine::Status]
    }
}

fn visible_completion_rows(app: &App) -> Vec<(bool, &'static str, &'static str)> {
    let total = app.completion_candidates.len();
    if total == 0 {
        return Vec::new();
    }
    let window = total.min(3);
    let start = if total <= window {
        0
    } else {
        app.completion_selected.min(total.saturating_sub(window))
    };
    app.completion_candidates[start..start + window]
        .iter()
        .enumerate()
        .map(|(offset, (cmd, desc))| (start + offset == app.completion_selected, *cmd, *desc))
        .collect()
}

fn print_live_area_line(
    stdout: &mut Stdout,
    line: &LiveAreaLine,
    ctrl_c_pending: bool,
    status: &StatusSnapshot,
    debug: bool,
    width: usize,
) -> Result<()> {
    match line {
        LiveAreaLine::Status => print_status_line(stdout, status, ctrl_c_pending, debug, width),
        LiveAreaLine::Completion {
            selected,
            command,
            description,
        } => print_completion_line(stdout, *selected, command, description, width),
    }
}

fn print_completion_line(
    stdout: &mut Stdout,
    selected: bool,
    command: &str,
    description: &str,
    width: usize,
) -> Result<()> {
    let marker = if selected { "› " } else { "  " };
    let command_width = command.chars().count();
    let separator = if description.is_empty() { "" } else { "  " };
    let used = marker.chars().count() + command_width + separator.chars().count();
    let desc_width = width.saturating_sub(used).max(1);
    let desc = truncate_to_width(description, desc_width);

    if selected {
        queue!(
            stdout,
            PrintStyledContent(style(marker).with(Color::Yellow)),
            PrintStyledContent(
                style(command)
                    .with(Color::Yellow)
                    .attribute(Attribute::Bold)
            )
        )?;
    } else {
        queue!(
            stdout,
            PrintStyledContent(style(marker).with(Color::DarkGrey)),
            PrintStyledContent(style(command).with(Color::Grey))
        )?;
    }

    if !desc.is_empty() {
        queue!(
            stdout,
            PrintStyledContent(style(separator).with(Color::DarkGrey)),
            PrintStyledContent(style(desc).with(Color::DarkGrey))
        )?;
    }
    Ok(())
}

fn task_state_color(task_state: &str) -> Color {
    match task_state {
        "Idle" => Color::DarkGrey,
        "Executing" | "Addressing" | "Checking" => Color::Yellow,
        "Consultation" => Color::Blue,
        "Reviewing" => Color::Cyan,
        "Complete" => Color::Green,
        "Failed" => Color::Red,
        "AwaitingHuman" => Color::Magenta,
        _ => Color::White,
    }
}

fn split_transcript(text: &str, kind: TranscriptKind) -> Vec<TranscriptLine> {
    let mut lines = Vec::new();
    for line in text.lines() {
        lines.push(TranscriptLine {
            text: line.to_string(),
            kind,
        });
    }
    if lines.is_empty() {
        lines.push(TranscriptLine {
            text: String::new(),
            kind,
        });
    }
    lines
}

fn terminal_width() -> u16 {
    size().map(|(w, _)| w).unwrap_or(80)
}

fn truncate_to_width(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn display_width(text: &str) -> usize {
    text.chars().count()
}

fn is_multiline_enter(modifiers: KeyModifiers) -> bool {
    let multiline = KeyModifiers::SHIFT | KeyModifiers::ALT;
    let disallowed =
        KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META;
    modifiers.intersects(multiline) && !modifiers.intersects(disallowed)
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

fn current_git_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
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

#[cfg(test)]
mod tui_tests {
    use super::*;

    #[test]
    fn first_tab_on_multiple_matches_selects_first_candidate() {
        let mut app = App::new();
        app.input = "/".into();
        app.cursor_pos = app.input.len();

        app.next_completion();

        assert!(app.completion_active);
        assert_eq!(app.completion_selected, 0);
        assert_eq!(app.completion_candidates[0].0, "/plan");
    }

    #[test]
    fn tab_extends_to_shared_prefix_before_cycling() {
        let mut app = App::new();
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
            vec!["/resume", "/review", "/reset", "/register"]
        );
    }

    #[test]
    fn abbreviate_home_replaces_home_prefix() {
        let path = Path::new("/home/user/Repos/ferrus");
        assert_eq!(abbreviate_home(path), "~/Repos/ferrus");
    }

    #[test]
    fn typing_slash_command_updates_context_without_tab() {
        let mut app = App::new();

        app.insert_char('/');
        app.insert_char('s');

        assert!(app.has_command_context());
        assert_eq!(
            app.completion_candidates
                .iter()
                .map(|(cmd, _)| *cmd)
                .collect::<Vec<_>>(),
            vec!["/supervisor", "/status", "/stop"]
        );
        assert!(!app.completion_active);
    }

    #[test]
    fn autocomplete_includes_new_hq_commands_and_omits_execute() {
        let commands: Vec<&str> = COMMANDS.iter().map(|(cmd, _)| *cmd).collect();

        assert!(commands.contains(&"/task"));
        assert!(commands.contains(&"/model"));
        assert!(commands.contains(&"/resume"));
        assert!(commands.contains(&"/supervisor"));
        assert!(commands.contains(&"/executor"));
        assert!(!commands.contains(&"/execute"));
    }

    #[test]
    fn render_prompt_wraps_multiline_input() {
        let mut app = App::new();
        app.input = "abcd\nef".into();
        app.cursor_pos = app.input.chars().count();

        let prompt = render_prompt(&app, 6);

        assert_eq!(prompt.lines, vec!["abcd", "ef"]);
        assert_eq!(prompt.cursor_row, 1);
        assert_eq!(prompt.cursor_col, 4);
    }

    #[test]
    fn render_prompt_preserves_trailing_newline() {
        let mut app = App::new();
        app.input = "abcd\n".into();
        app.cursor_pos = app.input.chars().count();

        let prompt = render_prompt(&app, 10);

        assert_eq!(prompt.lines, vec!["abcd", ""]);
        assert_eq!(prompt.cursor_row, 1);
        assert_eq!(prompt.cursor_col, 2);
    }

    #[test]
    fn multiline_submission_does_not_enter_history() {
        let mut app = App::new();
        let original_history_len = app.history.len();
        app.input = "first\nsecond".into();
        app.cursor_pos = app.input.chars().count();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        app.submit_input(&cmd_tx);

        assert_eq!(cmd_rx.try_recv().unwrap(), "first\nsecond");
        assert_eq!(app.history.len(), original_history_len);
    }

    #[test]
    fn multiline_enter_accepts_shift_and_alt_enter() {
        assert!(is_multiline_enter(KeyModifiers::SHIFT));
        assert!(is_multiline_enter(KeyModifiers::ALT));
        assert!(is_multiline_enter(KeyModifiers::SHIFT | KeyModifiers::ALT));
        assert!(!is_multiline_enter(KeyModifiers::NONE));
        assert!(!is_multiline_enter(KeyModifiers::CONTROL));
        assert!(!is_multiline_enter(
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ));
        assert!(!is_multiline_enter(
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ));
        assert!(!is_multiline_enter(KeyModifiers::SUPER | KeyModifiers::ALT));
    }
}
