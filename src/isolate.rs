//! The JavaScript isolate.
//!
//! One QuickJS context lives for the whole session on its own thread. It is a
//! composition layer, not a sandbox — the agent is meant to reach the whole
//! machine — so the value here is that state accumulates: helpers the model
//! writes, data it collects, and process handles it holds all survive from one
//! wake to the next.
//!
//! rquickjs' async runtime is not `Send` unless the experimental `parallel`
//! feature is on, so the context is pinned to a dedicated thread running a
//! current-thread tokio runtime inside a `LocalSet`, and the rest of the
//! program talks to it over channels.

use crate::event::ConsoleStream;
use crate::host::{self, HostContext, WakeDecision};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rquickjs::prelude::{Async, Func};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Function, Value};
use serde_json::{Value as JsonValue, json};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// Results larger than this are truncated on the way back to the model. The
/// isolate keeps the real value, so the model can slice it instead.
const MAX_RESULT_CHARS: usize = 40_000;

/// A single `await sleep(ms)` may not exceed this — waiting longer than a few
/// minutes is what `wake_in` is for, and it costs nothing while asleep.
const MAX_SLEEP_MS: u64 = 600_000;

#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub ok: bool,
    pub value: String,
    pub truncated: bool,
    pub duration_ms: u64,
}

enum Job {
    Exec {
        code: String,
        timeout: Duration,
        reply: oneshot::Sender<ExecOutcome>,
    },
    Manifest {
        reply: oneshot::Sender<String>,
    },
}

/// Handle to the isolate thread.
pub struct Isolate {
    tx: mpsc::UnboundedSender<Job>,
}

impl Isolate {
    /// Start the isolate thread and evaluate the prelude into it.
    pub fn start(host: Arc<HostContext>) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        std::thread::Builder::new()
            .name("isolate".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        let _ = ready_tx.send(Err(format!("tokio runtime: {err}")));
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&runtime, isolate_main(host, rx, ready_tx));
            })
            .map_err(|e| anyhow!("could not start isolate thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { tx }),
            Ok(Err(err)) => Err(anyhow!("isolate failed to start: {err}")),
            Err(_) => Err(anyhow!("isolate thread died during startup")),
        }
    }

    /// Run a block of JavaScript and wait for its result.
    pub async fn exec(&self, code: &str, timeout: Duration) -> Result<ExecOutcome> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Job::Exec {
                code: code.to_string(),
                timeout,
                reply,
            })
            .map_err(|_| anyhow!("isolate is gone"))?;
        response
            .await
            .map_err(|_| anyhow!("isolate dropped the job"))
    }

    /// Summarize what is still alive in the isolate, for the next wake.
    pub async fn manifest(&self) -> Result<String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Job::Manifest { reply })
            .map_err(|_| anyhow!("isolate is gone"))?;
        response
            .await
            .map_err(|_| anyhow!("isolate dropped the job"))
    }
}

async fn isolate_main(
    host: Arc<HostContext>,
    mut rx: mpsc::UnboundedReceiver<Job>,
    ready: std::sync::mpsc::Sender<std::result::Result<(), String>>,
) {
    let qjs = match AsyncRuntime::new() {
        Ok(rt) => rt,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    let ctx = match AsyncContext::full(&qjs).await {
        Ok(ctx) => ctx,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };

    qjs.set_max_stack_size(4 * 1024 * 1024).await;

    // The interrupt handler is the only way to stop a runaway JS loop without
    // killing the process. It fires during bytecode execution, so a deadline in
    // the past aborts the script with an uncatchable exception.
    let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
    {
        let deadline = deadline.clone();
        qjs.set_interrupt_handler(Some(Box::new(
            move || matches!(deadline.get(), Some(at) if Instant::now() > at),
        )))
        .await;
    }

    // Drives futures spawned by async host functions.
    tokio::task::spawn_local(qjs.drive());

    let install_result = ctx
        .async_with(async |ctx| install_globals(&ctx, &host).map_err(|e| e.to_string()))
        .await;
    if let Err(err) = install_result {
        let _ = ready.send(Err(err));
        return;
    }

    let _ = ready.send(Ok(()));

    while let Some(job) = rx.recv().await {
        match job {
            Job::Exec {
                code,
                timeout,
                reply,
            } => {
                let started = Instant::now();
                deadline.set(Some(started + timeout));
                let outcome = execute(&ctx, code).await;
                deadline.set(None);
                let _ = reply.send(ExecOutcome {
                    duration_ms: started.elapsed().as_millis() as u64,
                    ..outcome
                });
            }
            Job::Manifest { reply } => {
                let manifest = ctx
                    .async_with(async |ctx| {
                        let globals = ctx.globals();
                        let f: Function = match globals.get("__manifest") {
                            Ok(f) => f,
                            Err(_) => return String::new(),
                        };
                        f.call::<_, String>(()).unwrap_or_default()
                    })
                    .await;
                let _ = reply.send(manifest);
            }
        }
    }
}

/// Evaluate one block of model-written JavaScript.
async fn execute(ctx: &AsyncContext, code: String) -> ExecOutcome {
    // Wrapping in an async IIFE buys top-level `await` and `return`, and gives
    // each block a fresh lexical scope so the model can re-declare `const foo`
    // in a later block without a redeclaration error. State it wants to keep is
    // assigned to `globalThis` explicitly.
    let wrapped = format!("(async () => {{{code}\n}})()");

    ctx.async_with(async move |ctx| {
        let evaluated: rquickjs::Result<Value> = ctx.eval(wrapped);
        let value = match evaluated.catch(&ctx) {
            Ok(value) => value,
            Err(err) => return failure(format_caught(&err)),
        };

        // The IIFE always yields a promise; await it for the real result.
        let resolved = if let Some(promise) = value.clone().into_promise() {
            match promise.into_future::<Value>().await.catch(&ctx) {
                Ok(value) => value,
                Err(err) => return failure(format_caught(&err)),
            }
        } else {
            value
        };

        let rendered = match render(&ctx, resolved) {
            Ok(text) => text,
            Err(err) => return failure(format!("could not render result: {err}")),
        };
        success(rendered)
    })
    .await
}

/// Format a value using the prelude's `inspect`, falling back to a debug
/// rendering if the prelude is somehow unavailable.
fn render<'js>(ctx: &rquickjs::Ctx<'js>, value: Value<'js>) -> Result<String> {
    if value.is_undefined() {
        return Ok(String::new());
    }
    let globals = ctx.globals();
    let inspect: Function = globals
        .get("__inspect")
        .map_err(|e| anyhow!("prelude missing __inspect: {e}"))?;
    let text: String = inspect
        .call((value,))
        .map_err(|e| anyhow!("inspect failed: {e}"))?;
    Ok(text)
}

fn success(text: String) -> ExecOutcome {
    let (value, truncated) = truncate(text);
    ExecOutcome {
        ok: true,
        value,
        truncated,
        duration_ms: 0,
    }
}

fn failure(text: String) -> ExecOutcome {
    let (value, truncated) = truncate(text);
    ExecOutcome {
        ok: false,
        value,
        truncated,
        duration_ms: 0,
    }
}

/// Keep the head and tail of oversized output and say plainly what was cut, so
/// the model knows to go back and slice the value itself.
fn truncate(text: String) -> (String, bool) {
    if text.chars().count() <= MAX_RESULT_CHARS {
        return (text, false);
    }
    let chars: Vec<char> = text.chars().collect();
    let half = MAX_RESULT_CHARS / 2;
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    let omitted = chars.len() - MAX_RESULT_CHARS;
    (
        format!(
            "{head}\n\n… [{omitted} characters omitted — the full value is still in the isolate; \
keep it in a global and inspect it in slices] …\n\n{tail}"
        ),
        true,
    )
}

fn format_caught(err: &CaughtError<'_>) -> String {
    match err {
        // `Display` for Exception already prints "Error: message\n<stack>".
        CaughtError::Exception(exception) => exception.to_string(),
        CaughtError::Value(value) => {
            format!("threw a non-Error value: {value:?}")
        }
        CaughtError::Error(error) => error.to_string(),
    }
}

// --------------------------------------------------------------- bindings

/// JSON envelope so JavaScript can tell success from failure.
fn ok_json(value: serde_json::Value) -> String {
    json!({ "ok": true, "value": value }).to_string()
}

fn err_json(error: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": error.to_string() }).to_string()
}

fn parse_opts(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
}

/// Install the low-level bindings, then evaluate the prelude that turns them
/// into the API the model actually sees.
fn install_globals(ctx: &rquickjs::Ctx<'_>, host: &Arc<HostContext>) -> Result<()> {
    let globals = ctx.globals();

    // console + log
    {
        let host = host.clone();
        globals.set(
            "__emit",
            Func::from(move |stream: String, text: String| {
                let stream = if stream == "stderr" {
                    ConsoleStream::Stderr
                } else {
                    ConsoleStream::Stdout
                };
                host.console(stream, &text);
            }),
        )?;
    }

    // shell
    {
        let host = host.clone();
        globals.set(
            "__sh",
            Func::from(Async(move |cmd: String, opts: String| {
                let host = host.clone();
                async move {
                    match host::sh(&host, &cmd, &parse_opts(&opts)).await {
                        Ok(value) => ok_json(value),
                        Err(err) => err_json(err),
                    }
                }
            })),
        )?;
    }

    // background processes
    {
        let host = host.clone();
        globals.set(
            "__spawn",
            Func::from(move |cmd: String, opts: String| {
                match host::spawn_process(&host, &cmd, &parse_opts(&opts)) {
                    Ok(value) => ok_json(value),
                    Err(err) => err_json(err),
                }
            }),
        )?;
    }
    {
        let host = host.clone();
        globals.set(
            "__proc",
            Func::from(move |name: String, action: String, args: String| {
                proc_action(&host, &name, &action, &parse_opts(&args))
            }),
        )?;
    }

    // files
    {
        let host = host.clone();
        globals.set(
            "__read",
            Func::from(Async(move |path: String| {
                let host = host.clone();
                async move {
                    match host::read_file(&host, &path).await {
                        Ok(text) => ok_json(json!(text)),
                        Err(err) => err_json(err),
                    }
                }
            })),
        )?;
    }
    {
        let host = host.clone();
        globals.set(
            "__write",
            Func::from(Async(move |path: String, data: String| {
                let host = host.clone();
                async move {
                    match host::write_file(&host, &path, &data).await {
                        Ok(value) => ok_json(value),
                        Err(err) => err_json(err),
                    }
                }
            })),
        )?;
    }
    {
        let host = host.clone();
        globals.set(
            "__ls",
            Func::from(Async(move |path: String| {
                let host = host.clone();
                async move {
                    match host::list_dir(&host, &path).await {
                        Ok(value) => ok_json(value),
                        Err(err) => err_json(err),
                    }
                }
            })),
        )?;
    }
    {
        let host = host.clone();
        globals.set(
            "__exists",
            Func::from(Async(move |path: String| {
                let host = host.clone();
                async move { ok_json(json!(host::exists(&host, &path).await)) }
            })),
        )?;
    }

    // network
    {
        let host = host.clone();
        globals.set(
            "__fetch",
            Func::from(Async(move |url: String, opts: String| {
                let host = host.clone();
                async move {
                    match host::fetch(&host, &url, &parse_opts(&opts)).await {
                        Ok(value) => ok_json(value),
                        Err(err) => err_json(err),
                    }
                }
            })),
        )?;
    }

    // time
    globals.set(
        "__sleep",
        Func::from(Async(move |ms: f64| async move {
            let ms = (ms.max(0.0) as u64).min(MAX_SLEEP_MS);
            tokio::time::sleep(Duration::from_millis(ms)).await;
        })),
    )?;

    // scheduling + notification
    {
        let host = host.clone();
        globals.set(
            "__decide",
            Func::from(move |payload: String| decide(&host, &payload)),
        )?;
    }
    {
        let host = host.clone();
        globals.set(
            "__notify",
            Func::from(move |message: String, level: String| {
                host.notifications
                    .lock()
                    .expect("notify mutex poisoned")
                    .push((level.clone(), message.clone()));
                let _ = host
                    .log
                    .append(crate::event::EventKind::Notify { level, message });
            }),
        )?;
    }

    // browser (CDP). Rust only moves protocol messages; the ergonomics live in
    // the prelude so they can be changed without a recompile.
    {
        let host = host.clone();
        globals.set(
            "__cdp_send",
            Func::from(Async(
                move |method: String, params: String, session: String| {
                    let host = host.clone();
                    async move {
                        match cdp_send(&host, &method, &params, &session).await {
                            Ok(value) => ok_json(value),
                            Err(err) => err_json(err),
                        }
                    }
                },
            )),
        )?;
    }
    {
        let host = host.clone();
        globals.set(
            "__cdp_wait",
            Func::from(Async(
                move |method: String, session: String, timeout_ms: f64| {
                    let host = host.clone();
                    async move {
                        let client = match host.browser.ensure().await {
                            Ok(client) => client,
                            Err(err) => return err_json(err),
                        };
                        let session = (!session.is_empty()).then_some(session);
                        let event = client
                            .wait_for(
                                &method,
                                session.as_deref(),
                                Duration::from_millis(timeout_ms.max(0.0) as u64),
                            )
                            .await;
                        ok_json(match event {
                            Some(event) => {
                                json!({ "method": event.method, "params": event.params })
                            }
                            None => JsonValue::Null,
                        })
                    }
                },
            )),
        )?;
    }
    {
        let host = host.clone();
        globals.set(
            "__cdp_control",
            Func::from(Async(move |action: String, argument: String| {
                let host = host.clone();
                async move {
                    match cdp_control(&host, &action, &argument).await {
                        Ok(value) => ok_json(value),
                        Err(err) => err_json(err),
                    }
                }
            })),
        )?;
    }

    // constants
    globals.set("GOAL_DIR", host.goal_dir.to_string_lossy().to_string())?;
    globals.set(
        "LEDGER",
        host.goal_dir
            .join("ledger.md")
            .to_string_lossy()
            .to_string(),
    )?;
    globals.set("CWD", host.cwd.to_string_lossy().to_string())?;

    ctx.eval::<(), _>(include_str!("prelude.js"))
        .map_err(|e| anyhow!("prelude failed to load: {e}"))?;

    Ok(())
}

/// Forward one CDP command, connecting to a browser if we are not already.
async fn cdp_send(
    host: &Arc<HostContext>,
    method: &str,
    params: &str,
    session: &str,
) -> Result<serde_json::Value> {
    let client = host.browser.ensure().await?;
    let params = serde_json::from_str(params).unwrap_or_else(|_| json!({}));
    let session = (!session.is_empty()).then_some(session);
    client
        .send(method, params, session, crate::browser::DEFAULT_TIMEOUT)
        .await
}

/// Operations whose implementation differs by transport.
async fn cdp_control(
    host: &Arc<HostContext>,
    action: &str,
    argument: &str,
) -> Result<serde_json::Value> {
    // `status` must not force a connection — it is how the model asks whether
    // a browser is available at all.
    if action == "status" {
        return Ok(host.browser.status().await);
    }

    let client = host.browser.ensure().await?;
    match action {
        "targets" => crate::browser::targets(&client).await,
        "attach" => Ok(json!({ "session": crate::browser::attach(&client, argument).await? })),
        "new_tab" => crate::browser::new_tab(&client, argument).await,
        "activate" => crate::browser::activate(&client, argument).await,
        other => Err(anyhow!("unknown browser action `{other}`")),
    }
}

fn proc_action(
    host: &Arc<HostContext>,
    name: &str,
    action: &str,
    args: &serde_json::Value,
) -> String {
    let procs = host.procs.lock().expect("procs mutex poisoned");
    if action == "list" {
        let listed: Vec<serde_json::Value> = procs
            .values()
            .map(|p| {
                json!({
                    "name": p.name,
                    "command": p.command,
                    "running": p.running(),
                    "started": p.started.to_rfc3339(),
                })
            })
            .collect();
        return ok_json(json!(listed));
    }

    let Some(proc) = procs.get(name) else {
        return err_json(format!("no such process `{name}`"));
    };
    match action {
        "running" => ok_json(json!(proc.running())),
        "exit" => ok_json(json!(proc.exit().flatten())),
        "output" => {
            let lines = args.get("lines").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
            ok_json(json!(proc.output(lines)))
        }
        "kill" => match proc.kill() {
            Ok(()) => ok_json(json!(true)),
            Err(err) => err_json(err),
        },
        other => err_json(format!("unknown process action `{other}`")),
    }
}

/// Record how the model chose to end this wake.
fn decide(host: &Arc<HostContext>, payload: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return;
    };
    let note = value
        .get("note")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let decision = match value.get("type").and_then(|v| v.as_str()) {
        Some("wake_in") => {
            let ms = value.get("ms").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
            WakeDecision::At {
                at: Utc::now() + chrono::Duration::milliseconds(ms),
                note,
            }
        }
        Some("wake_at") => {
            let at = value
                .get("at")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(5));
            WakeDecision::At { at, note }
        }
        Some("on_exit") => {
            let name = value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            WakeDecision::OnExit { name, note }
        }
        Some("done") => WakeDecision::Done {
            summary: value
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        },
        _ => return,
    };
    host.set_decision(decision);
}
