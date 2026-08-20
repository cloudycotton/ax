//! Images reaching the model, and being refused when they cannot.
//!
//! Two failures matter here. A sighted model must actually receive the image
//! as a content part — attaching it anywhere the provider ignores would be
//! silent and useless. A blind model must be told plainly, rather than
//! spending wakes producing screenshots nobody reads.

mod common;

use ax::agent::{Agent, Limits};
use ax::llm::{LlmClient, LlmConfig};
use ax::session::Session;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The smallest valid PNG, written where the agent can reach it.
fn write_pixel(dir: &std::path::Path) -> std::path::PathBuf {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
        .unwrap();
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("pixel.png");
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Serve two turns — the code under test, then `done` — capturing every
/// request body so the test can inspect what the model was actually sent.
async fn server(code: String, seen: Arc<std::sync::Mutex<Vec<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = vec![0u8; 65536];
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&raw);
                        if let Some(start) = text.find("\r\n\r\n") {
                            let length = text[..start]
                                .lines()
                                .find_map(|line| {
                                    line.to_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if text.len() - (start + 4) >= length {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }

            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some(start) = text.find("\r\n\r\n")
                && let Ok(body) = serde_json::from_str::<Value>(&text[start + 4..])
            {
                seen.lock().unwrap().push(body);
            }

            let index = count.fetch_add(1, Ordering::SeqCst);
            let next = if index == 0 {
                code.clone()
            } else {
                "done(\"finished\");".to_string()
            };
            let chunk = json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": format!("call-{index}"),
                            "type": "function",
                            "function": { "name": "run_js", "arguments": json!({"code": next}).to_string() }
                        }]
                    }
                }]
            });
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

async fn run_with(model: &str, code: String, port: u16) -> Vec<Value> {
    common::test_home();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let base_url = server(code, seen.clone()).await;

    let config = LlmConfig {
        base_url,
        api_key: "test".into(),
        model: model.into(),
        max_tokens: None,
        temperature: None,
        include_usage: false,
    };
    let cwd = std::env::current_dir().unwrap();
    let session = Session::create("look at an image", &cwd, model).unwrap();
    let mut agent = Agent::new(
        session,
        LlmClient::new(config).unwrap(),
        Limits::default(),
        common::browser(port),
    )
    .unwrap();

    tokio::time::timeout(Duration::from_secs(60), agent.run())
        .await
        .expect("the wake loop hung")
        .unwrap();

    seen.lock().unwrap().clone()
}

#[tokio::test(flavor = "current_thread")]
async fn a_sighted_model_receives_the_image() {
    let dir = std::env::temp_dir().join("ax-vision-test");
    let path = write_pixel(&dir);

    let requests = run_with(
        "gpt-4o",
        format!(
            "see({});",
            serde_json::to_string(&path.to_string_lossy()).unwrap()
        ),
        18431,
    )
    .await;

    // The follow-up request must carry the image as a content part.
    let image = requests
        .iter()
        .flat_map(|body| {
            body["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
        })
        .find_map(|message| {
            let parts = message.get("content")?.as_array()?.clone();
            parts
                .into_iter()
                .find(|part| part["type"] == json!("image_url"))
        });

    let image = image.expect("no image_url part was ever sent to the model");
    let url = image["image_url"]["url"].as_str().unwrap_or_default();
    assert!(
        url.starts_with("data:image/png;base64,"),
        "image was not sent as a png data URI: {}",
        &url[..url.len().min(40)]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn a_blind_model_is_told_to_use_the_dom_instead() {
    let dir = std::env::temp_dir().join("ax-vision-blind");
    let path = write_pixel(&dir);

    let requests = run_with(
        "llama-3.3-70b-versatile",
        format!(
            "try {{ see({}); }} catch (e) {{ return 'refused: ' + e.message; }}",
            serde_json::to_string(&path.to_string_lossy()).unwrap()
        ),
        18432,
    )
    .await;

    let bodies: String = requests.iter().map(|b| b.to_string()).collect();
    assert!(
        !bodies.contains("image_url"),
        "a blind model must never be sent an image"
    );
    assert!(
        bodies.contains("cannot see images"),
        "the agent should have been told plainly why, and what to do instead"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
