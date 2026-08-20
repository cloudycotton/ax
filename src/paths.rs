//! Filesystem layout for the agent's durable state.
//!
//! Everything the agent knows how to survive a restart with lives under
//! `~/.agent`. The isolate is scratch; this directory is truth.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Root of all agent state: `~/.agent` (override with `AGENT_HOME`).
pub fn agent_home() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("AGENT_HOME") {
        return Ok(PathBuf::from(custom));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".agent"))
}

/// Where per-goal session directories live.
pub fn sessions_dir() -> Result<PathBuf> {
    Ok(agent_home()?.join("sessions"))
}

/// Directory for a single session.
pub fn session_dir(id: &str) -> Result<PathBuf> {
    Ok(sessions_dir()?.join(id))
}

/// The daemon's unix socket.
pub fn daemon_socket() -> Result<PathBuf> {
    Ok(agent_home()?.join("daemon.sock"))
}

/// Create a directory and all parents, with a useful error message.
pub fn ensure_dir(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create directory {}", path.display()))
}
