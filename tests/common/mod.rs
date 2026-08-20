//! A scripted OpenAI-compatible server, shared by the integration tests.
//!
//! It lets the whole harness be exercised — SSE parsing, tool-call assembly,
//! execution, scheduling — without credentials or network access.

use ax::chrome::BrowserManager;
use ax::relay::Relay;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One scripted assistant turn.
// Not every test binary uses every variant.
#[allow(dead_code)]
pub enum Turn {
    /// Emit a proper tool call running this code.
    ToolCall(&'static str),
    /// Emit prose only — exercises the fenced-code fallback path.
    Text(&'static str),
}

/// Serve the given turns, one per request, and return the base URL.
#[allow(dead_code)]
pub async fn scripted_server(turns: Vec<Turn>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let mut index = 0usize;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };

            // We do not inspect the request body; only that a request arrived.
            let mut buf = vec![0u8; 65536];
            let _ = socket.read(&mut buf).await;

            let chunk = match turns.get(index) {
                Some(Turn::ToolCall(code)) => json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": format!("call-{index}"),
                                "type": "function",
                                "function": {
                                    "name": "run_js",
                                    "arguments": json!({ "code": code }).to_string()
                                }
                            }]
                        }
                    }]
                }),
                Some(Turn::Text(text)) => json!({
                    "choices": [{ "delta": { "content": text } }]
                }),
                // Ran out of script: an empty turn.
                None => json!({ "choices": [{ "delta": { "content": "" } }] }),
            };
            index += 1;

            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    format!("http://{addr}/v1")
}
/// A browser manager the tests never connect through. Each gets its own relay
/// port so the suite can run in parallel.
#[allow(dead_code)]
pub fn browser(port: u16) -> std::sync::Arc<BrowserManager> {
    let home = std::env::temp_dir().join("ax-tests");
    let relay = Relay::new(&home, port).unwrap();
    std::sync::Arc::new(BrowserManager::new(home, relay))
}

/// Point AX_HOME at a scratch directory so tests never touch ~/.ax.
///
/// Set exactly once for the whole test binary: `set_var` is process-global, so
/// tests running concurrently must not each point it somewhere different.
/// Sessions get distinct ids, so sharing one home is safe.
#[allow(dead_code)]
pub fn test_home() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join("ax-tests");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Safety: this runs before any agent code reads the variable, and the
        // Once guarantees no concurrent writer.
        unsafe { std::env::set_var("AX_HOME", &dir) };
    });
}
