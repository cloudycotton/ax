//! Terminal rendering of session events.
//!
//! Attaching to a session replays its whole event log through here, so this is
//! what "come back and see what happened" actually looks like.

use crate::event::{ConsoleStream, EventKind, LoggedEvent};
use crate::session::Status;

pub fn bold(s: &str) -> String {
    format!("\x1b[1m{s}\x1b[0m")
}

pub fn dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}

fn colored(code: &str, s: &str) -> String {
    format!("\x1b[{code}m{s}\x1b[0m")
}

/// A green affirmative, for things that worked.
pub fn ok(s: &str) -> String {
    colored("32", s)
}

/// A red negative, for things that did not.
pub fn bad(s: &str) -> String {
    colored("31", s)
}

pub fn status_label(status: Status) -> String {
    match status {
        Status::Running => colored("32", "running"),
        Status::Sleeping => colored("36", "sleeping"),
        Status::Done => colored("2", "done"),
        Status::Failed => colored("31", "failed"),
    }
}

pub fn truncate_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Print one event the way a human wants to read it.
pub fn render(logged: &LoggedEvent) {
    let time = logged.ts.format("%H:%M:%S").to_string();
    let stamp = dim(&time);
    match &logged.kind {
        EventKind::SessionStarted { goal, cwd, model } => {
            println!("{stamp} {} {}", colored("1;35", "goal"), goal);
            println!("{stamp} {} {cwd}  {}", dim("dir "), dim(model));
        }
        EventKind::WakeStarted { wake, reason } => {
            println!();
            println!(
                "{stamp} {} {}",
                colored("1;36", &format!("── wake #{wake}")),
                dim(&reason.describe())
            );
        }
        EventKind::WakeEnded { wake, outcome } => {
            println!(
                "{stamp} {}",
                dim(&format!("── wake #{wake} ended: {outcome}"))
            );
        }
        EventKind::ModelText { text } => {
            for line in text.lines() {
                println!("{stamp} {line}");
            }
        }
        EventKind::ToolCall { code, .. } => {
            println!("{stamp} {}", colored("1;33", "run_js"));
            for line in code.lines() {
                println!("{stamp}   {}", dim(line));
            }
        }
        EventKind::Console { stream, text } => {
            let tag = match stream {
                ConsoleStream::Stdout => colored("2", "│"),
                ConsoleStream::Stderr => colored("31", "│"),
            };
            println!("{stamp} {tag} {text}");
        }
        EventKind::ToolResult {
            ok,
            value,
            truncated,
            duration_ms,
            ..
        } => {
            let marker = if *ok {
                colored("32", "→")
            } else {
                colored("31", "✗")
            };
            let suffix = if *truncated {
                dim(" (truncated)")
            } else {
                String::new()
            };
            println!(
                "{stamp} {marker} {}{suffix} {}",
                truncate_line(value, 100),
                dim(&format!("{duration_ms}ms"))
            );
        }
        EventKind::Scheduled { at, on, note } => {
            let when = match (at, on) {
                (Some(at), _) => at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                (_, Some(on)) => on.clone(),
                _ => "unspecified".to_string(),
            };
            println!("{stamp} {} {when} — {note}", colored("36", "sleep until"));
        }
        EventKind::Notify { level, message } => {
            println!(
                "{stamp} {} {message}",
                colored("1;35", &format!("[{level}]"))
            );
        }
        EventKind::Done { summary } => {
            println!("{stamp} {} {summary}", colored("1;32", "done"));
        }
        EventKind::UserMessage { text } => {
            println!("{stamp} {} {text}", colored("1;34", "user"));
        }
        EventKind::Compacted { through_seq, .. } => {
            println!(
                "{stamp} {}",
                dim(&format!("(context compacted through #{through_seq})"))
            );
        }
        EventKind::Usage {
            prompt_tokens,
            completion_tokens,
        } => {
            println!(
                "{stamp} {}",
                dim(&format!(
                    "tokens: {prompt_tokens} in / {completion_tokens} out"
                ))
            );
        }
        EventKind::Error { message } => {
            println!("{stamp} {} {message}", colored("1;31", "error"));
        }
    }
}
