#![allow(dead_code)]

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

// ── Prefix-key FSM ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum PrefixKeyState {
    Normal,
    GotCtrlB,
}

#[derive(Debug, PartialEq)]
pub enum RelayDecision {
    /// Bytes to forward to the PTY (may be empty — swallowed prefix char).
    Forward(Vec<u8>),
    /// Ctrl-B d was detected — caller should detach.
    Detach,
}

/// Pure function: process one byte through the Ctrl-B d FSM.
/// Ctrl-B Ctrl-B → forward a literal Ctrl-B (escape hatch).
pub fn process_byte(byte: u8, state: &mut PrefixKeyState) -> RelayDecision {
    match (&*state, byte) {
        (PrefixKeyState::Normal, 0x02) => {
            *state = PrefixKeyState::GotCtrlB;
            RelayDecision::Forward(vec![]) // swallow until we know intent
        }
        (PrefixKeyState::GotCtrlB, b'd') => {
            *state = PrefixKeyState::Normal;
            RelayDecision::Detach
        }
        (PrefixKeyState::GotCtrlB, 0x02) => {
            // Ctrl-B Ctrl-B → forward a literal Ctrl-B
            *state = PrefixKeyState::Normal;
            RelayDecision::Forward(vec![0x02])
        }
        (PrefixKeyState::GotCtrlB, other) => {
            // Unknown sequence → forward Ctrl-B + the key verbatim
            *state = PrefixKeyState::Normal;
            RelayDecision::Forward(vec![0x02, other])
        }
        // Use `b` not `byte` to avoid shadowing the function parameter of the same name.
        (PrefixKeyState::Normal, b) => RelayDecision::Forward(vec![b]),
    }
}

// ── BackgroundSession ─────────────────────────────────────────────────────────

/// Why attach() returned.
#[derive(Debug, Clone, PartialEq)]
pub enum DetachReason {
    UserDetach,
    ProcessExit,
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
}

/// Spawn `binary args` in a background PTY.  Output streams to `log_path` always;
/// optionally mirrors to a sink set via `stdout_sink` during `/attach`.
pub fn spawn_background(
    binary: &str,
    args: &[&str],
    name: &str,
    log_path: &Path,
) -> Result<BackgroundSession> {
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
    })
}

impl BackgroundSession {
    /// Returns true if the process is still alive.
    pub fn is_alive(&self) -> bool {
        self.exit_rx.borrow().is_none()
    }

    /// Attach the terminal to this session.
    ///
    /// - Enables crossterm raw mode so all keystrokes go to the PTY.
    /// - Spawns a blocking stdin relay with Ctrl-B d interception.
    /// - Returns when the user presses Ctrl-B d or the process exits.
    ///
    /// Stdin is not contended: HQ's one-shot readline returns *before* calling
    /// attach(), so there is no background readline task during attach.
    ///
    /// # Watch channel note
    /// `exit_rx.changed()` only fires for *new* sends after the receiver's last-seen
    /// version. We must check for an already-dead process via `borrow()` (which does NOT
    /// advance the seen-version) before cloning the receiver, so that the clone inherits
    /// the pre-exit seen-version and `changed()` resolves immediately if the process
    /// exited between the guard check and the select.
    pub async fn attach(&self) -> Result<DetachReason> {
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

        // Fast path: process already dead — don't enter raw mode at all.
        if self.exit_rx.borrow().is_some() {
            return Ok(DetachReason::ProcessExit);
        }

        // Enable raw mode BEFORE setting stdout_sink.
        // This way, if enable_raw_mode() fails, stdout_sink is never set and
        // there's nothing to clean up — no guard/defer needed.
        enable_raw_mode().context("Failed to enable raw mode")?;

        // Route PTY output to our stdout while attached.
        {
            let mut sink = self.stdout_sink.lock().unwrap();
            *sink = Some(Box::new(std::io::stdout()));
        }

        let stdin_writer = Arc::clone(&self.stdin_writer);
        // Clone AFTER borrow() so the clone inherits the pre-exit seen-version.
        // If the process exits between here and the select, changed() resolves immediately.
        let mut exit_rx = self.exit_rx.clone();

        // Stdin relay: runs in a blocking thread so it can call stdin.read() directly.
        // Returns DetachReason when done.
        let relay = tokio::task::spawn_blocking(move || -> DetachReason {
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
                        RelayDecision::Forward(_) => {} // swallowed prefix char
                        RelayDecision::Detach => return DetachReason::UserDetach,
                    },
                }
            }
        });

        // Wait for relay to finish (user detach) or process to exit — whichever comes first.
        // NOTE: on ProcessExit, the stdin relay task may outlive this call briefly —
        // it will remain blocked on stdin.read() until the user presses a key or the
        // PTY is closed. This is acceptable for MVP; Phase C can add a cancellation flag.
        let reason = tokio::select! {
            r = relay => r.unwrap_or(DetachReason::ProcessExit),
            _ = exit_rx.changed() => DetachReason::ProcessExit,
        };

        // Always restore terminal before returning.
        disable_raw_mode().ok();

        // Stop mirroring output to stdout.
        {
            let mut sink = self.stdout_sink.lock().unwrap();
            *sink = None;
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
    fn ctrl_b_swallowed_and_transitions() {
        let mut s = PrefixKeyState::Normal;
        assert_eq!(process_byte(0x02, &mut s), RelayDecision::Forward(vec![]));
        assert_eq!(s, PrefixKeyState::GotCtrlB);
    }

    #[test]
    fn ctrl_b_d_detaches() {
        let mut s = PrefixKeyState::GotCtrlB;
        assert_eq!(process_byte(b'd', &mut s), RelayDecision::Detach);
        assert_eq!(s, PrefixKeyState::Normal);
    }

    #[test]
    fn ctrl_b_ctrl_b_forwards_literal_ctrl_b() {
        let mut s = PrefixKeyState::GotCtrlB;
        assert_eq!(
            process_byte(0x02, &mut s),
            RelayDecision::Forward(vec![0x02])
        );
        assert_eq!(s, PrefixKeyState::Normal);
    }

    #[test]
    fn ctrl_b_unknown_key_forwards_both() {
        let mut s = PrefixKeyState::GotCtrlB;
        assert_eq!(
            process_byte(b'x', &mut s),
            RelayDecision::Forward(vec![0x02, b'x'])
        );
        assert_eq!(s, PrefixKeyState::Normal);
    }

    #[test]
    fn sequence_normal_ctrl_b_d_detach() {
        let mut s = PrefixKeyState::Normal;
        // 'h' forwarded
        assert_eq!(
            process_byte(b'h', &mut s),
            RelayDecision::Forward(vec![b'h'])
        );
        // Ctrl-B swallowed
        assert_eq!(process_byte(0x02, &mut s), RelayDecision::Forward(vec![]));
        // 'd' → detach
        assert_eq!(process_byte(b'd', &mut s), RelayDecision::Detach);
    }
}
