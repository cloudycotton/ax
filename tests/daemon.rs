//! End-to-end test for the supervisor.
//!
//! The point of the daemon is that a goal outlives the terminal that started
//! it, and that attaching later shows the whole story. This drives it through
//! the real unix socket with a scripted model behind it.

mod common;

use ax::daemon::{self, Daemon};
use ax::ipc::{Request, Response};
use ax::session::{Session, Status};
use common::{Turn, scripted_server};
use std::time::Duration;

/// The daemon reads its configuration from the environment, so this test binary
/// sets it up once before anything starts.
fn configure(base_url: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join("ax-daemon-test");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // Safety: this is the only writer, and it runs before the daemon starts.
    unsafe {
        std::env::set_var("AX_HOME", &home);
        std::env::set_var("AX_API_KEY", "test-key");
        std::env::set_var("AX_BASE_URL", base_url);
        std::env::set_var("AX_MODEL", "mock-model");
    }
    home
}

#[tokio::test]
async fn supervises_a_session_and_replays_it_on_attach() {
    let base_url = scripted_server(vec![
        Turn::ToolCall(
            r#"
            console.log("working on it");
            globalThis.answer = 42;
            wake_in(1000, "check back");
            "#,
        ),
        Turn::ToolCall(r#"done("answer was " + globalThis.answer);"#),
    ])
    .await;
    configure(&base_url);

    tokio::spawn(async {
        // Errors here surface as a failure to connect below.
        let _ = Daemon::start().await;
    });

    // Wait for the socket.
    let mut up = false;
    for _ in 0..100 {
        if daemon::is_running().await {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(up, "the daemon never came up");

    let cwd = std::env::current_dir().unwrap();
    let reply = daemon::request(&Request::Create {
        goal: "prove the daemon supervises sessions".into(),
        cwd: cwd.to_string_lossy().to_string(),
        model: None,
        max_wakes: None,
        max_tokens: None,
    })
    .await
    .unwrap();

    let id = match reply {
        Response::Created { id } => id,
        other => panic!("expected a session id, got {other:?}"),
    };

    // The session runs inside the daemon, not in this task: wait for it to
    // finish on its own.
    let mut finished = false;
    for _ in 0..300 {
        if let Ok(session) = Session::load(&id)
            && session.meta.status == Status::Done
        {
            finished = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(finished, "the supervised session never completed");

    // Attaching after the fact must still replay the entire history — this is
    // what makes coming back to a long-running session useful.
    let replies = daemon::request_stream(&Request::Attach {
        id: id.clone(),
        from_seq: 0,
    })
    .await
    .unwrap();

    let events: Vec<_> = replies
        .into_iter()
        .filter_map(|reply| match reply {
            Response::Event { event } => Some(*event),
            _ => None,
        })
        .collect();
    assert!(!events.is_empty(), "attach replayed nothing");

    let rendered = format!("{events:?}");
    assert!(
        rendered.contains("working on it"),
        "console output missing from the replay"
    );
    assert!(
        rendered.contains("answer was 42"),
        "the session's result is missing; state did not survive between wakes"
    );

    // Two wakes: the first scheduled, the second finished the goal.
    let session = Session::load(&id).unwrap();
    assert_eq!(session.meta.wakes, 2);
    // A finished session must not leave a pending wake behind for a future
    // daemon to resume.
    assert!(
        session.load_schedule().is_none(),
        "a completed session left its schedule on disk"
    );
}
