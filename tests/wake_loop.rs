//! End-to-end tests for the wake loop, driven by a scripted OpenAI-compatible
//! server. No credentials and no network required — the point is to prove the
//! whole path works: SSE parsing, tool-call assembly, execution in the isolate,
//! the scheduling decision, and the durable event log.

use as_agent::agent::{Agent, Limits};
use as_agent::chrome::BrowserManager;
use as_agent::relay::Relay;
use as_agent::event::{self, EventKind};
use as_agent::llm::{LlmClient, LlmConfig};
use as_agent::session::{Session, Status};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One scripted assistant turn.
enum Turn {
    /// Emit a proper tool call running this code.
    ToolCall(&'static str),
    /// Emit prose only — exercises the fenced-code fallback path.
    Text(&'static str),
}

/// A minimal HTTP server that replies to each request with the next scripted
/// turn, encoded as an SSE stream in OpenAI's chunk format.
async fn scripted_server(turns: Vec<Turn>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let mut index = 0usize;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };

            // Read the request head; we do not care about the body's contents,
            // only that a request arrived.
            let mut buf = vec![0u8; 65536];
            let _ = socket.read(&mut buf).await;

            let body = match turns.get(index) {
                Some(Turn::ToolCall(code)) => {
                    let arguments = json!({ "code": code }).to_string();
                    let chunk = json!({
                        "choices": [{
                            "delta": {
                                "tool_calls": [{
                                    "index": 0,
                                    "id": format!("call-{index}"),
                                    "type": "function",
                                    "function": { "name": "run_js", "arguments": arguments }
                                }]
                            }
                        }]
                    });
                    format!("data: {chunk}\n\ndata: [DONE]\n\n")
                }
                Some(Turn::Text(text)) => {
                    let chunk = json!({
                        "choices": [{ "delta": { "content": text } }]
                    });
                    format!("data: {chunk}\n\ndata: [DONE]\n\n")
                }
                None => {
                    // Ran out of script: emit an empty turn.
                    let chunk = json!({ "choices": [{ "delta": { "content": "" } }] });
                    format!("data: {chunk}\n\ndata: [DONE]\n\n")
                }
            };
            index += 1;

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

/// A browser manager the tests never actually connect through. Each gets its
/// own relay port so the suite can run in parallel.
fn browser(port: u16) -> std::sync::Arc<BrowserManager> {
    let home = std::env::temp_dir().join("as-agent-tests");
    let relay = Relay::new(&home, port).unwrap();
    std::sync::Arc::new(BrowserManager::new(home, relay))
}

fn config_for(base_url: String) -> LlmConfig {
    LlmConfig {
        base_url,
        api_key: "test-key".into(),
        model: "mock-model".into(),
        max_tokens: None,
        temperature: None,
        include_usage: false,
    }
}

/// Point AGENT_HOME at a scratch directory so tests never touch ~/.agent.
///
/// Set exactly once for the whole test binary: `set_var` is process-global, so
/// tests running concurrently must not each point it somewhere different.
/// Sessions get distinct ids, so sharing one home is safe.
fn test_home() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join("as-agent-tests");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Safety: this runs before any agent code reads the variable, and the
        // Once guarantees no concurrent writer.
        unsafe { std::env::set_var("AGENT_HOME", &dir) };
    });
}

#[tokio::test(flavor = "current_thread")]
async fn runs_a_wake_and_finishes_the_goal() {
    const PORT: u16 = 18411;
    test_home();

    let base_url = scripted_server(vec![
        // First turn: do some real work and record it.
        Turn::ToolCall(
            r#"
            globalThis.findings = [];
            const r = await sh("echo forty-two");
            findings.push(r.stdout.trim());
            console.log("collected", findings.length, "finding");
            return findings;
            "#,
        ),
        // Second turn: close out the goal.
        Turn::ToolCall(r#"done("verified: echo returned forty-two");"#),
    ])
    .await;

    let cwd = std::env::current_dir().unwrap();
    let session = Session::create("prove the harness works", &cwd, "mock-model").unwrap();
    let id = session.meta.id.clone();
    let log_path = session.log.path().to_path_buf();

    let client = LlmClient::new(config_for(base_url)).unwrap();
    let mut agent = Agent::new(session, client, Limits::default(), browser(PORT)).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(60), agent.run()).await;
    assert!(result.is_ok(), "the wake loop hung");
    result.unwrap().unwrap();

    // The session must be marked done on disk...
    let reloaded = Session::load(&id).unwrap();
    assert_eq!(reloaded.meta.status, Status::Done);
    assert_eq!(reloaded.meta.wakes, 1, "should have needed exactly one wake");

    // ...and the event log must contain the full story, in order.
    let events = event::read_all(&log_path).unwrap();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match &e.kind {
            EventKind::WakeStarted { .. } => "wake",
            EventKind::ToolCall { .. } => "call",
            EventKind::Console { .. } => "console",
            EventKind::ToolResult { .. } => "result",
            EventKind::WakeEnded { .. } => "wake_end",
            EventKind::Done { .. } => "done",
            _ => "other",
        })
        .collect();
    assert!(kinds.contains(&"wake"), "no wake recorded: {kinds:?}");
    assert!(kinds.contains(&"console"), "console output was not captured");
    assert!(kinds.contains(&"done"), "goal was never marked done: {kinds:?}");

    // The console line the code printed must be readable after the fact — this
    // is what `agent attach` replays.
    let printed = events.iter().any(|e| {
        matches!(&e.kind, EventKind::Console { text, .. } if text.contains("collected 1 finding"))
    });
    assert!(printed, "the console line was not in the log");

    // Sequence numbers must be dense and ordered: a second writer on the log
    // would show up here as duplicates or gaps.
    for (index, logged) in events.iter().enumerate() {
        assert_eq!(logged.seq, index as u64, "event log sequence is corrupt");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn state_persists_across_wakes_and_fenced_code_runs() {
    const PORT: u16 = 18412;
    test_home();

    let base_url = scripted_server(vec![
        // Wake 1: leave state behind, then sleep briefly.
        Turn::ToolCall(
            r#"
            globalThis.tally = 1;
            wake_in(1000, "check the tally");
            "#,
        ),
        // Wake 2: arrives as prose with a fenced block rather than a tool call,
        // exercising the fallback for models with weak function-calling. It
        // must still see the global from the previous wake.
        Turn::Text(
            "Let me continue.\n\n```js\nglobalThis.tally += 1;\nif (tally !== 2) throw new Error(\"state was lost: \" + tally);\ndone(\"tally reached \" + tally);\n```\n",
        ),
    ])
    .await;

    let cwd = std::env::current_dir().unwrap();
    let session = Session::create("persist across wakes", &cwd, "mock-model").unwrap();
    let id = session.meta.id.clone();
    let log_path = session.log.path().to_path_buf();

    let client = LlmClient::new(config_for(base_url)).unwrap();
    let mut agent = Agent::new(session, client, Limits::default(), browser(PORT)).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(90), agent.run()).await;
    assert!(result.is_ok(), "the wake loop hung");
    result.unwrap().unwrap();

    let reloaded = Session::load(&id).unwrap();
    assert_eq!(reloaded.meta.status, Status::Done);
    assert_eq!(reloaded.meta.wakes, 2, "should have taken two wakes");

    let events = event::read_all(&log_path).unwrap();
    // If the isolate had been rebuilt between wakes, the second block would
    // have thrown instead of finishing.
    let finished = events.iter().any(|e| {
        matches!(&e.kind, EventKind::Done { summary } if summary.contains("tally reached 2"))
    });
    assert!(finished, "state did not survive the wake boundary");
}
