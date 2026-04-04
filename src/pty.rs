#![allow(dead_code)]

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

// ── ANSI stripping ─────────────────────────────────────────────────────────────

/// Strip ANSI/CSI/OSC escape sequences and normalise `\r\n` → `\n`.
///
/// Used when replaying PTY log bytes in cooked mode so that sequences like
/// `\033[?1049h` (enter alternate screen) don't corrupt the terminal before we
/// enter raw mode.
fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            0x1B => {
                i += 1;
                if i >= input.len() {
                    break;
                }
                match input[i] {
                    // CSI: ESC [ … final-byte (0x40–0x7E)
                    b'[' => {
                        i += 1;
                        while i < input.len() && !(0x40..=0x7E).contains(&input[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                    // OSC: ESC ] … ST (BEL or ESC \)
                    b']' => {
                        i += 1;
                        while i < input.len() {
                            if input[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if input[i] == 0x1B && i + 1 < input.len() && input[i + 1] == b'\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    // Any other ESC-X two-byte sequence: skip the next byte.
                    _ => {
                        i += 1;
                    }
                }
            }
            // \r\n → emit just \n; lone \r → \n
            b'\r' => {
                if i + 1 < input.len() && input[i + 1] == b'\n' {
                    i += 1; // skip \r; \n emitted on next iteration
                } else {
                    out.push(b'\n');
                    i += 1;
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    out
}

// ── Prefix-key FSM ─────────────────────────────────────────────────────────────

/// Detach prefix key: Ctrl+] (0x1D, ASCII GS).
/// Chosen to avoid conflicts with tmux (Ctrl+B), readline (Ctrl+A/E/B/F), and Claude Code.
const PREFIX_KEY: u8 = 0x1D;

#[derive(Debug, Clone, PartialEq)]
pub enum PrefixKeyState {
    Normal,
    /// Received PREFIX_KEY; waiting to see if next byte is 'd' (detach) or another PREFIX_KEY
    /// (pass-through), or something else (forward both).
    GotPrefix,
}

#[derive(Debug, PartialEq)]
pub enum RelayDecision {
    /// Bytes to forward to the PTY (may be empty — swallowed prefix char).
    Forward(Vec<u8>),
    /// Ctrl+] d was detected — caller should detach.
    Detach,
}

/// Pure function: process one byte through the Ctrl+] d FSM.
/// Ctrl+] Ctrl+] → forward a literal Ctrl+] (escape hatch).
pub fn process_byte(byte: u8, state: &mut PrefixKeyState) -> RelayDecision {
    match (&*state, byte) {
        (PrefixKeyState::Normal, b) if b == PREFIX_KEY => {
            *state = PrefixKeyState::GotPrefix;
            RelayDecision::Forward(vec![]) // swallow until we know intent
        }
        (PrefixKeyState::GotPrefix, b'd') => {
            *state = PrefixKeyState::Normal;
            RelayDecision::Detach
        }
        (PrefixKeyState::GotPrefix, b) if b == PREFIX_KEY => {
            // Ctrl+] Ctrl+] → forward a literal Ctrl+]
            *state = PrefixKeyState::Normal;
            RelayDecision::Forward(vec![PREFIX_KEY])
        }
        (PrefixKeyState::GotPrefix, other) => {
            // Unknown sequence → forward Ctrl+] + the key verbatim
            *state = PrefixKeyState::Normal;
            RelayDecision::Forward(vec![PREFIX_KEY, other])
        }
        // Use `b` not `byte` to avoid shadowing the function parameter of the same name.
        (PrefixKeyState::Normal, b) => RelayDecision::Forward(vec![b]),
    }
}

// ── Stdin relay ────────────────────────────────────────────────────────────────
//
// The relay reads stdin and forwards bytes to the PTY writer, intercepting the
// Ctrl+] d detach sequence.
//
// On Unix we use libc::poll + libc::read directly on fd 0, bypassing Rust's
// BufReader<StdinRaw> and the global stdin Mutex.  This is critical: if we held
// stdin.lock() for the relay's lifetime, rustyline would be starved after
// attach() returns — the blocking thread keeps the mutex until the next keypress,
// causing every subsequent character to require multiple attempts.
//
// A self-pipe provides instant cancellation: writing one byte to pipe_write wakes
// poll() without waiting for stdin input.
//
// On non-Unix we fall back to stdin.lock() (broken-typing bug remains on that platform).

/// Token returned by `spawn_stdin_relay` that lets the caller cancel the relay.
///
/// Call `signal_and_wait` to wake the relay and wait for it to exit.
/// Dropping the token without signalling is safe — the relay will exit on
/// the next keypress (Unix: when the pipe read-end is closed).
struct RelayCancel {
    #[cfg(unix)]
    pipe_write: std::os::unix::io::RawFd,
}

impl RelayCancel {
    /// Write the cancel signal and wait up to `timeout_ms` for the relay to exit.
    ///
    /// Must only be called when the relay's JoinHandle has NOT been awaited yet
    /// (i.e. the ProcessExit arm of the select fired, not the relay arm).
    async fn signal_and_wait(self, relay: tokio::task::JoinHandle<DetachReason>, timeout_ms: u64) {
        #[cfg(unix)]
        {
            let b: u8 = 0;
            unsafe {
                libc::write(self.pipe_write, &b as *const u8 as *const libc::c_void, 1);
            }
            let _ = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), relay).await;
            // pipe_write closed by Drop
        }
        #[cfg(not(unix))]
        {
            let _ = timeout_ms;
            drop(relay);
        }
    }
}

#[cfg(unix)]
impl Drop for RelayCancel {
    fn drop(&mut self) {
        unsafe { libc::close(self.pipe_write) };
    }
}

/// Spawn the stdin relay thread.  Returns the JoinHandle and a cancel token.
fn spawn_stdin_relay(
    stdin_writer: Arc<Mutex<Box<dyn Write + Send>>>,
) -> Result<(tokio::task::JoinHandle<DetachReason>, RelayCancel)> {
    #[cfg(unix)]
    {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            anyhow::bail!("Failed to create relay cancellation pipe");
        }
        let (pipe_read, pipe_write) = (fds[0], fds[1]);

        let handle = tokio::task::spawn_blocking(move || -> DetachReason {
            let mut state = PrefixKeyState::Normal;
            let mut buf = [0u8; 1];
            loop {
                let mut pfds = [
                    libc::pollfd {
                        fd: 0,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: pipe_read,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // Block until stdin or cancel pipe becomes readable.
                if unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) } <= 0 {
                    unsafe { libc::close(pipe_read) };
                    return DetachReason::ProcessExit;
                }
                // Cancel pipe readable → exit.
                if pfds[1].revents & libc::POLLIN != 0 {
                    unsafe { libc::close(pipe_read) };
                    return DetachReason::ProcessExit;
                }
                // Stdin readable → read one byte.
                if pfds[0].revents & libc::POLLIN != 0 {
                    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, 1) };
                    if n <= 0 {
                        unsafe { libc::close(pipe_read) };
                        return DetachReason::ProcessExit;
                    }
                    match process_byte(buf[0], &mut state) {
                        RelayDecision::Forward(bytes) if !bytes.is_empty() => {
                            let mut w = stdin_writer.lock().unwrap();
                            if w.write_all(&bytes).is_err() {
                                unsafe { libc::close(pipe_read) };
                                return DetachReason::ProcessExit;
                            }
                        }
                        RelayDecision::Forward(_) => {}
                        RelayDecision::Detach => {
                            unsafe { libc::close(pipe_read) };
                            return DetachReason::UserDetach;
                        }
                    }
                }
            }
        });

        Ok((handle, RelayCancel { pipe_write }))
    }

    #[cfg(not(unix))]
    {
        let handle = tokio::task::spawn_blocking(move || -> DetachReason {
            use std::io::Read;
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            let mut state = PrefixKeyState::Normal;
            let mut buf = [0u8; 1];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => return DetachReason::ProcessExit,
                    Ok(_) => match process_byte(buf[0], &mut state) {
                        RelayDecision::Forward(bytes) if !bytes.is_empty() => {
                            let mut w = stdin_writer.lock().unwrap();
                            if w.write_all(&bytes).is_err() {
                                return DetachReason::ProcessExit;
                            }
                        }
                        RelayDecision::Forward(_) => {}
                        RelayDecision::Detach => return DetachReason::UserDetach,
                    },
                }
            }
        });

        Ok((handle, RelayCancel {}))
    }
}

// ── BackgroundSession ──────────────────────────────────────────────────────────

/// Why attach() returned.
#[derive(Debug, Clone, PartialEq)]
pub enum DetachReason {
    /// User pressed Ctrl+] d.
    UserDetach,
    /// The PTY process exited.
    ProcessExit,
    /// HQ determined the session's work is done (state transitioned out of the active range).
    AutoDetach,
}

/// A live background PTY session.
pub struct BackgroundSession {
    /// Session name (e.g. "executor-1").
    pub name: String,
    /// Write bytes into the agent's stdin.
    pub stdin_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// None = process alive; Some(code) = process exited.
    pub exit_rx: watch::Receiver<Option<i32>>,
    /// Path to the session log file.
    pub log_path: PathBuf,
    /// Swapped to Some(stdout) during /attach, None otherwise.
    pub stdout_sink: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    /// Fire to auto-detach any currently-attached terminal.
    /// Typically signalled by HQ when a state transition indicates the session is done.
    pub force_detach: Arc<tokio::sync::Notify>,
}

/// Spawn `binary args` in a background PTY.  Output streams to `log_path` always;
/// optionally mirrors to a sink set via `stdout_sink` during `/attach`.
pub fn spawn_background(
    binary: &str,
    args: &[&str],
    name: &str,
    log_path: &Path,
) -> Result<BackgroundSession> {
    use std::io::Read;

    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("Failed to open PTY")?;

    let mut cmd = CommandBuilder::new(binary);
    for arg in args {
        cmd.arg(arg);
    }
    // Explicitly set CWD so the child finds project-local config files (e.g. .codex/config.toml).
    // Relying on fork-inherited CWD is not always reliable across portable-pty backends.
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    // Forward TERM so agents that inspect it (e.g. codex) see a real terminal type.
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    cmd.env("TERM", term);

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("Failed to spawn command in PTY")?;
    // Drop slave end so the master gets EOF when the child exits.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("Failed to clone PTY reader")?;
    let stdin_writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .context("Failed to take PTY writer")?,
    ));

    let (exit_tx, exit_rx) = watch::channel::<Option<i32>>(None);
    // NOTE: stdout_sink is best-effort. Races during detach are acceptable:
    // output may be partially written during the attach→None transition.
    // TODO(Phase C): log rotation / retention policy for .ferrus/logs/.
    let stdout_sink: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(None));

    // Drain thread: PTY master → log file always, stdout_sink when attached.
    let sink_drain = Arc::clone(&stdout_sink);
    let log_path_buf = log_path.to_path_buf();
    std::thread::spawn(move || {
        let mut log_file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path_buf)
        {
            Ok(f) => f,
            Err(_) => return,
        };
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = log_file.write_all(&buf[..n]);
                    let _ = log_file.flush();
                    let mut sink = sink_drain.lock().unwrap();
                    if let Some(w) = sink.as_mut() {
                        if w.write_all(&buf[..n]).is_err() || w.flush().is_err() {
                            *sink = None;
                        }
                    }
                }
            }
        }
    });

    // Exit watcher thread: waits for child, signals exit_rx.
    // Note: portable_pty::ExitStatus has exit_code() -> u32, not success().
    // NOTE: this thread blocks on child.wait() indefinitely with no kill or timeout.
    // In Phase C, add explicit cancellation / kill support via a structured lifecycle.
    std::thread::spawn(move || {
        let code = child
            .wait()
            .map(|s| if s.exit_code() == 0 { 0i32 } else { 1i32 })
            .unwrap_or(-1);
        let _ = exit_tx.send(Some(code));
    });

    Ok(BackgroundSession {
        name: name.to_string(),
        stdin_writer,
        exit_rx,
        log_path: log_path.to_path_buf(),
        stdout_sink,
        force_detach: Arc::new(tokio::sync::Notify::new()),
    })
}

impl BackgroundSession {
    /// Returns true if the process is still alive.
    pub fn is_alive(&self) -> bool {
        self.exit_rx.borrow().is_none()
    }

    /// Attach the terminal to this session.
    ///
    /// - Replays the tail of the session log (ANSI-stripped) so the user sees context.
    /// - Enables crossterm raw mode so all keystrokes go to the PTY.
    /// - Spawns a blocking stdin relay with Ctrl+] d interception.
    /// - Returns when the user presses Ctrl+] d or the process exits.
    ///
    /// ## Relay implementation (Unix)
    /// The relay uses `libc::poll` + `libc::read` directly on fd 0, bypassing Rust's
    /// `BufReader<StdinRaw>` and the global stdin `Mutex`.  This is critical: holding
    /// `std::io::stdin().lock()` for the relay's lifetime would starve rustyline after
    /// `attach()` returns — the blocking thread keeps the mutex until the next keypress,
    /// making every subsequent keystroke require multiple attempts.
    ///
    /// A self-pipe provides instant cancellation: writing one byte to `pipe_write` wakes
    /// `poll()` without waiting for stdin input.
    ///
    /// ## Watch channel note
    /// `exit_rx.changed()` only fires for *new* sends after the receiver's last-seen
    /// version. We check for an already-dead process via `borrow()` (which does NOT
    /// advance the seen-version) before cloning, so `changed()` resolves immediately if
    /// the process exited between the guard check and the select.
    pub async fn attach(&self) -> Result<DetachReason> {
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

        // Fast path: process already dead — don't enter raw mode at all.
        if self.exit_rx.borrow().is_some() {
            return Ok(DetachReason::ProcessExit);
        }

        // Clear screen so the agent's TUI fills it cleanly from scratch once output flows.
        // (Replaying the log in cooked mode looked garbled because the raw PTY bytes were
        // intended for an alternate-screen context that we don't have yet.)
        {
            let _ = std::io::stdout().write_all(b"\x1b[2J\x1b[H");
            let _ = std::io::stdout().flush();
        }

        // Enable raw mode BEFORE setting stdout_sink.
        // If enable_raw_mode() fails, stdout_sink is never set — nothing to clean up.
        enable_raw_mode().context("Failed to enable raw mode")?;

        // Route PTY output to our stdout while attached.
        {
            let mut sink = self.stdout_sink.lock().unwrap();
            *sink = Some(Box::new(std::io::stdout()));
        }

        // Clone AFTER borrow() so the clone inherits the pre-exit seen-version.
        // If the process exits between here and the select, changed() fires immediately.
        let mut exit_rx = self.exit_rx.clone();

        let (mut relay, cancel) = match spawn_stdin_relay(Arc::clone(&self.stdin_writer)) {
            Ok(pair) => pair,
            Err(e) => {
                {
                    let mut s = self.stdout_sink.lock().unwrap();
                    *s = None;
                }
                disable_raw_mode().ok();
                return Err(e);
            }
        };

        // Wait for relay to finish, process to exit, or HQ to force-detach.
        //
        // IMPORTANT: capture the relay result via `r` here — don't await relay again
        // after the select since the JoinHandle is consumed on the first poll-to-completion.
        enum SelectOutcome {
            RelayDone(DetachReason),
            ProcessExited,
            ForceDetach,
        }
        let outcome = tokio::select! {
            // relay arm: relay returned on its own (UserDetach or stdin EOF).
            r = &mut relay => SelectOutcome::RelayDone(r.unwrap_or(DetachReason::ProcessExit)),
            // exit arm: PTY process exited.
            _ = exit_rx.changed() => SelectOutcome::ProcessExited,
            // force-detach arm: HQ determined this session's work is done.
            _ = self.force_detach.notified() => SelectOutcome::ForceDetach,
        };

        // If relay is still running (non-relay arms), signal it to stop and wait briefly
        // so stdin is free before rustyline takes over.
        let reason = match outcome {
            SelectOutcome::RelayDone(r) => {
                drop(cancel);
                r
            }
            SelectOutcome::ProcessExited => {
                cancel.signal_and_wait(relay, 100).await;
                DetachReason::ProcessExit
            }
            SelectOutcome::ForceDetach => {
                cancel.signal_and_wait(relay, 100).await;
                DetachReason::AutoDetach
            }
        };

        // Stop mirroring to stdout before terminal restore so the drain thread
        // doesn't race with our escape sequences.
        {
            let mut sink = self.stdout_sink.lock().unwrap();
            *sink = None;
        }

        // Restore terminal.
        disable_raw_mode().ok();

        // Restore terminal state the attached agent may have modified:
        //   \x1b[?1049l  leave alternate screen (Claude Code uses it for its TUI)
        //   \x1b[?25h    show cursor (may have been hidden by the agent)
        //   \x1b[0m      reset SGR attributes (colors, bold, etc.)
        {
            let _ = std::io::stdout().write_all(b"\x1b[?1049l\x1b[?25h\x1b[0m\r\n");
            let _ = std::io::stdout().flush();
        }

        Ok(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_char_forwarded() {
        let mut s = PrefixKeyState::Normal;
        assert_eq!(
            process_byte(b'a', &mut s),
            RelayDecision::Forward(vec![b'a'])
        );
        assert_eq!(s, PrefixKeyState::Normal);
    }

    #[test]
    fn prefix_swallowed_and_transitions() {
        let mut s = PrefixKeyState::Normal;
        assert_eq!(
            process_byte(PREFIX_KEY, &mut s),
            RelayDecision::Forward(vec![])
        );
        assert_eq!(s, PrefixKeyState::GotPrefix);
    }

    #[test]
    fn prefix_d_detaches() {
        let mut s = PrefixKeyState::GotPrefix;
        assert_eq!(process_byte(b'd', &mut s), RelayDecision::Detach);
        assert_eq!(s, PrefixKeyState::Normal);
    }

    #[test]
    fn prefix_prefix_forwards_literal_prefix() {
        let mut s = PrefixKeyState::GotPrefix;
        assert_eq!(
            process_byte(PREFIX_KEY, &mut s),
            RelayDecision::Forward(vec![PREFIX_KEY])
        );
        assert_eq!(s, PrefixKeyState::Normal);
    }

    #[test]
    fn prefix_unknown_key_forwards_both() {
        let mut s = PrefixKeyState::GotPrefix;
        assert_eq!(
            process_byte(b'x', &mut s),
            RelayDecision::Forward(vec![PREFIX_KEY, b'x'])
        );
        assert_eq!(s, PrefixKeyState::Normal);
    }

    #[test]
    fn sequence_normal_prefix_d_detach() {
        let mut s = PrefixKeyState::Normal;
        // 'h' forwarded
        assert_eq!(
            process_byte(b'h', &mut s),
            RelayDecision::Forward(vec![b'h'])
        );
        // Ctrl+] swallowed
        assert_eq!(
            process_byte(PREFIX_KEY, &mut s),
            RelayDecision::Forward(vec![])
        );
        // 'd' → detach
        assert_eq!(process_byte(b'd', &mut s), RelayDecision::Detach);
    }

    #[test]
    fn strip_ansi_removes_csi() {
        // CSI color sequence stripped
        let input = b"\x1b[32mhello\x1b[0m world";
        let out = strip_ansi(input);
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn strip_ansi_removes_alternate_screen() {
        // \033[?1049h is a CSI sequence
        let input = b"\x1b[?1049hsome text\x1b[?1049l";
        let out = strip_ansi(input);
        assert_eq!(out, b"some text");
    }

    #[test]
    fn strip_ansi_normalises_crlf() {
        let input = b"line1\r\nline2\rline3\n";
        let out = strip_ansi(input);
        assert_eq!(out, b"line1\nline2\nline3\n");
    }
}
