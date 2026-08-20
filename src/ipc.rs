//! The wire protocol between the CLI and the daemon.
//!
//! Newline-delimited JSON over a unix socket. Deliberately small: the daemon
//! owns the sessions, and the CLI is a thin client that asks it to start one,
//! streams its events, or passes a message along.

use crate::event::LoggedEvent;
use crate::session::SessionMeta;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check, used to decide whether to start a daemon.
    Ping,
    /// Start a new session pursuing a goal.
    Create {
        goal: String,
        cwd: String,
        model: Option<String>,
        max_wakes: Option<u64>,
        max_tokens: Option<u64>,
    },
    /// Replay a session's history from `from_seq`, then stream it live.
    Attach { id: String, from_seq: u64 },
    /// Deliver a message from a person into a session, waking it.
    Say { id: String, text: String },
    /// Sessions the daemon knows about.
    List,
    /// Stop supervising a session (its process tree keeps whatever it spawned).
    Stop { id: String },
    /// Shut the daemon down.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
    Pong {
        version: String,
        sessions: usize,
    },
    Created {
        id: String,
    },
    Sessions {
        sessions: Vec<SessionMeta>,
    },
    /// One event in an attach stream.
    Event {
        event: Box<LoggedEvent>,
    },
    Ok,
    Error {
        message: String,
    },
}

impl Response {
    pub fn error(message: impl std::fmt::Display) -> Self {
        Response::Error {
            message: message.to_string(),
        }
    }
}
