//! OpenAI chat-completions client.
//!
//! Chat completions is the wire format because it is stateless and universally
//! spoken — any provider, gateway, or local server can be dropped in by
//! changing `base_url` and `model`. Two deliberate accommodations for the long
//! haul: the transcript is only ever appended to (so provider-side prefix
//! caching keeps working across hundreds of wakes), and models with broken
//! function-calling still work via the fenced-code fallback in
//! [`Completion::from_parts`].

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

/// The single tool the model ever sees.
pub const TOOL_NAME: &str = "run_js";

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Ask for token usage on the final stream chunk. Standard, but not every
    /// OpenAI-compatible server accepts it.
    pub include_usage: bool,
}

impl LlmConfig {
    /// Read configuration from the environment. `AX_BASE_URL` /
    /// `AX_API_KEY` / `AX_MODEL` win, falling back to the usual
    /// `OPENAI_*` names so an existing shell just works.
    pub fn from_env(model_override: Option<String>) -> Result<Self> {
        let base_url = std::env::var("AX_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
            .trim_end_matches('/')
            .to_string();
        let api_key = std::env::var("AX_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .context("set AX_API_KEY (or OPENAI_API_KEY) to your provider's key")?;
        let model = model_override
            .or_else(|| std::env::var("AX_MODEL").ok())
            .unwrap_or_else(|| "gpt-4.1".to_string());
        Ok(Self {
            base_url,
            api_key,
            model,
            max_tokens: None,
            temperature: None,
            include_usage: true,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON string, exactly as the model produced it.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: &str, arguments: String) -> Self {
        Self {
            id: id.into(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            tool_calls,
            ..Default::default()
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_call_id: Some(call_id.into()),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// One assistant turn.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

impl Completion {
    /// Build a completion from streamed parts, applying the fallback for models
    /// that emit code in prose instead of calling the tool properly.
    fn from_parts(text: String, tool_calls: Vec<ToolCall>, usage: Usage) -> Self {
        if !tool_calls.is_empty() {
            return Self {
                text,
                tool_calls,
                usage,
            };
        }
        match extract_fenced_js(&text) {
            Some(code) => {
                let arguments = json!({ "code": code }).to_string();
                Self {
                    text,
                    tool_calls: vec![ToolCall::new("fenced-0", TOOL_NAME, arguments)],
                    usage,
                }
            }
            None => Self {
                text,
                tool_calls,
                usage,
            },
        }
    }

    pub fn as_message(&self) -> Message {
        Message::assistant(
            if self.text.is_empty() {
                None
            } else {
                Some(self.text.clone())
            },
            self.tool_calls.clone(),
        )
    }
}

/// Pull the first ```js / ```javascript fenced block out of prose. This is the
/// escape hatch that lets models with unreliable tool-calling drive the agent.
fn extract_fenced_js(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let (lang, body) = {
            let nl = after.find('\n')?;
            (after[..nl].trim(), &after[nl + 1..])
        };
        let close = body.find("```")?;
        let code = &body[..close];
        if matches!(lang, "js" | "javascript" | "") && !code.trim().is_empty() {
            return Some(code.to_string());
        }
        rest = &body[close + 3..];
    }
    None
}

pub struct LlmClient {
    http: reqwest::Client,
    config: LlmConfig,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Individual wakes can involve long thinking; the retry loop below
            // handles genuine stalls.
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self { http, config })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// The tool schema. One tool, one required argument — deliberately trivial
    /// so that even small models call it correctly.
    fn tools(&self) -> serde_json::Value {
        json!([{
            "type": "function",
            "function": {
                "name": TOOL_NAME,
                "description": "Run JavaScript in your persistent isolate and return the result. \
        Top-level await is supported; `return` a value to see it. Globals you define persist across calls.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "code": {
                            "type": "string",
                            "description": "JavaScript to execute."
                        }
                    },
                    "required": ["code"]
                }
            }
        }])
    }

    /// One cheap, non-streaming request to prove the endpoint, key, and model
    /// all work. Deliberately does not retry: `ax setup` wants a wrong key to
    /// fail immediately, not after a minute of backoff.
    pub async fn probe(&self) -> Result<()> {
        let body = json!({
            "model": self.config.model,
            "messages": [{ "role": "user", "content": "Reply with the single word: ok" }],
            "max_tokens": 8,
        });
        let response = self
            .http
            .post(format!("{}/chat/completions", self.config.base_url))
            .bearer_auth(&self.config.api_key)
            .timeout(Duration::from_secs(30))
            .json(&body)
            .send()
            .await
            .context("could not reach the endpoint")?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let detail = response.text().await.unwrap_or_default();
        // Surface the provider's own message: it is nearly always the clearest
        // explanation of what is wrong (bad key, unknown model, no credit).
        let message = serde_json::from_str::<serde_json::Value>(&detail)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| detail.trim().chars().take(300).collect());
        bail!("{status}: {message}")
    }

    /// Stream one completion, invoking `on_text` with prose deltas as they
    /// arrive. Retries transient failures with backoff — a week-long session
    /// must not die because a provider hiccuped at 3am.
    pub async fn complete(
        &self,
        messages: &[Message],
        mut on_text: impl FnMut(&str),
    ) -> Result<Completion> {
        let mut attempt = 0u32;
        loop {
            match self.try_complete(messages, &mut on_text).await {
                Ok(completion) => return Ok(completion),
                Err(err) => {
                    attempt += 1;
                    if attempt >= 6 {
                        return Err(err.context("model request failed after 6 attempts"));
                    }
                    let backoff = Duration::from_millis(800 * 2u64.pow(attempt.min(5)));
                    eprintln!(
                        "\x1b[33m! model request failed ({err}); retrying in {:?}\x1b[0m",
                        backoff
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    async fn try_complete(
        &self,
        messages: &[Message],
        on_text: &mut impl FnMut(&str),
    ) -> Result<Completion> {
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "tools": self.tools(),
            "tool_choice": "auto",
            "stream": true,
        });
        if self.config.include_usage {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(max) = self.config.max_tokens {
            body["max_tokens"] = json!(max);
        }
        if let Some(temp) = self.config.temperature {
            body["temperature"] = json!(temp);
        }

        let response = self
            .http
            .post(format!("{}/chat/completions", self.config.base_url))
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await
            .context("could not reach the model endpoint")?;

        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            bail!("model returned {status}: {}", detail.trim());
        }

        let mut text = String::new();
        let mut usage = Usage::default();
        // Tool call deltas arrive fragmented and keyed by index.
        let mut partials: Vec<PartialToolCall> = Vec::new();
        let mut buffer = String::new();
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("stream interrupted")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // SSE events are separated by a blank line.
            while let Some(split) = find_event_boundary(&buffer) {
                let (raw_event, rest) = buffer.split_at(split);
                let raw_event = raw_event.to_string();
                buffer = rest.trim_start_matches(['\r', '\n']).to_string();

                for line in raw_event.lines() {
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        return Ok(Completion::from_parts(
                            text,
                            finish_tool_calls(partials),
                            usage,
                        ));
                    }
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                        continue;
                    };
                    apply_chunk(&value, &mut text, &mut partials, &mut usage, on_text);
                }
            }
        }

        Ok(Completion::from_parts(
            text,
            finish_tool_calls(partials),
            usage,
        ))
    }
}

#[derive(Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn find_event_boundary(buffer: &str) -> Option<usize> {
    let lf = buffer.find("\n\n");
    let crlf = buffer.find("\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn apply_chunk(
    value: &serde_json::Value,
    text: &mut String,
    partials: &mut Vec<PartialToolCall>,
    usage: &mut Usage,
    on_text: &mut impl FnMut(&str),
) {
    if let Some(u) = value.get("usage").and_then(|u| u.as_object()) {
        usage.prompt_tokens = u
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(usage.prompt_tokens);
        usage.completion_tokens = u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(usage.completion_tokens);
    }

    let Some(delta) = value
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
    else {
        return;
    };

    if let Some(content) = delta.get("content").and_then(|c| c.as_str())
        && !content.is_empty()
    {
        text.push_str(content);
        on_text(content);
    }

    let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) else {
        return;
    };
    for call in calls {
        let index = call.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
        if partials.len() <= index {
            partials.resize(index + 1, PartialToolCall::default());
        }
        let slot = &mut partials[index];
        if let Some(id) = call.get("id").and_then(|v| v.as_str())
            && !id.is_empty()
        {
            slot.id = id.to_string();
        }
        if let Some(function) = call.get("function") {
            if let Some(name) = function.get("name").and_then(|v| v.as_str())
                && !name.is_empty()
            {
                slot.name = name.to_string();
            }
            if let Some(args) = function.get("arguments").and_then(|v| v.as_str()) {
                slot.arguments.push_str(args);
            }
        }
    }
}

fn finish_tool_calls(partials: Vec<PartialToolCall>) -> Vec<ToolCall> {
    partials
        .into_iter()
        .enumerate()
        .filter(|(_, p)| !p.name.is_empty() || !p.arguments.is_empty())
        .map(|(i, p)| {
            let id = if p.id.is_empty() {
                format!("call-{i}")
            } else {
                p.id
            };
            let name = if p.name.is_empty() {
                TOOL_NAME.to_string()
            } else {
                p.name
            };
            ToolCall::new(id, &name, p.arguments)
        })
        .collect()
}

/// Pull the `code` argument out of a tool call, tolerating the ways models get
/// JSON slightly wrong.
pub fn parse_code_argument(arguments: &str) -> Result<String> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        bail!("tool call had no arguments");
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(code) = value.get("code").and_then(|c| c.as_str()) {
            return Ok(code.to_string());
        }
        // Some models send the code as a bare JSON string.
        if let Some(code) = value.as_str() {
            return Ok(code.to_string());
        }
    }
    // Last resort: assume the model sent raw source with no JSON wrapper.
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_fenced_javascript() {
        let text = "Let me check.\n\n```js\nreturn 1 + 1;\n```\n";
        assert_eq!(extract_fenced_js(text).as_deref(), Some("return 1 + 1;\n"));
    }

    #[test]
    fn ignores_non_js_fences() {
        let text = "```python\nprint(1)\n```";
        assert_eq!(extract_fenced_js(text), None);
    }

    #[test]
    fn parses_code_argument_from_json() {
        let args = r#"{"code":"return 42;"}"#;
        assert_eq!(parse_code_argument(args).unwrap(), "return 42;");
    }

    #[test]
    fn falls_back_to_raw_source() {
        assert_eq!(parse_code_argument("return 42;").unwrap(), "return 42;");
    }
}
