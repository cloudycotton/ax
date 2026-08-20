//! The supervisor.
//!
//! One daemon per machine owns every running session. It exists so that goals
//! outlive the terminal that started them: you launch a session, close the
//! window, and reattach a day later to the whole story. It also owns the single
//! browser relay, since only one process can hold that port.
//!
//! An idle daemon costs nothing. Sessions asleep on a timer are just pending
//! futures; no model is called until something actually wakes one.

use crate::agent::{Agent, Limits};
use crate::chrome::BrowserManager;
use crate::event::{self, EventLog};
use crate::ipc::{Request, Response};
use crate::llm::{LlmClient, LlmConfig};
use crate::paths;
use crate::relay::{self, Relay};
use crate::session::{Session, SessionMeta, Status};
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

/// What the daemon keeps for each supervised session.
struct Supervised {
    meta_dir: PathBuf,
    log: Arc<EventLog>,
    inbox: mpsc::UnboundedSender<String>,
    task: tokio::task::JoinHandle<()>,
}

pub struct Daemon {
    sessions: Mutex<HashMap<String, Supervised>>,
    browser: Arc<BrowserManager>,
}

impl Daemon {
    /// Start the daemon: bind the control socket, take the browser relay, and
    /// resume any session that was still in flight.
    pub async fn start() -> Result<()> {
        let home = paths::agent_home()?;
        paths::ensure_dir(&home)?;

        let socket_path = paths::daemon_socket()?;
        // A socket file left behind by a crash would block the bind, but one
        // that still answers means a daemon is already running.
        if socket_path.exists() {
            if UnixStream::connect(&socket_path).await.is_ok() {
                anyhow::bail!("a daemon is already running");
            }
            let _ = std::fs::remove_file(&socket_path);
        }

        let relay = Relay::new(&home, relay::DEFAULT_RELAY_PORT)?;
        if let Err(err) = relay.listen().await {
            eprintln!("browser relay unavailable: {err}");
        }
        let browser = Arc::new(BrowserManager::new(home, relay));

        let daemon = Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            browser,
        });

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("could not bind {}", socket_path.display()))?;
        eprintln!("ax daemon listening on {}", socket_path.display());

        daemon.resume_all().await;

        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let daemon = daemon.clone();
            tokio::spawn(async move {
                if let Err(err) = daemon.serve(stream).await {
                    eprintln!("client error: {err}");
                }
            });
        }
    }

    /// Pick up sessions that were running or sleeping when we last stopped.
    ///
    /// Their isolates are gone, which is exactly why the ledger and the files
    /// on disk are the durable state; the model is told it restarted and reads
    /// its ledger to work out where it was.
    async fn resume_all(self: &Arc<Self>) {
        let candidates = match Session::list() {
            Ok(list) => list,
            Err(err) => {
                eprintln!("could not enumerate sessions: {err}");
                return;
            }
        };
        for meta in candidates {
            if !matches!(meta.status, Status::Running | Status::Sleeping) {
                continue;
            }
            match self.spawn_session(&meta.id, None, None).await {
                Ok(()) => eprintln!("resumed session {}", meta.id),
                Err(err) => eprintln!("could not resume {}: {err}", meta.id),
            }
        }
    }

    /// Load a session and run its wake loop as a supervised task.
    async fn spawn_session(
        self: &Arc<Self>,
        id: &str,
        model: Option<String>,
        limits: Option<Limits>,
    ) -> Result<()> {
        if self.sessions.lock().await.contains_key(id) {
            return Err(anyhow!("session {id} is already running"));
        }

        let session = Session::load(id)?;
        let config = LlmConfig::from_env(model.or_else(|| Some(session.meta.model.clone())))?;
        let client = LlmClient::new(config)?;
        let log = session.log.clone();
        let meta_dir = session.dir.clone();

        let mut agent = Agent::new(
            session,
            client,
            limits.unwrap_or_default(),
            self.browser.clone(),
        )?;
        let inbox = agent.inbox();

        let id_owned = id.to_string();
        let daemon = self.clone();
        let task = tokio::spawn(async move {
            if let Err(err) = agent.run().await {
                eprintln!("session {id_owned} stopped: {err}");
            }
            // Drop the registry entry so the id can be started again.
            daemon.sessions.lock().await.remove(&id_owned);
        });

        self.sessions.lock().await.insert(
            id.to_string(),
            Supervised {
                meta_dir,
                log,
                inbox,
                task,
            },
        );
        Ok(())
    }

    async fn serve(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let request: Request = match serde_json::from_str(&line) {
                Ok(request) => request,
                Err(err) => {
                    send(&mut writer, &Response::error(err)).await?;
                    continue;
                }
            };

            match request {
                Request::Ping => {
                    let sessions = self.sessions.lock().await.len();
                    send(
                        &mut writer,
                        &Response::Pong {
                            version: env!("CARGO_PKG_VERSION").to_string(),
                            sessions,
                        },
                    )
                    .await?;
                }

                Request::Create {
                    goal,
                    cwd,
                    model,
                    max_wakes,
                    max_tokens,
                } => {
                    let response = self
                        .create(goal, cwd, model, max_wakes, max_tokens)
                        .await
                        .map(|id| Response::Created { id })
                        .unwrap_or_else(Response::error);
                    send(&mut writer, &response).await?;
                }

                Request::Attach { id, from_seq } => {
                    // Streams until the client disconnects; nothing else can be
                    // read from this connection afterwards.
                    return self.attach(&id, from_seq, writer).await;
                }

                Request::Say { id, text } => {
                    let response = match self.sessions.lock().await.get(&id) {
                        Some(supervised) => match supervised.inbox.send(text) {
                            Ok(()) => Response::Ok,
                            Err(_) => {
                                Response::error("that session is no longer accepting messages")
                            }
                        },
                        None => Response::error(format!("session {id} is not running")),
                    };
                    send(&mut writer, &response).await?;
                }

                Request::List => {
                    let sessions = Session::list().unwrap_or_default();
                    send(&mut writer, &Response::Sessions { sessions }).await?;
                }

                Request::Stop { id } => {
                    let response = match self.sessions.lock().await.remove(&id) {
                        Some(supervised) => {
                            supervised.task.abort();
                            let _ = mark_stopped(&supervised.meta_dir);
                            Response::Ok
                        }
                        None => Response::error(format!("session {id} is not running")),
                    };
                    send(&mut writer, &response).await?;
                }

                Request::Shutdown => {
                    send(&mut writer, &Response::Ok).await?;
                    let _ = writer.flush().await;
                    // Sessions are all restartable from disk, so exiting here
                    // loses nothing but the in-memory isolates.
                    std::process::exit(0);
                }
            }
        }
        Ok(())
    }

    async fn create(
        self: &Arc<Self>,
        goal: String,
        cwd: String,
        model: Option<String>,
        max_wakes: Option<u64>,
        max_tokens: Option<u64>,
    ) -> Result<String> {
        // Resolve the model now so a bad configuration is reported to the
        // client rather than buried in the daemon's log.
        let config = LlmConfig::from_env(model.clone())?;
        let session = Session::create(&goal, std::path::Path::new(&cwd), &config.model)?;
        let id = session.meta.id.clone();
        drop(session);

        let limits = Limits {
            max_wakes,
            max_tokens,
            ..Default::default()
        };
        self.spawn_session(&id, model, Some(limits)).await?;
        Ok(id)
    }

    /// Replay a session's history, then forward events as they happen. This is
    /// the whole point of the daemon: the log on disk is complete, so attaching
    /// late still shows everything that happened.
    async fn attach(
        self: &Arc<Self>,
        id: &str,
        from_seq: u64,
        mut writer: tokio::net::unix::OwnedWriteHalf,
    ) -> Result<()> {
        // Subscribe before replaying so nothing that lands mid-replay is lost.
        let live = self
            .sessions
            .lock()
            .await
            .get(id)
            .map(|supervised| supervised.log.subscribe());

        let dir = paths::session_dir(id)?;
        let log_path = dir.join("events.jsonl");
        if !log_path.exists() {
            send(
                &mut writer,
                &Response::error(format!("no such session {id}")),
            )
            .await?;
            return Ok(());
        }

        let mut last_sent = None;
        for logged in event::read_all(&log_path)? {
            if logged.seq < from_seq {
                continue;
            }
            last_sent = Some(logged.seq);
            send(
                &mut writer,
                &Response::Event {
                    event: Box::new(logged),
                },
            )
            .await?;
        }

        let Some(mut live) = live else {
            // Not running: the replay above is the whole story.
            return Ok(());
        };

        loop {
            match live.recv().await {
                Ok(logged) => {
                    // Skip anything the replay already covered.
                    if last_sent.is_some_and(|seq| logged.seq <= seq) {
                        continue;
                    }
                    last_sent = Some(logged.seq);
                    if send(
                        &mut writer,
                        &Response::Event {
                            event: Box::new(logged),
                        },
                    )
                    .await
                    .is_err()
                    {
                        // The client detached.
                        return Ok(());
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return Ok(()),
            }
        }
    }
}

/// Record that a session is no longer supervised, so `ax ls` does not claim it
/// is still running and a later daemon does not try to resume it.
fn mark_stopped(dir: &std::path::Path) -> Result<()> {
    let path = dir.join("meta.json");
    let raw = std::fs::read_to_string(&path)?;
    let mut meta: SessionMeta = serde_json::from_str(&raw)?;
    meta.status = Status::Failed;
    std::fs::write(&path, serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

async fn send(writer: &mut tokio::net::unix::OwnedWriteHalf, response: &Response) -> Result<()> {
    let mut line = serde_json::to_string(response)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Client side: send one request and read its single reply.
///
/// The daemon keeps the connection open for further commands, so this must not
/// read to end-of-stream — it would block until the daemon shut down.
pub async fn request(request: &Request) -> Result<Response> {
    let (reader, mut writer) = dial(request).await?;
    let mut lines = BufReader::new(reader).lines();
    match lines.next_line().await? {
        Some(line) => {
            let response = serde_json::from_str(&line)?;
            // Politely end the conversation.
            let _ = writer.shutdown().await;
            Ok(response)
        }
        None => Err(anyhow!("the daemon closed the connection without replying")),
    }
}

/// Client side: send one request and collect replies until the daemon closes
/// the connection. Only `Attach` streams like this.
pub async fn request_stream(request: &Request) -> Result<Vec<Response>> {
    let (reader, _writer) = dial(request).await?;
    let mut responses = Vec::new();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        responses.push(serde_json::from_str(&line)?);
    }
    Ok(responses)
}

async fn dial(
    request: &Request,
) -> Result<(
    tokio::net::unix::OwnedReadHalf,
    tokio::net::unix::OwnedWriteHalf,
)> {
    let socket_path = paths::daemon_socket()?;
    let stream = UnixStream::connect(&socket_path)
        .await
        .with_context(|| "the ax daemon is not running (start it with `ax daemon`)")?;
    let (reader, mut writer) = stream.into_split();

    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok((reader, writer))
}

/// Is a daemon answering right now?
pub async fn is_running() -> bool {
    let Ok(path) = paths::daemon_socket() else {
        return false;
    };
    UnixStream::connect(&path).await.is_ok()
}
