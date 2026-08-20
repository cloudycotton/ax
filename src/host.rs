//! Host capabilities, implemented independently of the JavaScript engine.
//!
//! Everything here takes plain Rust arguments and returns `serde_json::Value`,
//! so the isolate module is a thin binding layer and these can be tested
//! without a JS context. The set is deliberately small: shell, files, network,
//! background processes, and scheduling. Anything richer is the model's job to
//! build in JavaScript on top of these.

use crate::chrome::BrowserManager;
use crate::event::{ConsoleStream, EventKind, EventLog};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

/// How the model chose to end the current wake.
#[derive(Debug, Clone)]
pub enum WakeDecision {
    /// Sleep until a wall-clock time.
    At { at: DateTime<Utc>, note: String },
    /// Sleep until a named background process exits.
    OnExit { name: String, note: String },
    /// The goal is complete.
    Done { summary: String },
}

/// A background process that outlives the wake that started it.
pub struct ProcHandle {
    pub name: String,
    pub command: String,
    pub started: DateTime<Utc>,
    output: Arc<Mutex<Vec<String>>>,
    exit_code: Arc<Mutex<Option<Option<i32>>>>,
    pid: Option<u32>,
}

impl ProcHandle {
    /// `None` while still running, `Some(code)` once exited.
    pub fn exit(&self) -> Option<Option<i32>> {
        *self.exit_code.lock().expect("proc mutex poisoned")
    }

    pub fn running(&self) -> bool {
        self.exit().is_none()
    }

    /// Captured stdout+stderr, most recent `limit` lines.
    pub fn output(&self, limit: usize) -> String {
        let buf = self.output.lock().expect("proc mutex poisoned");
        let start = buf.len().saturating_sub(limit);
        buf[start..].join("\n")
    }

    pub fn kill(&self) -> Result<()> {
        if let Some(pid) = self.pid {
            // Target the whole process group: `sh -c` usually has children that
            // would otherwise be orphaned and keep running for days.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        Ok(())
    }
}

/// Shared state every host function needs.
pub struct HostContext {
    pub cwd: PathBuf,
    pub goal_dir: PathBuf,
    pub log: Arc<EventLog>,
    pub procs: Mutex<HashMap<String, Arc<ProcHandle>>>,
    /// Set when the model calls wake_in / wake_at / on_exit / done.
    pub decision: Mutex<Option<WakeDecision>>,
    /// Messages the model wants a human to see.
    pub notifications: Mutex<Vec<(String, String)>>,
    pub http: reqwest::Client,
    /// Announces process exits so the daemon can wake a sleeping session.
    pub exits: mpsc::UnboundedSender<(String, Option<i32>)>,
    /// Lazily-established connection to a browser.
    pub browser: Arc<BrowserManager>,
    /// Whether the configured model accepts images. Gates `see()`.
    pub vision: bool,
    /// Images the agent asked the model to look at, attached to the next
    /// message and then cleared.
    pub pending_images: Mutex<Vec<String>>,
}

impl HostContext {
    pub fn new(
        cwd: PathBuf,
        goal_dir: PathBuf,
        log: Arc<EventLog>,
        exits: mpsc::UnboundedSender<(String, Option<i32>)>,
        browser: Arc<BrowserManager>,
        vision: bool,
    ) -> Result<Self> {
        Ok(Self {
            cwd,
            goal_dir,
            log,
            procs: Mutex::new(HashMap::new()),
            decision: Mutex::new(None),
            notifications: Mutex::new(Vec::new()),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()?,
            exits,
            browser,
            vision,
            pending_images: Mutex::new(Vec::new()),
        })
    }

    /// Emit a console line: recorded in the log and streamed to anyone attached.
    pub fn console(&self, stream: ConsoleStream, text: &str) {
        let _ = self.log.append(EventKind::Console {
            stream,
            text: text.to_string(),
        });
    }

    /// Queue an image for the model to look at.
    pub fn attach_image(&self, data_uri: String) {
        self.pending_images
            .lock()
            .expect("image mutex poisoned")
            .push(data_uri);
    }

    /// Take everything queued since the last call.
    pub fn take_images(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending_images.lock().expect("image mutex poisoned"))
    }

    /// Resolve a path the way the agent's own file helpers do.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        self.resolve(path)
    }

    pub fn take_decision(&self) -> Option<WakeDecision> {
        self.decision
            .lock()
            .expect("decision mutex poisoned")
            .take()
    }

    /// Whether the model has already ended this wake.
    pub fn decision_pending(&self) -> bool {
        self.decision
            .lock()
            .expect("decision mutex poisoned")
            .is_some()
    }

    pub fn set_decision(&self, decision: WakeDecision) {
        *self.decision.lock().expect("decision mutex poisoned") = Some(decision);
    }

    /// Resolve a possibly-relative path against the session's working directory.
    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }
}

/// Run a shell command to completion.
pub async fn sh(ctx: &HostContext, cmd: &str, opts: &Value) -> Result<Value> {
    let cwd = opts
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| ctx.resolve(s))
        .unwrap_or_else(|| ctx.cwd.clone());
    let timeout_ms = opts
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(120_000);

    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(env) = opts.get("env").and_then(|v| v.as_object()) {
        for (key, value) in env {
            if let Some(value) = value.as_str() {
                command.env(key, value);
            }
        }
    }

    let child = command
        .spawn()
        .with_context(|| format!("failed to run: {cmd}"))?;
    let output = match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        child.wait_with_output(),
    )
    .await
    {
        Ok(result) => result.context("command failed")?,
        Err(_) => {
            return Ok(json!({
                "stdout": "",
                "stderr": format!("command timed out after {timeout_ms}ms"),
                "code": -1,
                "timed_out": true,
            }));
        }
    };

    Ok(json!({
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "code": output.status.code().unwrap_or(-1),
        "timed_out": false,
    }))
}

/// Start a background process that outlives this wake.
pub fn spawn_process(ctx: &Arc<HostContext>, cmd: &str, opts: &Value) -> Result<Value> {
    let cwd = opts
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| ctx.resolve(s))
        .unwrap_or_else(|| ctx.cwd.clone());
    let name = opts
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let n = ctx.procs.lock().expect("procs mutex poisoned").len();
            format!("proc{n}")
        });

    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("failed to spawn: {cmd}"))?;

    let output = Arc::new(Mutex::new(Vec::<String>::new()));
    let exit_code = Arc::new(Mutex::new(None));
    let pid = child.id();

    if let Some(stdout) = child.stdout.take() {
        let buf = output.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                push_capped(&buf, line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let buf = output.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                push_capped(&buf, line);
            }
        });
    }

    {
        let exit_code = exit_code.clone();
        let exits = ctx.exits.clone();
        let name = name.clone();
        tokio::spawn(async move {
            let status = child.wait().await.ok();
            let code = status.and_then(|s| s.code());
            *exit_code.lock().expect("proc mutex poisoned") = Some(code);
            let _ = exits.send((name, code));
        });
    }

    let handle = Arc::new(ProcHandle {
        name: name.clone(),
        command: cmd.to_string(),
        started: Utc::now(),
        output,
        exit_code,
        pid,
    });
    ctx.procs
        .lock()
        .expect("procs mutex poisoned")
        .insert(name.clone(), handle);

    Ok(json!({ "name": name, "pid": pid }))
}

/// Keep background output bounded; a server left running for days must not
/// grow without limit.
fn push_capped(buf: &Arc<Mutex<Vec<String>>>, line: String) {
    let mut buf = buf.lock().expect("proc mutex poisoned");
    buf.push(line);
    if buf.len() > 5_000 {
        let excess = buf.len() - 5_000;
        buf.drain(0..excess);
    }
}

pub async fn read_file(ctx: &HostContext, path: &str) -> Result<String> {
    let full = ctx.resolve(path);
    tokio::fs::read_to_string(&full)
        .await
        .with_context(|| format!("could not read {}", full.display()))
}

pub async fn write_file(ctx: &HostContext, path: &str, data: &str) -> Result<Value> {
    let full = ctx.resolve(path);
    if let Some(parent) = full.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::write(&full, data)
        .await
        .with_context(|| format!("could not write {}", full.display()))?;
    Ok(json!({ "path": full.to_string_lossy(), "bytes": data.len() }))
}

pub async fn list_dir(ctx: &HostContext, path: &str) -> Result<Value> {
    let full = ctx.resolve(path);
    let mut entries = tokio::fs::read_dir(&full)
        .await
        .with_context(|| format!("could not list {}", full.display()))?;
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let meta = entry.metadata().await.ok();
        out.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "dir": meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
            "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
        }));
    }
    Ok(Value::Array(out))
}

pub async fn exists(ctx: &HostContext, path: &str) -> bool {
    tokio::fs::metadata(ctx.resolve(path)).await.is_ok()
}

/// A small `fetch`. Body is returned as text; the JS prelude adds `.json()`.
pub async fn fetch(ctx: &HostContext, url: &str, opts: &Value) -> Result<Value> {
    let method = opts
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("GET")
        .to_uppercase();
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .with_context(|| format!("invalid HTTP method: {method}"))?;

    let mut request = ctx.http.request(method, url);
    if let Some(headers) = opts.get("headers").and_then(|v| v.as_object()) {
        for (key, value) in headers {
            if let Some(value) = value.as_str() {
                request = request.header(key, value);
            }
        }
    }
    if let Some(body) = opts.get("body") {
        match body {
            Value::String(s) => request = request.body(s.clone()),
            other if !other.is_null() => {
                request = request
                    .header("content-type", "application/json")
                    .body(other.to_string())
            }
            _ => {}
        }
    }

    let response = request.send().await.with_context(|| format!("GET {url}"))?;
    let status = response.status().as_u16();
    let mut headers = serde_json::Map::new();
    for (key, value) in response.headers() {
        if let Ok(value) = value.to_str() {
            headers.insert(key.to_string(), json!(value));
        }
    }
    let text = response.text().await.unwrap_or_default();

    Ok(json!({
        "status": status,
        "ok": (200..300).contains(&status),
        "headers": Value::Object(headers),
        "body": text,
    }))
}
