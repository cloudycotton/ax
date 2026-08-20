//! Chrome DevTools Protocol transport.
//!
//! Rust does nothing here but move messages: send a command, correlate the
//! reply, and buffer events. Every ergonomic concern — sessions, frames,
//! waiting for a page to settle, accessibility snapshots — lives in
//! `prelude.js`, where it can be changed without a recompile and read by the
//! model itself.
//!
//! Two transports produce the same interface:
//!
//! - **Relay** (primary): a Chrome extension speaking `chrome.debugger` in the
//!   user's real, logged-in browser, connected to us over a local WebSocket.
//!   This is the only way to reach the default profile — Chrome 136+ refuses
//!   `--remote-debugging-port` there precisely because that profile holds
//!   valuable credentials.
//! - **Direct** (fallback): a Chrome instance we launch ourselves on a
//!   dedicated profile with a debugging port. No extension needed, but it does
//!   not have the user's logins until they sign in once.

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// A protocol event, flattened so JavaScript can match on it simply.
#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

/// Commands awaiting a reply, keyed by the id we sent them with.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A live CDP connection.
pub struct CdpClient {
    next_id: AtomicU64,
    pending: Pending,
    outgoing: mpsc::UnboundedSender<String>,
    events: broadcast::Sender<CdpEvent>,
    /// Which transport this connection came from, for reporting.
    pub kind: &'static str,
}

impl CdpClient {
    /// Connect to a CDP WebSocket endpoint by dialling out (direct transport).
    pub async fn connect(url: &str, kind: &'static str) -> Result<Arc<Self>> {
        let (stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("could not open a CDP connection to {url}"))?;
        let (mut writer, mut reader) = stream.split();

        let (to_peer, mut outbox) = mpsc::unbounded_channel::<String>();
        let (inbox, from_peer) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(text) = outbox.recv().await {
                if writer.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(Ok(message)) = reader.next().await {
                let text = match message {
                    Message::Text(text) => text.to_string(),
                    Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                if inbox.send(text).is_err() {
                    break;
                }
            }
        });

        Ok(Self::from_channels(to_peer, from_peer, kind))
    }

    /// Build a client over an already-established message channel. The relay
    /// uses this: there the extension dials in to us, so there is no URL to
    /// connect to, but the framing is identical.
    pub fn from_channels(
        to_peer: mpsc::UnboundedSender<String>,
        mut from_peer: mpsc::UnboundedReceiver<String>,
        kind: &'static str,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(2048);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let client = Arc::new(Self {
            next_id: AtomicU64::new(1),
            pending: pending.clone(),
            outgoing: to_peer,
            events: events.clone(),
            kind,
        });

        // Replies go to their waiter, everything else is an event.
        tokio::spawn(async move {
            while let Some(text) = from_peer.recv().await {
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };

                if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                    if let Some(waiter) = pending.lock().await.remove(&id) {
                        let reply = match value.get("error") {
                            Some(error) => Err(error.to_string()),
                            None => Ok(value.get("result").cloned().unwrap_or(json!({}))),
                        };
                        let _ = waiter.send(reply);
                    }
                    continue;
                }

                if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
                    let _ = events.send(CdpEvent {
                        method: method.to_string(),
                        params: value.get("params").cloned().unwrap_or(json!({})),
                        session_id: value
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    });
                }
            }
            // Connection closed: fail anything still waiting rather than
            // leaving the model hanging on a promise that can never resolve.
            let mut pending = pending.lock().await;
            for (_, waiter) in pending.drain() {
                let _ = waiter.send(Err("the CDP connection closed".to_string()));
            }
        });

        client
    }

    /// Issue a command and wait for its reply.
    pub async fn send(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session_id {
            message["sessionId"] = json!(session);
        }

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.outgoing
            .send(message.to_string())
            .map_err(|_| anyhow!("the CDP connection is closed"))?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => bail!("{method} failed: {error}"),
            Ok(Err(_)) => bail!("{method}: the connection dropped before replying"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("{method}: timed out after {timeout:?}")
            }
        }
    }

    /// Wait for the next event matching `method`, optionally scoped to a
    /// session. Returns `None` on timeout.
    pub async fn wait_for(
        &self,
        method: &str,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Option<CdpEvent> {
        let mut receiver = self.events.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, receiver.recv()).await {
                Ok(Ok(event)) => {
                    let method_matches = event.method == method;
                    let session_matches = match session_id {
                        Some(want) => event.session_id.as_deref() == Some(want),
                        None => true,
                    };
                    if method_matches && session_matches {
                        return Some(event);
                    }
                }
                // Lagged past some events; keep waiting rather than failing.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                Err(_) => return None,
            }
        }
    }

    /// Whether the connection is still usable.
    pub fn is_open(&self) -> bool {
        !self.outgoing.is_closed()
    }
}
