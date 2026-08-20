//! End-to-end tests for the wake loop, driven by a scripted OpenAI-compatible
//! server. No credentials and no network required — the point is to prove the
//! whole path works: SSE parsing, tool-call assembly, execution in the isolate,
//! the scheduling decision, and the durable event log.

mod common;

use ax::agent::{Agent, Limits};
use ax::chrome::BrowserManager;
use ax::relay::Relay;
use ax::event::{self, EventKind};
use ax::llm::{LlmClient, LlmConfig};
use ax::session::{Session, Status};
use common::{Turn, scripted_server};
use std::time::Duration;

/// A browser manager the tests never connect through. Each gets its own relay
/// port so the suite can run in parallel.
fn browser(port: u16) -> std::sync::Arc<BrowserManager> {
    let home = std::env::temp_dir().join("ax-tests");
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

/// Point AX_HOME at a scratch directory so tests never touch ~/.ax.
///
/// Set exactly once for the whole test binary: `set_var` is process-global, so
/// tests running concurrently must not each point it somewhere different.
/// Sessions get distinct ids, so sharing one home is safe.
fn test_home() {
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
    // is what `ax attach` replays.
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

/// The recorded wake time must be the one that is actually honoured. If the
/// minimum-interval floor were applied after logging, the event log and
/// `schedule.json` would both claim a time that never happens — and a resumed
/// session would trust it.
#[tokio::test(flavor = "current_thread")]
async fn the_logged_schedule_reflects_the_enforced_floor() {
    const PORT: u16 = 18413;
    test_home();

    let base_url = scripted_server(vec![
        // Ask to wake far sooner than the floor allows.
        Turn::ToolCall(r#"wake_in(50, "much too soon");"#),
        Turn::ToolCall(r#"done("finished");"#),
    ])
    .await;

    let cwd = std::env::current_dir().unwrap();
    let session = Session::create("respect the wake floor", &cwd, "mock-model").unwrap();
    let id = session.meta.id.clone();
    let log_path = session.log.path().to_path_buf();

    let client = LlmClient::new(config_for(base_url)).unwrap();
    let limits = Limits {
        min_wake_interval: Duration::from_secs(2),
        ..Default::default()
    };
    let mut agent = Agent::new(session, client, limits, browser(PORT)).unwrap();

    let before = chrono::Utc::now();
    tokio::time::timeout(Duration::from_secs(60), agent.run())
        .await
        .expect("the wake loop hung")
        .unwrap();

    let scheduled = event::read_all(&log_path)
        .unwrap()
        .into_iter()
        .find_map(|logged| match logged.kind {
            EventKind::Scheduled { at: Some(at), .. } => Some(at),
            _ => None,
        })
        .expect("no scheduled event was recorded");

    let delay = (scheduled - before).num_milliseconds();
    assert!(
        delay >= 2000,
        "logged a wake {delay}ms out, sooner than the 2s floor that is enforced"
    );

    // And the session really did wait that long rather than spinning.
    let session = Session::load(&id).unwrap();
    assert_eq!(session.meta.wakes, 2);
}
