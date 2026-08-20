//! The session event log.
//!
//! This is the complete, append-only history of a session: every wake, every
//! block of code the model ran, every line it printed. It is deliberately
//! *separate* from the message window sent to the model — the model's context
//! gets compacted as a session runs for days, but this log never loses
//! anything. Attaching to a session replays this file and then tails it, which
//! is what makes `ax attach` behave like reconnecting to a tmux pane.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::broadcast;

/// Why the agent woke up. Injected into the model's context so it always knows
/// what pulled it out of sleep.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeReason {
    /// First wake of a brand new session.
    Initial,
    /// A timer the agent set for itself elapsed.
    Timer { note: String },
    /// A background process the agent spawned exited.
    ProcessExit { name: String, code: Option<i32> },
    /// A watched path changed.
    FileChanged { path: String },
    /// A human sent a message into the session.
    User { text: String },
    /// The daemon restarted and re-armed this session.
    Restart,
}

impl WakeReason {
    /// One-line description injected into the model's wake message.
    pub fn describe(&self) -> String {
        match self {
            WakeReason::Initial => "new goal, first wake".to_string(),
            WakeReason::Timer { note } => format!("timer fired: {note}"),
            WakeReason::ProcessExit { name, code } => match code {
                Some(c) => format!("background process `{name}` exited with code {c}"),
                None => format!("background process `{name}` was killed"),
            },
            WakeReason::FileChanged { path } => format!("watched path changed: {path}"),
            WakeReason::User { text } => format!("message from the user: {text}"),
            WakeReason::Restart => "daemon restarted; session resumed".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleStream {
    Stdout,
    Stderr,
}

/// Everything that can happen in a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted {
        goal: String,
        cwd: String,
        model: String,
    },
    WakeStarted {
        wake: u64,
        reason: WakeReason,
    },
    WakeEnded {
        wake: u64,
        /// How the wake was closed out: scheduled / done / budget / error.
        outcome: String,
    },
    /// Prose the model emitted (its reasoning-out-loud between tool calls).
    ModelText {
        text: String,
    },
    /// The model asked to run a block of JavaScript.
    ToolCall {
        call_id: String,
        code: String,
    },
    /// A line the running code printed, streamed live as it happens.
    Console {
        stream: ConsoleStream,
        text: String,
    },
    /// The outcome of a block of JavaScript.
    ToolResult {
        call_id: String,
        ok: bool,
        value: String,
        truncated: bool,
        duration_ms: u64,
    },
    /// The agent scheduled its own next wake.
    Scheduled {
        at: Option<DateTime<Utc>>,
        on: Option<String>,
        note: String,
    },
    /// The agent wants a human to see something.
    Notify {
        level: String,
        message: String,
    },
    /// The goal is finished.
    Done {
        summary: String,
    },
    /// A human said something into the session.
    UserMessage {
        text: String,
    },
    /// The model's context window was compacted. The full history stays here.
    Compacted {
        through_seq: u64,
        summary: String,
    },
    /// Token accounting, for budget enforcement across wakes.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    Error {
        message: String,
    },
}

/// An event with its position and timestamp in the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggedEvent {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Append-only writer + live broadcaster for one session's events.
pub struct EventLog {
    path: PathBuf,
    file: Mutex<File>,
    seq: AtomicU64,
    tx: broadcast::Sender<LoggedEvent>,
}

impl EventLog {
    /// Open (creating if needed) the log at `path`, resuming the sequence
    /// counter from whatever is already on disk.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let next_seq = if path.exists() {
            read_all(&path)?.last().map(|e| e.seq + 1).unwrap_or(0)
        } else {
            0
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open event log {}", path.display()))?;
        let (tx, _) = broadcast::channel(1024);
        Ok(Self {
            path,
            file: Mutex::new(file),
            seq: AtomicU64::new(next_seq),
            tx,
        })
    }

    /// Append an event: assign it a sequence number, flush it to disk, and
    /// hand it to every attached viewer.
    pub fn append(&self, kind: EventKind) -> Result<LoggedEvent> {
        let event = LoggedEvent {
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
            ts: Utc::now(),
            kind,
        };
        let line = serde_json::to_string(&event)?;
        {
            let mut file = self.file.lock().expect("event log mutex poisoned");
            // Flush per line: a session that dies mid-wake should still be
            // fully readable afterwards.
            writeln!(file, "{line}")?;
            file.flush()?;
        }
        // A send error just means nobody is attached right now.
        let _ = self.tx.send(event.clone());
        Ok(event)
    }

    /// Subscribe to events appended from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<LoggedEvent> {
        self.tx.subscribe()
    }

    /// The sequence number the next appended event will get.
    pub fn next_seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read an entire event log from disk. Malformed lines are skipped rather than
/// failing the read: a truncated final line from a hard kill should not make
/// the whole session unviewable.
pub fn read_all(path: &Path) -> Result<Vec<LoggedEvent>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open event log {}", path.display()))?;
    let mut events = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<LoggedEvent>(&line) {
            events.push(event);
        }
    }
    Ok(events)
}
