//! Transport-neutral browser operations.
//!
//! Listing tabs, attaching to one, and opening a new one are the three things
//! that genuinely differ between driving the user's browser through the
//! extension and driving one we launched ourselves. Normalizing them here means
//! `prelude.js` — and the model — see one interface either way: a target has an
//! id, attaching yields a session, and every other CDP command carries that
//! session unchanged.

use crate::cdp::CdpClient;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::Duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Open pages, normalized to `{id, url, title}`.
pub async fn targets(client: &CdpClient) -> Result<Value> {
    match client.kind {
        "extension" => {
            let result = client
                .send("ax.tabs", json!({}), None, DEFAULT_TIMEOUT)
                .await?;
            Ok(result.get("tabs").cloned().unwrap_or(json!([])))
        }
        _ => {
            let result = client
                .send("Target.getTargets", json!({}), None, DEFAULT_TIMEOUT)
                .await?;
            let listed = result
                .get("targetInfos")
                .and_then(|v| v.as_array())
                .map(|infos| {
                    infos
                        .iter()
                        .filter(|info| info.get("type").and_then(|t| t.as_str()) == Some("page"))
                        .map(|info| {
                            json!({
                                "id": info.get("targetId").cloned().unwrap_or(Value::Null),
                                "url": info.get("url").cloned().unwrap_or(Value::Null),
                                "title": info.get("title").cloned().unwrap_or(Value::Null),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok(json!(listed))
        }
    }
}

/// Attach to a page and return the session id used for subsequent commands.
pub async fn attach(client: &CdpClient, target_id: &str) -> Result<String> {
    match client.kind {
        "extension" => {
            // The extension attaches its debugger to the tab and uses the tab
            // id as the session id.
            let result = client
                .send(
                    "ax.attach",
                    json!({ "tabId": target_id }),
                    None,
                    DEFAULT_TIMEOUT,
                )
                .await?;
            match result.get("sessionId").and_then(|v| v.as_str()) {
                Some(session) => Ok(session.to_string()),
                None => bail!("the extension did not return a session for tab {target_id}"),
            }
        }
        _ => {
            let result = client
                .send(
                    "Target.attachToTarget",
                    // Flat mode keeps every session on one connection, which is
                    // what makes a single `send(..., session)` interface work.
                    json!({ "targetId": target_id, "flatten": true }),
                    None,
                    DEFAULT_TIMEOUT,
                )
                .await?;
            match result.get("sessionId").and_then(|v| v.as_str()) {
                Some(session) => Ok(session.to_string()),
                None => bail!("could not attach to target {target_id}"),
            }
        }
    }
}

/// Open a new page and attach to it.
pub async fn new_tab(client: &CdpClient, url: &str) -> Result<Value> {
    let url = if url.is_empty() { "about:blank" } else { url };
    let target_id = match client.kind {
        "extension" => {
            let result = client
                .send("ax.newTab", json!({ "url": url }), None, DEFAULT_TIMEOUT)
                .await?;
            result
                .get("tabId")
                .map(|v| match v {
                    Value::Number(n) => n.to_string(),
                    other => other.as_str().unwrap_or_default().to_string(),
                })
                .unwrap_or_default()
        }
        _ => {
            let result = client
                .send(
                    "Target.createTarget",
                    json!({ "url": url }),
                    None,
                    DEFAULT_TIMEOUT,
                )
                .await?;
            result
                .get("targetId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        }
    };

    if target_id.is_empty() {
        bail!("the browser did not report a new tab");
    }
    let session = attach(client, &target_id).await?;
    Ok(json!({ "id": target_id, "session": session }))
}

/// Bring a tab to the front (extension transport only; harmless elsewhere).
pub async fn activate(client: &CdpClient, target_id: &str) -> Result<Value> {
    match client.kind {
        "extension" => {
            client
                .send(
                    "ax.activate",
                    json!({ "tabId": target_id }),
                    None,
                    DEFAULT_TIMEOUT,
                )
                .await
        }
        _ => {
            client
                .send(
                    "Target.activateTarget",
                    json!({ "targetId": target_id }),
                    None,
                    DEFAULT_TIMEOUT,
                )
                .await
        }
    }
}
