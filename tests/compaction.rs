//! A wake that runs long must not die.
//!
//! The scripted server here asserts on what it receives: every request must
//! stay under the context ceiling, and every tool call must still be answered
//! by a tool result. Those are the two ways compaction can go wrong — silently
//! growing past the window, or splitting a call from its answer and getting the
//! whole request rejected by the provider.

mod common;

use ax::agent::{Agent, Limits};
use ax::llm::{LlmClient, LlmConfig};
use ax::session::{Session, Status};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Rough token estimate, matching the harness's own approximation.
fn estimate(body: &Value) -> usize {
    body.get("messages")
        .and_then(|m| m.as_array())
        .map(|messages| {
            messages
                .iter()
                .map(|m| m.to_string().len().div_ceil(4))
                .sum::<usize>()
        })
        .unwrap_or(0)
}

/// Every `tool` message must answer a `tool_calls` entry that came before it,
/// and no tool call may be left dangling. Providers reject either.
fn pairing_is_intact(body: &Value) -> Result<(), String> {
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or("no messages")?;

    let mut announced: Vec<String> = Vec::new();
    let mut answered: Vec<String> = Vec::new();
    for message in messages {
        if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
            for call in calls {
                if let Some(id) = call.get("id").and_then(|i| i.as_str()) {
                    announced.push(id.to_string());
                }
            }
        }
        if message.get("role").and_then(|r| r.as_str()) == Some("tool") {
            let id = message
                .get("tool_call_id")
                .and_then(|i| i.as_str())
                .unwrap_or_default()
                .to_string();
            if !announced.contains(&id) {
                return Err(format!("tool result `{id}` has no matching call"));
            }
            answered.push(id);
        }
    }
    for id in &announced {
        if !answered.contains(id) {
            return Err(format!("tool call `{id}` was never answered"));
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_long_wake_stays_inside_the_context_window() {
    common::test_home();

    const BUDGET: usize = 12_000;
    // Safety: the test harness sets this before the agent reads it.
    unsafe { std::env::set_var("AX_CONTEXT_TOKENS", BUDGET.to_string()) };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let breach = Arc::new(std::sync::Mutex::new(Option::<String>::None));

    {
        let (requests, peak, breach) = (requests.clone(), peak.clone(), breach.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };

                // Read the whole request: the body is what we assert on.
                let mut raw = Vec::new();
                let mut buf = vec![0u8; 65536];
                loop {
                    match socket.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            raw.extend_from_slice(&buf[..n]);
                            let text = String::from_utf8_lossy(&raw);
                            if let Some(start) = text.find("\r\n\r\n") {
                                let headers = &text[..start];
                                let length = headers
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
                let count = requests.fetch_add(1, Ordering::SeqCst);
                if let Some(start) = text.find("\r\n\r\n")
                    && let Ok(body) = serde_json::from_str::<Value>(&text[start + 4..])
                {
                    {
                        let tokens = estimate(&body);
                        peak.fetch_max(tokens, Ordering::SeqCst);
                        if tokens > BUDGET * 2 {
                            *breach.lock().unwrap() =
                                Some(format!("request {count} carried ~{tokens} tokens"));
                        }
                        if let Err(problem) = pairing_is_intact(&body) {
                            *breach.lock().unwrap() = Some(problem);
                        }
                    }
                }

                // Twelve turns of bulky output, then finish. Each result is
                // large enough that an uncompacted transcript would balloon.
                let code = if count < 12 {
                    format!("return 'block {count}: ' + 'y'.repeat(9000);")
                } else {
                    "done(\"long wake survived\");".to_string()
                };
                let chunk = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": format!("call-{count}"),
                                "type": "function",
                                "function": { "name": "run_js", "arguments": json!({"code": code}).to_string() }
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
    }

    let config = LlmConfig {
        base_url: format!("http://{addr}/v1"),
        api_key: "test".into(),
        model: "mock-model".into(),
        max_tokens: None,
        temperature: None,
        include_usage: false,
    };

    let cwd = std::env::current_dir().unwrap();
    let session = Session::create("survive a long wake", &cwd, "mock-model").unwrap();
    let id = session.meta.id.clone();
    let mut agent = Agent::new(
        session,
        LlmClient::new(config).unwrap(),
        Limits::default(),
        common::browser(18421),
    )
    .unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(120), agent.run()).await;
    assert!(outcome.is_ok(), "the wake loop hung");
    outcome.unwrap().unwrap();

    if let Some(problem) = breach.lock().unwrap().clone() {
        panic!("compaction produced an invalid or oversized request: {problem}");
    }

    let reloaded = Session::load(&id).unwrap();
    assert_eq!(
        reloaded.meta.status,
        Status::Done,
        "the goal should have completed despite a long wake"
    );
    assert_eq!(reloaded.meta.wakes, 1, "this should all be one wake");

    // Uncompacted, thirteen turns of 9KB output would be ~30k tokens; the
    // ceiling here is 12k, so this only passes if compaction ran.
    let observed = peak.load(Ordering::SeqCst);
    assert!(
        observed <= BUDGET * 2,
        "requests grew to ~{observed} tokens against a {BUDGET} ceiling"
    );

    let events = ax::event::read_all(reloaded.log.path()).unwrap();
    let compacted = events
        .iter()
        .any(|e| matches!(&e.kind, ax::event::EventKind::Compacted { .. }));
    assert!(compacted, "compaction should be recorded in the event log");
}
