use rustyline::{error::ReadlineError, DefaultEditor};
use tokio::sync::mpsc::UnboundedSender;

/// Blocking readline loop — run inside `tokio::task::spawn_blocking`.
/// Sends `Some(line)` for each input; `None` on EOF/quit.
pub fn readline_loop(tx: UnboundedSender<Option<String>>) {
    let mut rl = match DefaultEditor::new() {
        Ok(r) => r,
        Err(_) => return,
    };
    loop {
        let prompt = if std::path::Path::new("ferrus.toml").exists() {
            "ferrus> "
        } else {
            "ferrus (run /init first)> "
        };
        match rl.readline(prompt) {
            Ok(line) => {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    let _ = rl.add_history_entry(&line);
                }
                if tx.send(Some(line)).is_err() {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => {
                let _ = tx.send(Some(String::new())); // let main loop print hint
            }
            Err(ReadlineError::Eof) | Err(_) => {
                let _ = tx.send(None);
                break;
            }
        }
    }
}
