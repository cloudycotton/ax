//! Tests for the extension bridge.
//!
//! This socket can drive a fully logged-in browser, so its access checks are
//! the most security-sensitive code in the project. These tests stand in for
//! the extension and confirm that a wrong token is refused, a non-extension
//! origin is refused, and a correctly paired client can round-trip commands.

use as_agent::relay::Relay;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Message, http::HeaderValue};

fn temp_home(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("as-agent-relay-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Connect the way the extension does, with a controllable Origin.
async fn dial(
    port: u16,
    origin: &str,
) -> Option<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>
{
    let mut request = format!("ws://127.0.0.1:{port}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("origin", HeaderValue::from_str(origin).unwrap());
    tokio_tungstenite::connect_async(request)
        .await
        .ok()
        .map(|(stream, _)| stream)
}

#[tokio::test]
async fn rejects_a_bad_token() {
    let home = temp_home("bad-token");
    let relay = Relay::new(&home, 18401).unwrap();
    relay.listen().await.unwrap();

    let mut socket = dial(18401, "chrome-extension://abcdef").await.unwrap();
    socket
        .send(Message::Text(json!({ "token": "wrong" }).to_string().into()))
        .await
        .unwrap();

    let reply = socket.next().await.unwrap().unwrap();
    let body: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
    assert_eq!(body["ok"], json!(false), "a bad token must be refused");
    assert!(
        !relay.has_extension(),
        "a refused client must not be treated as connected"
    );
}

#[tokio::test]
async fn rejects_a_non_extension_origin() {
    let home = temp_home("bad-origin");
    let relay = Relay::new(&home, 18402).unwrap();
    relay.listen().await.unwrap();
    let token = relay.token().to_string();

    // A web page that somehow learned the port and the token still must not get in.
    let socket = dial(18402, "https://evil.example.com").await;
    if let Some(mut socket) = socket {
        let _ = socket
            .send(Message::Text(json!({ "token": token }).to_string().into()))
            .await;
        // The server drops the connection rather than serving it.
        let _ = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !relay.has_extension(),
        "a page origin must never be accepted as the extension"
    );
}

#[tokio::test]
async fn a_paired_extension_round_trips_commands() {
    let home = temp_home("paired");
    let relay = Relay::new(&home, 18403).unwrap();
    relay.listen().await.unwrap();
    let token = relay.token().to_string();

    let mut socket = dial(18403, "chrome-extension://abcdef").await.unwrap();
    socket
        .send(Message::Text(json!({ "token": token }).to_string().into()))
        .await
        .unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    let body: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
    assert_eq!(body["ok"], json!(true), "the correct token must be accepted");

    // Stand in for the extension: answer agent.tabs the way background.js does.
    tokio::spawn(async move {
        while let Some(Ok(message)) = socket.next().await {
            let Ok(text) = message.into_text() else {
                continue;
            };
            let Ok(command) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let id = command["id"].clone();
            let response = json!({
                "id": id,
                "result": { "tabs": [{ "id": "7", "url": "https://example.com", "title": "Example" }] }
            });
            let _ = socket
                .send(Message::Text(response.to_string().into()))
                .await;
        }
    });

    // Give the relay a moment to register the connection.
    for _ in 0..50 {
        if relay.has_extension() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(relay.has_extension(), "the extension should be connected");

    let client = relay.connect().await.unwrap();
    assert_eq!(client.kind, "extension");

    let tabs = as_agent::browser::targets(&client).await.unwrap();
    assert_eq!(tabs[0]["id"], json!("7"));
    assert_eq!(tabs[0]["url"], json!("https://example.com"));
}
