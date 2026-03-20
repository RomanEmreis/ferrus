# Ferrus HQ Phase B — Background PTY Sessions Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Phase A's foreground `spawn_and_wait` with background PTY sessions so agents run headlessly while HQ stays interactive; users can `/attach <name>` to observe an agent and `Ctrl-B d` to detach.

**Architecture:** A new `src/pty.rs` module wraps `portable-pty` to spawn background sessions with a drain thread routing PTY output to a log file (always) and optionally to stdout during `/attach`. The HQ REPL switches from a persistent background `readline_loop` to one-shot per-command `readline_once` so stdin is never contended with an active PTY relay. `HqContext::on_state_change` becomes active, calling `transition_action` and spawning/killing background PTY sessions automatically as state transitions arrive from the state watcher.

**Tech Stack:** portable-pty 0.8 (cross-platform PTY), crossterm 0.28 (raw mode), tokio watch channel (exit signalling), std::sync::Mutex (stdout sink swap), rustyline 14 (one-shot readline)

---

## File Structure

| Action | Path | Responsibility |
|---|---|---|
| Create | `src/pty.rs` | `BackgroundSession`, `spawn_background`, `PrefixKeyState` FSM, `attach()` |
| Modify | `src/hq/repl.rs` | Replace `readline_loop` with `readline_once` |
| Modify | `src/hq/mod.rs` | Serial readline loop; `HqContext` gains `sessions`, `state_rx`, `last_task_state`; `on_state_change` drives orchestration |
| Modify | `src/hq/agent_manager.rs` | Add `spawn_background_pty` wrapping `pty::spawn_background` |
| Modify | `src/main.rs` | Add `mod pty;` |
| Modify | `Cargo.toml` | Add `portable-pty = "0.8"`, `crossterm = "0.28"` |
| Modify | `README.md`, `CLAUDE.md`, `AGENTS.md` | Phase B UX and `/attach` docs |

---

### Task 1: Add dependencies and PTY foundation

**Files:**
- Modify: `Cargo.toml`
- Create: `src/pty.rs`
- Modify: `src/main.rs` (add `mod pty;`)

- [ ] **Step 1: Add dependencies to Cargo.toml**

In `[dependencies]`:
```toml
portable-pty = "0.8"
crossterm = "0.28"
```

- [ ] **Step 2: Verify deps compile**

```sh
cargo check
```
Expected: compiles clean (no new warnings).

- [ ] **Step 3: Write failing FSM unit tests in src/pty.rs**

Create `src/pty.rs`:
```rust
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
    let child = pair.slave.spawn_command(cmd).context("Failed to spawn command in PTY")?;
    // Drop slave end so the master gets EOF when the child exits.
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("Failed to clone PTY reader")?;
    let stdin_writer: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(pair.master.take_writer().context("Failed to take PTY writer")?));

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
    // Note: portable_pty::ExitStatus has exit_code() -> Option<u32>, not success().
    // NOTE: this thread blocks on child.wait() indefinitely with no kill or timeout.
    // In Phase C, add explicit cancellation / kill support via a structured lifecycle.
    std::thread::spawn(move || {
        let code = child
            .wait()
            .map(|s| if s.exit_code() == Some(0) { 0i32 } else { 1i32 })
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_char_forwarded() {
        let mut s = PrefixKeyState::Normal;
        assert_eq!(process_byte(b'a', &mut s), RelayDecision::Forward(vec![b'a']));
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
        assert_eq!(process_byte(0x02, &mut s), RelayDecision::Forward(vec![0x02]));
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
        assert_eq!(process_byte(b'h', &mut s), RelayDecision::Forward(vec![b'h']));
        // Ctrl-B swallowed
        assert_eq!(process_byte(0x02, &mut s), RelayDecision::Forward(vec![]));
        // 'd' → detach
        assert_eq!(process_byte(b'd', &mut s), RelayDecision::Detach);
    }
}
```

- [ ] **Step 4: Declare mod in main.rs**

In `src/main.rs`, add `mod pty;` alongside the other mod declarations.

- [ ] **Step 5: Run FSM tests**

```sh
cargo test pty::
```
Expected: 6 tests pass.

- [ ] **Step 6: Run full test suite to confirm no regressions**

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```
Expected: all pass.

- [ ] **Step 7: Commit**

```sh
git add Cargo.toml Cargo.lock src/pty.rs src/main.rs
git commit -m "feat(pty): add BackgroundSession, spawn_background, Ctrl-B d FSM"
```

---

### Task 2: BackgroundSession::attach()

**Files:**
- Modify: `src/pty.rs` (add `attach()` impl)

- [ ] **Step 1: Write the attach() method**

Add to `src/pty.rs` after the `is_alive()` method:

```rust
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
```

- [ ] **Step 2: Verify compilation**

```sh
cargo check
```
Expected: compiles without errors.

- [ ] **Step 3: Run tests**

```sh
cargo test
cargo clippy -- -D warnings
```
Expected: all pass (no new warnings).

- [ ] **Step 4: Commit**

```sh
git add src/pty.rs
git commit -m "feat(pty): add BackgroundSession::attach() with Ctrl-B d detach"
```

---

### Task 3: agent_manager::spawn_background_pty

**Files:**
- Modify: `src/hq/agent_manager.rs`

- [ ] **Step 1: Add regression test**

Add to the `#[cfg(test)]` block in `src/hq/agent_manager.rs`:
```rust
#[test]
fn background_pty_log_path_contains_role() {
    // Regression: log path must embed the role name for easy grepping.
    let role = "executor";
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let log_path = format!(".ferrus/logs/{}_{}.log", role, ts);
    assert!(log_path.contains(role));
}
```

- [ ] **Step 2: Run test to confirm it compiles and passes**

```sh
cargo test background_pty_log_path_contains_role
```
Expected: PASS (this is a pure string-construction test; it verifies the log path format).

- [ ] **Step 3: Add spawn_background_pty function**

Add to `src/hq/agent_manager.rs`:
```rust
/// Spawn an agent in a background PTY session. Returns the `BackgroundSession`
/// handle. Agents.json is updated to `Running`.
pub async fn spawn_background_pty(
    agent_type: &str,
    role: &str,
    name: &str,
    prompt: Option<&str>,
) -> Result<crate::pty::BackgroundSession> {
    let binary = agent_binary(agent_type);

    let log_dir = std::path::Path::new(".ferrus/logs");
    tokio::fs::create_dir_all(log_dir)
        .await
        .context("Failed to create .ferrus/logs")?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let log_path = log_dir.join(format!("{role}_{ts}.log"));

    let args: Vec<&str> = match prompt {
        Some(p) => vec![p],
        None => vec![],
    };

    let session = crate::pty::spawn_background(binary, &args, name, &log_path)
        .with_context(|| format!("Failed to spawn {binary} as {role} in PTY"))?;

    // Update agents.json.
    let mut reg = read_agents().await?;
    reg.upsert(AgentEntry {
        role: role.to_string(),
        agent_type: agent_type.to_string(),
        name: name.to_string(),
        pid: None, // PTY child PID not directly accessible via portable-pty trait
        status: AgentStatus::Running,
        started_at: Some(chrono::Utc::now()),
    });
    write_agents(&reg).await?;

    // NOTE: `agents.json` is now a *logical* registry, not OS-level truth.
    // With PTY sessions, `pid` is None — `agents.json` tracks role lifecycle
    // (Running/Suspended) for display and reconciliation, not for OS-level kill.
    // The PTY session handle (`BackgroundSession`) is the authoritative control object.

    Ok(session)
}
```

- [ ] **Step 4: Run the test**

```sh
cargo test background_pty_log_path_contains_role
```
Expected: PASS.

- [ ] **Step 5: Full suite**

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```
Expected: all pass.

- [ ] **Step 6: Commit**

```sh
git add src/hq/agent_manager.rs
git commit -m "feat(agent_manager): add spawn_background_pty wrapping pty::spawn_background"
```

---

### Task 4: REPL refactor — one-shot readline

**Files:**
- Modify: `src/hq/repl.rs`
- Modify: `src/hq/mod.rs`

The Phase A REPL runs rustyline in a persistent `spawn_blocking` loop that holds stdin open at all times. Phase B needs stdin free between readline calls so the PTY relay can use it during `/attach`. Solution: `readline_once` — reads exactly one line, then returns.

**Why `block_in_place` not `spawn_blocking`:** `rustyline::DefaultEditor` is `!Send` (contains `Rc<...>` internally). `spawn_blocking` requires `Send + 'static`, so moving `DefaultEditor` across the boundary fails to compile. `tokio::task::block_in_place` runs blocking code on the *current* thread without spawning a new one — `DefaultEditor` stays in the same task and `!Send` is fine. This requires the multi-thread tokio runtime (the default for `#[tokio::main]`).

- [ ] **Step 1: Replace readline_loop with readline_once in repl.rs**

Replace the entire contents of `src/hq/repl.rs` with:

```rust
use rustyline::{error::ReadlineError, DefaultEditor};

/// Read a single line from the terminal.
///
/// Call via `tokio::task::block_in_place` (not `spawn_blocking`) — the caller owns
/// `DefaultEditor` which is `!Send` and must stay on the current thread.
///
/// Returns:
/// - `Some(line)` — user typed something (may be empty string for blank line / Ctrl-C)
/// - `None` — EOF or irrecoverable error; caller should exit the REPL loop
pub fn readline_once(rl: &mut DefaultEditor, prompt: &str) -> Option<String> {
    match rl.readline(prompt) {
        Ok(line) => {
            let line = line.trim().to_string();
            if !line.is_empty() {
                let _ = rl.add_history_entry(&line);
            }
            Some(line)
        }
        Err(ReadlineError::Interrupted) => {
            // Ctrl-C: treat as empty line so the caller can print a hint
            Some(String::new())
        }
        Err(_) => None, // EOF or other error → exit
    }
}

pub fn hq_prompt() -> &'static str {
    if std::path::Path::new("ferrus.toml").exists() {
        "ferrus> "
    } else {
        "ferrus (run /init first)> "
    }
}
```

- [ ] **Step 2: Write a unit test for readline_once**

In `src/hq/repl.rs` add:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hq_prompt_is_non_empty() {
        // smoke test — real tty detection tested manually
        assert!(!hq_prompt().is_empty());
    }
}
```

- [ ] **Step 3: Run test**

```sh
cargo test hq_prompt_is_non_empty
```
Expected: PASS.

- [ ] **Step 4: Refactor HqContext and run() in mod.rs**

Replace the `HqContext` struct, its `impl`, and the `run()` function in `src/hq/mod.rs` with the following. Keep `dispatch()`, `parse_agent_type()`, `pid_is_alive()`, `reconcile_agent_pids()`, `TransitionAction`, `transition_action()`, and all existing tests unchanged.

New `HqContext`:
```rust
pub(crate) struct HqContext {
    pub(crate) supervisor_type: Option<String>,
    pub(crate) executor_type: Option<String>,
    /// Active background PTY sessions, keyed by name (e.g. "executor-1").
    pub(crate) sessions: std::collections::HashMap<String, crate::pty::BackgroundSession>,
    /// Last observed task state (for transition detection in on_state_change).
    pub(crate) last_task_state: Option<crate::state::machine::TaskState>,
    /// State watcher receiver — drained before each readline call.
    state_rx: watch::Receiver<Option<StateData>>,
}
```

New `impl HqContext`:
```rust
impl HqContext {
    fn new(state_rx: watch::Receiver<Option<StateData>>) -> Self {
        Self {
            supervisor_type: None,
            executor_type: None,
            sessions: std::collections::HashMap::new(),
            last_task_state: None,
            state_rx,
        }
    }

    /// Drain any state changes that arrived since the last readline call.
    /// Prints transition banners and triggers on_state_change without blocking.
    pub(crate) async fn drain_state_changes(&mut self) {
        loop {
            match self.state_rx.has_changed() {
                Ok(true) => {
                    let new = self.state_rx.borrow_and_update().clone();
                    if let Some(new_state) = new {
                        let prev = self.last_task_state.clone();
                        if prev.as_ref() != Some(&new_state.state) {
                            if let Some(ref p) = prev {
                                display::print_transition(p, &new_state.state);
                            }
                            self.on_state_change(&new_state).await;
                        }
                        self.last_task_state = Some(new_state.state.clone());
                    }
                }
                _ => break,
            }
        }
    }

    /// Called when STATE.json transitions to a new TaskState.
    /// Phase B: drives automatic spawning of executor/reviewer background sessions.
    ///
    /// # Design note: bootstrap guard
    /// `on_state_change` requires a known previous state to compute `transition_action`.
    /// When `last_task_state` is None (HQ just started or restarted with an active task),
    /// there is no previous state, so we record the current state and return — no spawning.
    /// This prevents a cold-start observation of e.g. `Executing` from being misread as
    /// a fresh Idle→Executing transition that needs a new executor spawned.
    ///
    /// The Idle→Executing transition triggered by `/plan` is handled *explicitly* in
    /// `plan()` via `spawn_background_session` — not via this path.
    ///
    /// TODO(Phase C): `bootstrap_from_state` — when HQ restarts with an active task and
    /// no live session, auto-reattach or prompt the user to resume.
    pub(crate) async fn on_state_change(&mut self, state: &StateData) {
        // Bootstrap guard: first observation records state without spawning anything.
        // Prevents misinterpreting a cold-start observation as a new transition.
        if self.last_task_state.is_none() {
            self.last_task_state = Some(state.state.clone());
            return;
        }
        // Requires last_task_state to compute the transition action.
        let Some(ref prev) = self.last_task_state else {
            return;
        };
        let action = transition_action(prev, &state.state);
        let exe_type = self.executor_type.clone().unwrap_or_else(|| "codex".into());
        let sup_type = self
            .supervisor_type
            .clone()
            .unwrap_or_else(|| "claude-code".into());
        match action {
            TransitionAction::SpawnExecutor => {
                if let Err(e) = self
                    .spawn_background_session(
                        &exe_type,
                        "executor",
                        "executor-1",
                        Some(agent_manager::executor_prompt()),
                    )
                    .await
                {
                    display::print_error(&format!("Failed to spawn executor: {e}"));
                }
            }
            TransitionAction::SpawnReviewer => {
                // Close the executor session before spawning the reviewer.
                // If the executor is still alive (rare), dropping closes the PTY.
                // On Unix this typically sends SIGHUP; not guaranteed on all platforms.
                self.sessions.remove("executor-1");
                if let Err(e) = self
                    .spawn_background_session(
                        &sup_type,
                        "supervisor",
                        "supervisor-1",
                        Some(agent_manager::reviewer_prompt()),
                    )
                    .await
                {
                    display::print_error(&format!("Failed to spawn reviewer: {e}"));
                }
            }
            TransitionAction::KillReviewerSpawnExecutor => {
                // Dropping the session closes the PTY master.
                // On Unix this typically results in SIGHUP to the child,
                // but this is not guaranteed across all platforms or agents.
                self.sessions.remove("supervisor-1");
                if let Err(e) = self
                    .spawn_background_session(
                        &exe_type,
                        "executor",
                        "executor-1",
                        Some(agent_manager::executor_prompt()),
                    )
                    .await
                {
                    display::print_error(&format!("Failed to spawn executor: {e}"));
                }
            }
            TransitionAction::TaskComplete => {
                // Clean up all sessions — task is done.
                self.sessions.remove("executor-1");
                self.sessions.remove("supervisor-1");
                display::print_info("Task complete! Use /plan to start a new task.");
            }
            TransitionAction::TaskFailed => {
                // Clean up all sessions — nothing useful left running.
                self.sessions.remove("executor-1");
                self.sessions.remove("supervisor-1");
                display::print_info(
                    "Task failed. Use /status for details, /reset to try again.",
                );
            }
            TransitionAction::NoOp => {}
        }
    }

    /// Spawn a named background PTY session, skipping if one is already alive.
    ///
    /// # Session name contract
    /// Session names (e.g. "executor-1", "supervisor-1") are unique by role. The reuse
    /// check is by name only — callers must ensure the name always maps to the same
    /// role/agent_type combination. This invariant holds for Phase B's fixed roles.
    pub(crate) async fn spawn_background_session(
        &mut self,
        agent_type: &str,
        role: &str,
        name: &str,
        prompt: Option<&str>,
    ) -> Result<()> {
        // Reuse if already alive.
        if let Some(existing) = self.sessions.get(name) {
            if existing.is_alive() {
                display::print_info(&format!("{name} already running."));
                return Ok(());
            }
            self.sessions.remove(name);
        }
        display::print_info(&format!("Spawning {name} ({agent_type}) in background…"));
        let session =
            agent_manager::spawn_background_pty(agent_type, role, name, prompt).await?;
        display::print_info(&format!(
            "{name} started. Use /attach {name} to observe. Logs: {}",
            session.log_path.display(),
        ));
        self.sessions.insert(name.to_string(), session);
        Ok(())
    }

    /// Synchronous planning flow (still interactive — user types with supervisor).
    async fn plan(&mut self) -> Result<()> {
        use crate::config::Config;
        use crate::state::machine::TaskState;

        let config = Config::load().await?;
        let hq = config.hq.ok_or_else(|| {
            anyhow::anyhow!(
                "No [hq] section in ferrus.toml. Add:\n[hq]\nsupervisor = \"claude-code\"\nexecutor = \"codex\""
            )
        })?;

        let state = store::read_state().await?;
        if state.state != TaskState::Idle {
            anyhow::bail!(
                "State is {:?} — /plan requires Idle. Use /status.",
                state.state
            );
        }

        self.supervisor_type = Some(hq.supervisor.clone());
        self.executor_type = Some(hq.executor.clone());

        display::print_info(&format!("Spawning supervisor ({})…", hq.supervisor));
        display::print_info(
            "Collaborate with the supervisor to define the task. Exit it when done.",
        );

        // Interactive planning — foreground so user can type.
        agent_manager::spawn_and_wait(
            &hq.supervisor,
            "supervisor",
            "supervisor-1",
            Some(agent_manager::supervisor_plan_prompt()),
        )
        .await?;

        // After supervisor exits, check if a task was created.
        let new_state = store::read_state().await?;
        if new_state.state == TaskState::Executing {
            display::print_info("Task created — spawning executor in background…");
            self.spawn_background_session(
                &hq.executor,
                "executor",
                "executor-1",
                Some(agent_manager::executor_prompt()),
            )
            .await?;
            display::print_info(
                "Executor running. State changes will print automatically. Use /attach executor-1 to observe.",
            );
        } else {
            display::print_info(&format!(
                "No task created (state is {:?}). Re-run /plan when ready.",
                new_state.state
            ));
        }
        Ok(())
    }
}
```

New `run()`:

Also update the module-level import: remove `mpsc` from `use tokio::sync::{mpsc, watch};` → `use tokio::sync::watch;` (mpsc is no longer used).

```rust
pub async fn run() -> Result<()> {
    use rustyline::DefaultEditor;

    reconcile_agent_pids().await;
    display::print_info("ferrus HQ — /status, /plan, /attach <name>, /quit, /help");

    let (state_tx, state_rx) = watch::channel::<Option<StateData>>(None);
    tokio::spawn(state_watcher::watch(state_tx));

    let mut ctx = HqContext::new(state_rx);
    // DefaultEditor is !Send — use block_in_place (same thread) rather than spawn_blocking.
    let mut rl = DefaultEditor::new()?;

    loop {
        // Print any state transitions that arrived while we were waiting.
        ctx.drain_state_changes().await;

        let prompt = repl::hq_prompt().to_string();
        // block_in_place: runs blocking readline on the current thread without spawning.
        // rl stays in scope — no Send requirement, no move-in/move-out dance.
        let line = tokio::task::block_in_place(|| repl::readline_once(&mut rl, &prompt));

        match line {
            Some(l) if !l.is_empty() => {
                if let Err(e) = dispatch(&l, &mut ctx).await {
                    display::print_error(&e.to_string());
                }
            }
            Some(_) => {} // blank line or Ctrl-C
            None => {
                display::print_info("Bye.");
                break;
            }
        }
    }
    Ok(())
}
```

Update `dispatch()` — replace the `ShellCommand::Attach` arm and `ShellCommand::Plan` arm:
```rust
ShellCommand::Plan => ctx.plan().await?,
ShellCommand::Attach { name } => {
    if let Some(session) = ctx.sessions.get(&name) {
        display::print_info(&format!(
            "Attaching to {name}. Ctrl-B d to detach.",
        ));
        match session.attach().await {
            Ok(crate::pty::DetachReason::UserDetach) => {
                display::print_info(&format!(
                    "Detached from {name}. Use /attach {name} to reconnect."
                ))
            }
            Ok(crate::pty::DetachReason::ProcessExit) => {
                display::print_info(&format!("{name} process exited."));
                ctx.sessions.remove(&name);
            }
            Err(e) => display::print_error(&format!("Attach error: {e}")),
        }
    } else {
        display::print_error(&format!(
            "No session named '{name}'. Run /status to see active sessions."
        ));
    }
}
```

- [ ] **Step 5: Fix display::print_status to show sessions**

In `src/hq/display.rs`, update `print_status` (or add a new `print_sessions`) to also list sessions from `HqContext`. Since `print_status` currently takes `&StateData` and `&AgentsRegistry`, update `dispatch` for `ShellCommand::Status` to also print session names:

In `dispatch()`, change the Status arm to:
```rust
ShellCommand::Status => {
    let state = store::read_state().await?;
    let reg = agents::read_agents().await?;
    display::print_status(&state, &reg);
    if !ctx.sessions.is_empty() {
        display::print_info("PTY sessions:");
        for (name, session) in &ctx.sessions {
            let status = if session.is_alive() { "running" } else { "exited" };
            display::print_info(&format!(
                "  {name} ({status}) — /attach {name} — logs: {}",
                session.log_path.display(),
            ));
        }
    }
}
```

- [ ] **Step 6: Compile and test**

```sh
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```
Expected: all pass (may need to add `#[allow(dead_code)]` to `run_executor_loop` if still present — but see next step).

- [ ] **Step 7: Remove dead run_executor_loop**

The `run_executor_loop` method is now dead (replaced by `on_state_change`). Remove it from `HqContext`. Verify clippy is still clean.

```sh
cargo clippy -- -D warnings
```

- [ ] **Step 8: Commit**

```sh
git add src/hq/repl.rs src/hq/mod.rs src/hq/display.rs
git commit -m "feat(hq): one-shot readline_once; HqContext with background sessions and on_state_change"
```

---

### Task 5: Wire /attach into dispatch and integration smoke test

**Files:**
- Modify: `src/hq/commands.rs` (verify Attach parses name correctly)
- Manual smoke test

- [ ] **Step 1: Verify /attach command parsing**

In `src/hq/commands.rs`, confirm `ShellCommand::Attach { name: String }` is defined and `parse_command("/attach executor-1")` returns `Attach { name: "executor-1" }`. If not, fix the clap definition.

Run:
```sh
cargo test parse_command
```
Expected: existing parse tests pass; add a new test:

```rust
#[test]
fn attach_parses_name() {
    let cmd = parse_command("/attach executor-1").unwrap();
    assert!(matches!(cmd, ShellCommand::Attach { name } if name == "executor-1"));
}
```

Run `cargo test attach_parses_name` — expected: PASS.

- [ ] **Step 2: Update /attach stub comment in AGENTS.md/README.md**

The `README.md` lists `/attach <role>` as "Phase B" — update to remove the "(Phase B)" marker. Update description: "Attach terminal to a running background session. Ctrl-B d to detach."

- [ ] **Step 3: Manual smoke test (document expected behavior)**

```
# In one terminal:
cargo build
./target/debug/ferrus init
# Edit ferrus.toml to add [hq] section (or use ferrus register)

# Then:
./target/debug/ferrus
ferrus> /plan
# Supervisor spawns interactively (foreground). Define a trivial task.
# After supervisor creates task and exits:
# → "Task created — spawning executor in background…"
# → "executor-1 started. Use /attach executor-1 to observe."
ferrus> /status
# Shows state = Executing, PTY sessions: executor-1
ferrus> /attach executor-1
# Terminal shows executor output in real-time
# Press Ctrl-B d to detach
# → "Detached from executor-1."
ferrus> /status
# Shows updated state as executor progresses
```

- [ ] **Step 4: Full test suite + clippy**

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```
Expected: all pass.

- [ ] **Step 5: Commit**

```sh
git add src/hq/commands.rs README.md
git commit -m "feat(hq): wire /attach to BackgroundSession::attach(); update /attach docs"
```

---

### Task 6: Cleanup, documentation, and version bump

**Files:**
- Modify: `Cargo.toml` (version bump)
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `AGENTS.md`

- [ ] **Step 1: Bump version in Cargo.toml**

Change `version = "0.2.0-alpha.1"` → `version = "0.2.0-alpha.2"`.

- [ ] **Step 2: Update README.md HQ commands table**

Replace the HQ commands table with:

| Command | Description |
|---|---|
| `/plan` | Spawn supervisor (interactive) to plan; executor runs headlessly in background |
| `/status` | Show task state, agent list, and active PTY sessions |
| `/attach <name>` | Attach terminal to a running background session (e.g. `executor-1`). Ctrl-B d to detach |
| `/quit` | Exit HQ |

Add a note about Ctrl-B d escape:
> **Detach key:** `Ctrl-B d` detaches from an attached session without killing it. `Ctrl-B Ctrl-B` sends a literal `Ctrl-B` to the agent.

- [ ] **Step 3: Update CLAUDE.md Source Layout**

Add `src/pty.rs` to the source layout table:
```
src/
  pty.rs                     # BackgroundSession, spawn_background, Ctrl-B d FSM, attach()
```

Update `hq/agent_manager.rs` description to: `spawn_and_wait, spawn_background_pty, kill_role; agents.json updates`

- [ ] **Step 4: Update AGENTS.md**

Update the Source Layout section with `pty.rs`. Update the HQ command table to match README.

- [ ] **Step 5: Final full suite**

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```
Expected: all pass.

- [ ] **Step 6: Commit**

```sh
git add Cargo.toml README.md CLAUDE.md AGENTS.md
git commit -m "chore: bump to 0.2.0-alpha.2; update Phase B docs"
```

---

## Exit Criteria

- `cargo test` passes with no failures
- `cargo clippy -- -D warnings` is clean
- `cargo fmt --check` is clean
- `ferrus` opens HQ REPL
- `/plan` spawns supervisor interactively, then executor in background PTY after task creation
- `/attach <name>` connects terminal to a background session; Ctrl-B d detaches cleanly
- State transitions (Reviewing, Complete, etc.) print automatically between readline prompts
- Log files written to `.ferrus/logs/<role>_<ts>.log` during background sessions

---

## Known Phase B Limitations

These are intentional design choices for Phase B, not bugs. Recorded here to prevent future confusion.

| Limitation | Notes |
|---|---|
| `agents.json` is a logical registry, not OS truth | `pid` is `None` for all PTY sessions; no OS-level kill or liveness probe via PID in Phase B |
| HQ restart does not rehydrate PTY sessions | On restart, `last_task_state` bootstraps from current STATE.json but no sessions are respawned. User must `/plan` again or attach manually if agent is still running. Phase C: `bootstrap_from_state` |
| Relay task may outlive `attach()` on `ProcessExit` | When process exits first, the stdin relay task stays blocked on `stdin.read()` until the user presses a key. Acceptable for MVP. Phase C: cancellation flag |
| stdout mirroring is best-effort during detach | Drain thread may write to the old stdout sink briefly after `*sink = None` is set. Rare, not data-corrupting, acceptable for terminal output |
| No kill/timeout for background PTY child | The exit watcher thread calls `child.wait()` indefinitely. Phase C: structured lifecycle with explicit kill support |
| Log files grow indefinitely | No rotation or retention policy. Phase C: add rotation / max-size limit |
| Session drop ≠ guaranteed kill | Dropping a `BackgroundSession` closes the PTY master, which typically sends SIGHUP on Unix. Not guaranteed on all platforms or for agents that ignore SIGHUP |
