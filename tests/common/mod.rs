//! A scripted OpenAI-compatible server, shared by the integration tests.
//!
//! It lets the whole harness be exercised — SSE parsing, tool-call assembly,
//! execution, scheduling — without credentials or network access.

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
