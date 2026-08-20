//! The bridge to the user's real browser.
//!
//! A Chrome extension holds a `chrome.debugger` session against the user's
//! actual tabs and relays the protocol to us over a WebSocket on loopback. This
//! is the only supported way into the default profile — the one with their
//! logins — since Chrome 136 stopped honouring `--remote-debugging-port` there.
//!
//! **This socket is a capability.** Anything that can talk to it can drive a
//! fully logged-in browser, so it is not open by default: the listener binds
//! loopback only, requires a pairing token generated at install time, and
//! rejects connections whose `Origin` is not a Chrome extension. Without those
//! three checks, any process on the machine could read the user's mail.

use crate::cdp::CdpClient;
use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

/// Fixed so the extension knows where to look without any configuration.
pub const DEFAULT_RELAY_PORT: u16 = 8317;

pub struct Relay {
    token: String,
    port: u16,
    connected: AtomicBool,
    client: Mutex<Option<Arc<CdpClient>>>,
}

impl Relay {
    /// Load (or create) the pairing token for this machine.
    pub fn new(agent_home: &Path, port: u16) -> Result<Arc<Self>> {
        let token = load_or_create_token(agent_home)?;
        Ok(Arc::new(Self {
            token,
            port,
            connected: AtomicBool::new(false),
            client: Mutex::new(None),
        }))
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Is a paired extension connected right now?
    pub fn has_extension(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// The CDP client for the connected extension.
    pub async fn connect(&self) -> Result<Arc<CdpClient>> {
        self.client
            .lock()
            .await
            .clone()
            .filter(|client| client.is_open())
            .ok_or_else(|| anyhow!("the browser extension is not connected"))
    }

    /// Start listening for the extension. Binds loopback only.
    pub async fn listen(self: &Arc<Self>) -> Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", self.port))
            .await
            .with_context(|| format!("could not listen on 127.0.0.1:{}", self.port))?;

        let relay = self.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    continue;
                };
                let relay = relay.clone();
                tokio::spawn(async move {
                    if let Err(err) = relay.serve(socket).await {
                        eprintln!("\x1b[33m! browser extension rejected: {err}\x1b[0m");
                    }
                });
            }
        });
        Ok(())
    }

    async fn serve(self: Arc<Self>, socket: tokio::net::TcpStream) -> Result<()> {
        // Only a Chrome extension page may open this socket; a web page that
        // happened to learn the port must not be able to.
        let mut origin_ok = false;
        let stream = tokio_tungstenite::accept_hdr_async(
            socket,
            |request: &Request, response: Response| {
                if let Some(origin) = request.headers().get("origin") {
                    let origin = origin.to_str().unwrap_or_default();
                    origin_ok = origin.starts_with("chrome-extension://");
                }
                Ok(response)
            },
        )
        .await
        .context("websocket handshake failed")?;

        if !origin_ok {
            bail!("connection did not come from a browser extension");
        }

        let (mut writer, mut reader) = stream.split();

        // First frame must be the pairing handshake.
        let hello = match reader.next().await {
            Some(Ok(Message::Text(text))) => text.to_string(),
            _ => bail!("extension did not send a handshake"),
        };
        let hello: serde_json::Value =
            serde_json::from_str(&hello).context("handshake was not valid JSON")?;
        let presented = hello.get("token").and_then(|v| v.as_str()).unwrap_or("");
        if !constant_time_eq(presented, &self.token) {
            let _ = writer
                .send(Message::Text(
                    r#"{"ok":false,"error":"bad token"}"#.to_string().into(),
                ))
                .await;
            bail!("extension presented an invalid pairing token");
        }
        writer
            .send(Message::Text(r#"{"ok":true}"#.to_string().into()))
            .await?;

        let (to_peer, mut outbox) = mpsc::unbounded_channel::<String>();
        let (inbox, from_peer) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(text) = outbox.recv().await {
                if writer.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });

        let client = CdpClient::from_channels(to_peer, from_peer, "extension");
        *self.client.lock().await = Some(client);
        self.connected.store(true, Ordering::SeqCst);
        eprintln!("\x1b[32m✓ browser extension connected\x1b[0m");

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

        self.connected.store(false, Ordering::SeqCst);
        *self.client.lock().await = None;
        eprintln!("\x1b[33m! browser extension disconnected\x1b[0m");
        Ok(())
    }
}

/// The pairing token lives in a 0600 file; the user copies it into the
/// extension once at install time.
fn load_or_create_token(agent_home: &Path) -> Result<String> {
    let path = token_path(agent_home);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }

    let token = random_token();
    std::fs::create_dir_all(agent_home)?;
    std::fs::write(&path, &token)
        .with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(token)
}

pub fn token_path(agent_home: &Path) -> PathBuf {
    agent_home.join("relay-token")
}

/// 256 bits from the OS CSPRNG. This guards a logged-in browser, so it must not
/// come from a time-seeded PRNG.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn getrandom(buf: &mut [u8]) {
    use std::io::Read;
    // /dev/urandom is the portable CSPRNG on every platform this runs on.
    let mut file = std::fs::File::open("/dev/urandom").expect("no /dev/urandom");
    file.read_exact(buf).expect("could not read /dev/urandom");
}

/// Compare without leaking length-prefix information through timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_and_unique() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b, "token generation must not be deterministic");
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
    }
}
