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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hq_prompt_is_non_empty() {
        // smoke test — real tty detection tested manually
        assert!(!hq_prompt().is_empty());
    }
}
