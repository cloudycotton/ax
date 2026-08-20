//! The wake loop.
//!
//! A wake is one activation of the agent: something (a timer it set, a process
//! it was watching, a person) causes the harness to build one context and run
//! the model until it schedules its next wake. Between wakes the process sleeps
//! and costs nothing.
//!
//! The important structural decision here: **each wake starts from a fresh
//! context** — system prompt, plus a wake message carrying the ledger, the live
//! isolate manifest, and why it woke. Nothing else carries over. That keeps cost
//! per wake bounded no matter how long a session runs (a thousand wakes cost the
//! same as the first), and it forces the ledger to stay good enough to work
//! from, which is exactly the discipline a week-long goal needs. The complete
//! history is never lost — it is all in the event log for a human to read.

use crate::chrome::BrowserManager;
use crate::event::{EventKind, WakeReason};
use crate::host::{HostContext, WakeDecision};
use crate::isolate::Isolate;
use crate::llm::{self, LlmClient, Message};
use crate::prompt;
use crate::session::{Schedule, Session, Status};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// How long one block of model-written JavaScript may run.
const EXEC_TIMEOUT: Duration = Duration::from_secs(600);

/// Model turns allowed inside a single wake before the harness ends it. Stops a
/// confused model from burning a budget in one sitting.
const MAX_TURNS_PER_WAKE: usize = 40;

/// If the model ends a turn without calling a tool and without scheduling, it
/// gets this many nudges before the harness schedules for it.
const MAX_NUDGES: usize = 2;

pub struct Limits {
    /// Stop the session after this many wakes (`None` = run indefinitely).
    pub max_wakes: Option<u64>,
    /// Stop the session after this many total completion tokens.
    pub max_tokens: Option<u64>,
    /// Never wake more often than this, whatever the model asks for.
    pub min_wake_interval: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_wakes: None,
            max_tokens: None,
            // A self-waking agent in a tight loop is the classic way to burn a
            // budget overnight; the floor is enforced here, not in the prompt.
            min_wake_interval: Duration::from_secs(10),
        }
    }
}

pub struct Agent {
    session: Session,
    llm: LlmClient,
    isolate: Isolate,
    host: Arc<HostContext>,
    exits: mpsc::UnboundedReceiver<(String, Option<i32>)>,
    /// Messages sent into the session by a person, via `ax say`.
    inbox: mpsc::UnboundedReceiver<String>,
    /// Kept so the channel stays open even when nobody holds a sender.
    inbox_tx: mpsc::UnboundedSender<String>,
    limits: Limits,
    /// Things that happened while the agent was asleep, reported on next wake.
    pending: Vec<String>,
    tokens_used: u64,
}

impl Agent {
    pub fn new(
        session: Session,
        llm: LlmClient,
        limits: Limits,
        browser: Arc<BrowserManager>,
    ) -> Result<Self> {
        let (exit_tx, exits) = mpsc::unbounded_channel();
        let host = Arc::new(HostContext::new(
            session.meta.cwd.clone(),
            session.dir.clone(),
            session.log.clone(),
            exit_tx,
            browser,
        )?);
        let isolate = Isolate::start(host.clone())?;
        let (inbox_tx, inbox) = mpsc::unbounded_channel();
        Ok(Self {
            session,
            llm,
            isolate,
            host,
            exits,
            inbox,
            inbox_tx,
            limits,
            pending: Vec::new(),
            tokens_used: 0,
        })
    }

    /// A handle for delivering messages from a person into this session.
    /// Anything sent here wakes the agent as soon as it is next idle.
    pub fn inbox(&self) -> mpsc::UnboundedSender<String> {
        self.inbox_tx.clone()
    }

    /// Run until the goal is done, a limit is hit, or the process is stopped.
    pub async fn run(&mut self) -> Result<()> {
        // A session with wakes behind it is being resumed after a restart:
        // honour whatever it had scheduled instead of barging in immediately.
        let mut reason = if self.session.meta.wakes == 0 {
            WakeReason::Initial
        } else {
            self.resume().await
        };
        loop {
            if let Some(max) = self.limits.max_wakes {
                if self.session.meta.wakes >= max {
                    self.session.log.append(EventKind::Error {
                        message: format!("stopping: reached the {max}-wake limit"),
                    })?;
                    self.session.set_status(Status::Failed)?;
                    return Ok(());
                }
            }
            if let Some(max) = self.limits.max_tokens {
                if self.tokens_used >= max {
                    self.session.log.append(EventKind::Error {
                        message: format!("stopping: reached the {max}-token budget"),
                    })?;
                    self.session.set_status(Status::Failed)?;
                    return Ok(());
                }
            }

            let decision = self.wake(reason).await?;
            match decision {
                WakeDecision::Done { summary } => {
                    self.session.log.append(EventKind::Done { summary })?;
                    self.session.clear_schedule();
                    self.session.set_status(Status::Done)?;
                    return Ok(());
                }
                WakeDecision::At { at, note } => {
                    // Apply the floor first: the log and the schedule must say
                    // when the agent will really wake, not what it asked for.
                    let at = self.not_before(at);
                    self.session.log.append(EventKind::Scheduled {
                        at: Some(at),
                        on: None,
                        note: note.clone(),
                    })?;
                    self.session.save_schedule(&Schedule {
                        at: Some(at),
                        on_exit: None,
                        note: note.clone(),
                    });
                    self.session.set_status(Status::Sleeping)?;
                    reason = self.sleep_until(at, note).await;
                }
                WakeDecision::OnExit { name, note } => {
                    self.session.log.append(EventKind::Scheduled {
                        at: None,
                        on: Some(format!("exit of `{name}`")),
                        note: note.clone(),
                    })?;
                    self.session.save_schedule(&Schedule {
                        at: None,
                        on_exit: Some(name.clone()),
                        note: note.clone(),
                    });
                    self.session.set_status(Status::Sleeping)?;
                    reason = self.wait_for_exit(&name).await;
                }
            }
        }
    }

    /// Work out how a restarted session should re-enter its loop.
    ///
    /// The isolate does not survive a restart — that is why the ledger and the
    /// files on disk are the source of truth — but the *schedule* does, so a
    /// session told to check back in six hours does not lose those six hours
    /// just because the machine rebooted.
    async fn resume(&mut self) -> WakeReason {
        let Some(schedule) = self.session.load_schedule() else {
            return WakeReason::Restart;
        };
        match schedule.at {
            // A process it was waiting on cannot have survived the restart.
            None => WakeReason::Restart,
            Some(at) if at <= Utc::now() => WakeReason::Restart,
            Some(at) => {
                let _ = self.session.log.append(EventKind::Scheduled {
                    at: Some(at),
                    on: None,
                    note: format!("resumed after restart — {}", schedule.note),
                });
                let _ = self.session.set_status(Status::Sleeping);
                self.sleep_until(at, schedule.note).await
            }
        }
    }

    /// One activation: build a fresh context, run turns until the model
    /// schedules its next wake.
    async fn wake(&mut self, reason: WakeReason) -> Result<WakeDecision> {
        self.session.meta.wakes += 1;
        let wake = self.session.meta.wakes;
        self.session.set_status(Status::Running)?;
        self.session
            .log
            .append(EventKind::WakeStarted { wake, reason: reason.clone() })?;

        let manifest = self.isolate.manifest().await.unwrap_or_default();
        let pending = std::mem::take(&mut self.pending);
        let mut messages = vec![
            Message::system(prompt::system_prompt(
                &self.session.meta.goal,
                &self.session.dir,
                &self.session.meta.cwd,
            )),
            Message::user(prompt::wake_message(
                wake,
                &reason,
                &self.session.ledger(),
                &manifest,
                &pending,
            )),
        ];

        let mut nudges = 0usize;
        for turn in 0..MAX_TURNS_PER_WAKE {
            let completion = self.turn(&mut messages).await?;

            // Executing code may have set a decision; that ends the wake.
            if let Some(decision) = self.host.take_decision() {
                self.session.log.append(EventKind::WakeEnded {
                    wake,
                    outcome: describe(&decision),
                })?;
                return Ok(decision);
            }

            if completion.tool_calls.is_empty() {
                // The model stopped talking without scheduling anything. Ask it
                // to close the wake properly; give up after a couple of tries.
                nudges += 1;
                if nudges > MAX_NUDGES {
                    break;
                }
                messages.push(Message::user(
                    "You ended a turn without scheduling your next wake. Call wake_in, wake_at, \
on_exit, or done now — via run_js, as a normal function call."
                        .to_string(),
                ));
                continue;
            }

            if turn + 1 == MAX_TURNS_PER_WAKE {
                messages.push(Message::user(
                    "This wake has used its turn budget. Update the ledger and schedule your next \
wake now.".to_string(),
                ));
            }
        }

        // Nothing scheduled: rather than stopping the session, let it breathe
        // and come back. A stalled wake is a bad turn, not a dead goal.
        let fallback = WakeDecision::At {
            at: Utc::now() + chrono::Duration::minutes(5),
            note: "harness default: the previous wake ended without scheduling".to_string(),
        };
        self.session.log.append(EventKind::WakeEnded {
            wake,
            outcome: "no schedule set; defaulting to 5 minutes".to_string(),
        })?;
        Ok(fallback)
    }

    /// One model turn: stream a completion, then run whatever code it asked for.
    async fn turn(&mut self, messages: &mut Vec<Message>) -> Result<llm::Completion> {
        let mut streamed = false;
        let completion = self
            .llm
            .complete(messages, |delta| {
                // Live prose for anyone watching the terminal. The complete text
                // is written to the event log below, which is what `ax log`
                // and `ax attach` replay.
                print!("{delta}");
                let _ = std::io::stdout().flush();
                streamed = true;
            })
            .await?;
        if streamed {
            println!();
        }

        if !completion.text.trim().is_empty() {
            self.session.log.append(EventKind::ModelText {
                text: completion.text.clone(),
            })?;
        }
        if completion.usage.completion_tokens > 0 || completion.usage.prompt_tokens > 0 {
            self.tokens_used += completion.usage.completion_tokens;
            self.session.log.append(EventKind::Usage {
                prompt_tokens: completion.usage.prompt_tokens,
                completion_tokens: completion.usage.completion_tokens,
            })?;
        }

        messages.push(completion.as_message());

        for call in &completion.tool_calls {
            let code = match llm::parse_code_argument(&call.function.arguments) {
                Ok(code) => code,
                Err(err) => {
                    messages.push(Message::tool_result(
                        &call.id,
                        format!("could not read your tool arguments: {err}"),
                    ));
                    continue;
                }
            };

            self.session.log.append(EventKind::ToolCall {
                call_id: call.id.clone(),
                code: code.clone(),
            })?;

            let outcome = self.isolate.exec(&code, EXEC_TIMEOUT).await?;
            self.session.log.append(EventKind::ToolResult {
                call_id: call.id.clone(),
                ok: outcome.ok,
                value: outcome.value.clone(),
                truncated: outcome.truncated,
                duration_ms: outcome.duration_ms,
            })?;

            let body = if outcome.value.trim().is_empty() {
                if outcome.ok {
                    "(no value returned)".to_string()
                } else {
                    "(failed with no message)".to_string()
                }
            } else {
                outcome.value.clone()
            };
            let body = if outcome.ok {
                body
            } else {
                format!("ERROR\n{body}")
            };
            messages.push(Message::tool_result(&call.id, body));

            // A decision made mid-turn ends the wake; stop running further calls.
            if self.host.decision_pending() {
                break;
            }
        }

        Ok(completion)
    }

    /// Sleep until `at`, waking early if a background process exits.
    /// Never sooner than the minimum wake interval. A confused model asking to
    /// wake every second is the classic way to burn a budget overnight, so the
    /// floor is enforced here rather than trusted to the prompt.
    fn not_before(&self, at: DateTime<Utc>) -> DateTime<Utc> {
        let floor = Utc::now()
            + chrono::Duration::from_std(self.limits.min_wake_interval)
                .unwrap_or_else(|_| chrono::Duration::seconds(10));
        at.max(floor)
    }

    async fn sleep_until(&mut self, at: DateTime<Utc>, note: String) -> WakeReason {
        let at = self.not_before(at);

        loop {
            let remaining = (at - Utc::now()).to_std().unwrap_or(Duration::ZERO);
            tokio::select! {
                _ = tokio::time::sleep(remaining) => {
                    return WakeReason::Timer { note };
                }
                Some((name, code)) = self.exits.recv() => {
                    // A process dying is usually more interesting than the
                    // timer that was pending, so wake now and say why.
                    return WakeReason::ProcessExit { name, code };
                }
                Some(text) = self.inbox.recv() => {
                    // A person always outranks a timer.
                    let _ = self.session.log.append(EventKind::UserMessage {
                        text: text.clone(),
                    });
                    return WakeReason::User { text };
                }
            }
        }
    }

    /// Wait for a specific process to exit. Other exits are recorded and
    /// reported at the next wake rather than triggering one.
    async fn wait_for_exit(&mut self, name: &str) -> WakeReason {
        loop {
            tokio::select! {
                exit = self.exits.recv() => match exit {
                    Some((exited, code)) if exited == name => {
                        return WakeReason::ProcessExit { name: exited, code };
                    }
                    Some((exited, code)) => {
                        self.pending.push(format!(
                            "process `{exited}` exited with {}",
                            code.map(|c| c.to_string()).unwrap_or_else(|| "a signal".into())
                        ));
                    }
                    None => {
                        // No senders left; nothing can wake us on this path.
                        return WakeReason::Timer {
                            note: format!("no process named `{name}` can report an exit"),
                        };
                    }
                },
                Some(text) = self.inbox.recv() => {
                    let _ = self.session.log.append(EventKind::UserMessage {
                        text: text.clone(),
                    });
                    return WakeReason::User { text };
                }
            }
        }
    }
}

fn describe(decision: &WakeDecision) -> String {
    match decision {
        WakeDecision::At { at, note } => format!("sleeping until {at} — {note}"),
        WakeDecision::OnExit { name, note } => format!("waiting for `{name}` to exit — {note}"),
        WakeDecision::Done { summary } => format!("done — {summary}"),
    }
}
