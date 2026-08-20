//! A session is one goal, rooted at one directory.
//!
//! Layout on disk:
//! ```text
//! ~/.ax/sessions/<id>/
//!   meta.json      session identity + status
//!   events.jsonl   complete history (see event.rs)
//!   ledger.md      the model's own record of progress; injected every wake
//!   schedule.json  pending wakes, re-armed on restart
//!   memory/        whatever the model decides to keep
//!   artifacts/     files it produces
//! ```

use crate::event::EventLog;
use crate::paths;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// A wake is executing right now.
    Running,
    /// Waiting for a timer or an event.
    Sleeping,
    /// The model declared the goal complete.
    Done,
    /// Stopped by an unrecoverable error.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub goal: String,
    /// Working directory the agent operates in — usually a repo.
    pub cwd: PathBuf,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub status: Status,
    /// How many wakes have started.
    pub wakes: u64,
}

pub struct Session {
    pub meta: SessionMeta,
    pub dir: PathBuf,
    /// Shared so the host functions append to the *same* log the session does;
    /// two writers on one file would corrupt the sequence numbering.
    pub log: Arc<EventLog>,
}

const LEDGER_TEMPLATE: &str = "\
# Goal

{goal}

## Plan

_(not started)_

## Done

## In progress

## Blocked / failed approaches

_Record what did not work here so you never retry it._

## Notes
";

impl Session {
    /// Create a fresh session rooted at `cwd`.
    pub fn create(goal: &str, cwd: &Path, model: &str) -> Result<Self> {
        let (id, dir) = claim_session_dir(cwd)?;
        paths::ensure_dir(&dir.join("memory"))?;
        paths::ensure_dir(&dir.join("artifacts"))?;

        let meta = SessionMeta {
            id,
            goal: goal.to_string(),
            cwd: cwd.to_path_buf(),
            model: model.to_string(),
            created_at: Utc::now(),
            status: Status::Running,
            wakes: 0,
        };

        let ledger = LEDGER_TEMPLATE.replace("{goal}", goal);
        std::fs::write(dir.join("ledger.md"), ledger)?;

        let log = Arc::new(EventLog::open(dir.join("events.jsonl"))?);
        let session = Self { meta, dir, log };
        // Recorded here rather than by the caller so every session's history
        // starts with its own header, however it was created.
        session
            .log
            .append(crate::event::EventKind::SessionStarted {
                goal: goal.to_string(),
                cwd: cwd.to_string_lossy().to_string(),
                model: model.to_string(),
            })?;
        session.save_meta()?;
        Ok(session)
    }

    /// Load an existing session by id.
    pub fn load(id: &str) -> Result<Self> {
        let dir = paths::session_dir(id)?;
        let meta_path = dir.join("meta.json");
        let raw = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("no such session `{id}`"))?;
        let meta: SessionMeta = serde_json::from_str(&raw)
            .with_context(|| format!("corrupt session metadata at {}", meta_path.display()))?;
        let log = Arc::new(EventLog::open(dir.join("events.jsonl"))?);
        Ok(Self { meta, dir, log })
    }

    /// All known sessions, newest first.
    pub fn list() -> Result<Vec<SessionMeta>> {
        let root = paths::sessions_dir()?;
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let meta_path = entry.path().join("meta.json");
            if !meta_path.exists() {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(&meta_path)
                && let Ok(meta) = serde_json::from_str::<SessionMeta>(&raw)
            {
                out.push(meta);
            }
        }
        out.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
        Ok(out)
    }

    pub fn save_meta(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&self.meta)?;
        std::fs::write(self.dir.join("meta.json"), raw)?;
        Ok(())
    }

    pub fn set_status(&mut self, status: Status) -> Result<()> {
        self.meta.status = status;
        self.save_meta()
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.dir.join("ledger.md")
    }

    /// The model's progress record, injected verbatim into every wake.
    pub fn ledger(&self) -> String {
        std::fs::read_to_string(self.ledger_path()).unwrap_or_default()
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.dir.join("memory")
    }
}

/// Claim a session id and its directory in one atomic step.
///
/// `create_dir` fails if the directory already exists, which is what makes the
/// claim exclusive: checking `exists()` first and creating later would let two
/// agents started in the same directory at the same moment claim one id and
/// then both write to the same event log.
fn claim_session_dir(cwd: &Path) -> Result<(String, PathBuf)> {
    let slug: String = cwd
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "session".into())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() {
        "session".into()
    } else {
        slug
    };

    let sessions = paths::sessions_dir()?;
    paths::ensure_dir(&sessions)?;
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 16;

    for attempt in 0..256 {
        let id = format!(
            "{slug}-{:04x}",
            (seed.wrapping_add(attempt * 7919)) & 0xffff
        );
        let dir = sessions.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("could not create {}", dir.display()));
            }
        }
    }
    anyhow::bail!(
        "could not allocate a unique session id for {}",
        cwd.display()
    )
}

/// A session's pending wake, persisted so a restart does not lose it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    /// Wake at this time.
    pub at: Option<DateTime<Utc>>,
    /// Wake when this background process exits. Not durable across a restart —
    /// the process died with the daemon — but recorded so the reason is visible.
    pub on_exit: Option<String>,
    pub note: String,
}

impl Session {
    fn schedule_path(&self) -> PathBuf {
        self.dir.join("schedule.json")
    }

    /// Record the pending wake. Best-effort: failing to write it costs us the
    /// timer on a restart, which is not worth aborting a live session over.
    pub fn save_schedule(&self, schedule: &Schedule) {
        if let Ok(raw) = serde_json::to_string_pretty(schedule) {
            let _ = std::fs::write(self.schedule_path(), raw);
        }
    }

    pub fn load_schedule(&self) -> Option<Schedule> {
        let raw = std::fs::read_to_string(self.schedule_path()).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn clear_schedule(&self) {
        let _ = std::fs::remove_file(self.schedule_path());
    }
}
