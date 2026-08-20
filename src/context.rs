//! Keeping a wake inside the model's context window.
//!
//! Wake-to-wake, cost is already flat: every wake builds a fresh context from
//! the system prompt, the ledger, and the state manifest. What still grows is a
//! single long wake — forty turns of tool output will outrun any window.
//!
//! Two mechanisms, in order of preference:
//!
//! 1. **Land the wake.** Past a soft threshold the agent is told to write its
//!    ledger and schedule the next wake. This is the architecture's own answer:
//!    a fresh context with a good ledger beats a compressed one.
//! 2. **Compact.** If it keeps going anyway, older tool output is shrunk and
//!    then whole turns are dropped. This is a backstop that guarantees the loop
//!    cannot die, not the primary plan.
//!
//! The invariant that matters: an assistant message carrying `tool_calls` and
//! the `tool` messages answering it are one indivisible unit. Splitting them
//! produces a request every OpenAI-compatible provider rejects.

use crate::llm::Message;

/// Rough token count. Deliberately provider-agnostic: real tokenizers differ
/// per model, and being approximately right everywhere beats being exactly
/// right for one provider.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message).sum()
}

fn estimate_message(message: &Message) -> usize {
    // Per-message framing overhead, in the spirit of OpenAI's own guidance.
    let mut chars = 8 + message.role.len();
    if let Some(content) = &message.content {
        chars += match content {
            serde_json::Value::String(text) => text.len(),
            other => other.to_string().len(),
        };
    }
    for call in &message.tool_calls {
        chars += call.function.name.len() + call.function.arguments.len() + 8;
    }
    // ~4 characters per token is the usual English approximation.
    chars.div_ceil(4)
}

/// What a compaction did, for the event log and for the model to be told about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    pub turns_dropped: usize,
    pub results_shrunk: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
}

impl Compaction {
    pub fn describe(&self) -> String {
        format!(
            "compacted {} → {} tokens ({} earlier turn{} dropped, {} output{} shrunk)",
            self.tokens_before,
            self.tokens_after,
            self.turns_dropped,
            if self.turns_dropped == 1 { "" } else { "s" },
            self.results_shrunk,
            if self.results_shrunk == 1 { "" } else { "s" },
        )
    }
}

/// How much of a tool result to keep when shrinking it.
const SHRUNK_HEAD: usize = 400;
const SHRUNK_TAIL: usize = 200;

/// Messages at the front that are never touched: the system prompt and the
/// wake message carrying the ledger and manifest.
const PREAMBLE: usize = 2;

/// Turns at the end kept verbatim, so recent work stays exact.
const KEEP_RECENT_TURNS: usize = 3;

/// Bring `messages` under `budget` tokens, in place.
///
/// Returns `None` if nothing needed doing.
pub fn compact(messages: &mut Vec<Message>, budget: usize) -> Option<Compaction> {
    let tokens_before = estimate_tokens(messages);
    if tokens_before <= budget {
        return None;
    }

    let preamble = PREAMBLE.min(messages.len());
    let mut turns = into_turns(messages.split_off(preamble));
    let mut report = Compaction {
        turns_dropped: 0,
        results_shrunk: 0,
        tokens_before,
        tokens_after: tokens_before,
    };

    // Pass 1: shrink tool output in all but the most recent turns. Tool results
    // are nearly always what filled the window.
    let shrinkable = turns.len().saturating_sub(KEEP_RECENT_TURNS);
    for turn in turns.iter_mut().take(shrinkable) {
        report.results_shrunk += turn.shrink_results();
    }

    // Pass 2: still over — drop whole turns from the oldest end, keeping units
    // intact so tool calls never lose their answers.
    while over_budget(messages, &turns, budget) && turns.len() > KEEP_RECENT_TURNS {
        turns.remove(0);
        report.turns_dropped += 1;
    }

    // Pass 3: the recent turns alone exceed the budget. Shrink those too rather
    // than dropping them; losing the newest work is worse than losing detail.
    if over_budget(messages, &turns, budget) {
        for turn in turns.iter_mut() {
            report.results_shrunk += turn.shrink_results();
        }
    }

    if report.turns_dropped > 0 {
        messages.push(Message::user(format!(
            "[{} earlier turn{} in this wake were dropped to stay inside the context window. \
Your ledger and the state manifest above are the record of what happened; re-read files or \
re-run commands if you need detail you no longer see.]",
            report.turns_dropped,
            if report.turns_dropped == 1 { "" } else { "s" },
        )));
    }
    for turn in turns {
        messages.extend(turn.messages);
    }

    report.tokens_after = estimate_tokens(messages);
    Some(report)
}

fn over_budget(head: &[Message], turns: &[Turn], budget: usize) -> bool {
    let used: usize = estimate_tokens(head)
        + turns
            .iter()
            .map(|turn| estimate_tokens(&turn.messages))
            .sum::<usize>();
    used > budget
}

/// One assistant message plus the tool results answering it. Kept together so
/// compaction can never orphan a tool call.
struct Turn {
    messages: Vec<Message>,
}

impl Turn {
    /// Replace long tool output with its head and tail. Returns how many were
    /// changed, ignoring those already short enough to be worth keeping whole.
    fn shrink_results(&mut self) -> usize {
        let mut changed = 0;
        for message in &mut self.messages {
            if message.role != "tool" {
                continue;
            }
            let Some(serde_json::Value::String(text)) = &message.content else {
                continue;
            };
            if text.chars().count() <= SHRUNK_HEAD + SHRUNK_TAIL {
                continue;
            }
            let chars: Vec<char> = text.chars().collect();
            let head: String = chars[..SHRUNK_HEAD].iter().collect();
            let tail: String = chars[chars.len() - SHRUNK_TAIL..].iter().collect();
            let omitted = chars.len() - SHRUNK_HEAD - SHRUNK_TAIL;
            message.content = Some(serde_json::Value::String(format!(
                "{head}\n… [{omitted} characters of this earlier output were dropped to save \
context; re-run the command if you need them] …\n{tail}"
            )));
            changed += 1;
        }
        changed
    }
}

/// Split a flat message list into indivisible turns.
fn into_turns(messages: Vec<Message>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for message in messages {
        // A `tool` reply belongs to the turn already open; anything else starts
        // a new one.
        let continues = message.role == "tool";
        match (continues, turns.last_mut()) {
            (true, Some(open)) => open.messages.push(message),
            _ => turns.push(Turn {
                messages: vec![message],
            }),
        }
    }
    turns
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;

    fn assistant_call(id: &str) -> Message {
        Message::assistant(
            Some("thinking".into()),
            vec![ToolCall::new(id, "run_js", "{\"code\":\"1\"}".into())],
        )
    }

    fn tool_reply(id: &str, size: usize) -> Message {
        Message::tool_result(id, "x".repeat(size))
    }

    fn conversation(turns: usize, result_size: usize) -> Vec<Message> {
        let mut messages = vec![Message::system("system"), Message::user("wake")];
        for index in 0..turns {
            let id = format!("call-{index}");
            messages.push(assistant_call(&id));
            messages.push(tool_reply(&id, result_size));
        }
        messages
    }

    /// Every tool result must still answer a tool call that is present.
    fn assert_pairing_intact(messages: &[Message]) {
        let mut open: Vec<String> = Vec::new();
        for message in messages {
            if !message.tool_calls.is_empty() {
                open = message.tool_calls.iter().map(|c| c.id.clone()).collect();
                continue;
            }
            if message.role == "tool" {
                let id = message.tool_call_id.clone().unwrap_or_default();
                assert!(
                    open.contains(&id),
                    "tool result {id} has no preceding tool call"
                );
            }
        }
        // …and no tool call may be left without an answer.
        for (index, message) in messages.iter().enumerate() {
            for call in &message.tool_calls {
                let answered = messages[index + 1..].iter().any(|later| {
                    later.role == "tool" && later.tool_call_id.as_deref() == Some(&call.id)
                });
                assert!(answered, "tool call {} was left unanswered", call.id);
            }
        }
    }

    #[test]
    fn leaves_small_conversations_alone() {
        let mut messages = conversation(3, 100);
        let before = messages.clone();
        assert!(compact(&mut messages, 100_000).is_none());
        assert_eq!(messages.len(), before.len());
    }

    #[test]
    fn shrinks_old_output_before_dropping_anything() {
        // Sized so that shrinking the five oldest turns is enough on its own:
        // the three kept verbatim must still fit inside the budget.
        let mut messages = conversation(8, 20_000);
        let report = compact(&mut messages, 20_000).expect("should compact");
        assert!(report.results_shrunk > 0, "{report:?}");
        assert_eq!(
            report.turns_dropped, 0,
            "shrinking should be tried before anything is thrown away: {report:?}"
        );
        assert!(estimate_tokens(&messages) <= 20_000);
        assert_pairing_intact(&messages);
    }

    #[test]
    fn drops_whole_turns_when_shrinking_is_not_enough() {
        let mut messages = conversation(60, 8_000);
        let report = compact(&mut messages, 4_000).expect("should compact");
        assert!(report.turns_dropped > 0, "{report:?}");
        assert!(
            estimate_tokens(&messages) <= 4_000,
            "still {} tokens",
            estimate_tokens(&messages)
        );
        assert_pairing_intact(&messages);
    }

    #[test]
    fn never_touches_the_system_prompt_or_the_wake_message() {
        let mut messages = conversation(60, 8_000);
        compact(&mut messages, 2_000);
        assert_eq!(messages[0].role, "system");
        assert_eq!(
            messages[0].content,
            Some(serde_json::Value::String("system".into()))
        );
        assert_eq!(messages[1].role, "user");
        assert_eq!(
            messages[1].content,
            Some(serde_json::Value::String("wake".into()))
        );
    }

    #[test]
    fn keeps_the_most_recent_turn_verbatim_when_possible() {
        let mut messages = conversation(20, 6_000);
        compact(&mut messages, 8_000);
        // The final tool result is the newest work and should not be elided.
        let last = messages.last().unwrap();
        assert_eq!(last.role, "tool");
        let text = match &last.content {
            Some(serde_json::Value::String(text)) => text.clone(),
            other => panic!("unexpected content: {other:?}"),
        };
        assert!(
            !text.contains("dropped to save context"),
            "newest output was shrunk"
        );
    }

    #[test]
    fn tells_the_model_when_turns_disappeared() {
        let mut messages = conversation(60, 8_000);
        compact(&mut messages, 3_000);
        let noted = messages
            .iter()
            .any(|m| matches!(&m.content, Some(serde_json::Value::String(t)) if t.contains("were dropped to stay inside")));
        assert!(noted, "the model must be told its history was truncated");
    }

    #[test]
    fn survives_a_single_oversized_turn() {
        // One turn far larger than the whole budget: it must still come back
        // under, and must not be split apart.
        let mut messages = conversation(1, 400_000);
        compact(&mut messages, 1_000);
        assert!(
            estimate_tokens(&messages) <= 1_000,
            "{}",
            estimate_tokens(&messages)
        );
        assert_pairing_intact(&messages);
    }

    #[test]
    fn estimates_grow_with_content() {
        let small = vec![Message::user("hi")];
        let large = vec![Message::user("x".repeat(4_000))];
        assert!(estimate_tokens(&large) > estimate_tokens(&small) * 10);
    }
}
