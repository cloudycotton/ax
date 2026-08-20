//! An autonomous, goal-oriented agent that acts by writing and running code.
//!
//! The pieces:
//! - [`isolate`] — a persistent QuickJS context, the agent's only way to act
//! - [`host`] — the small set of capabilities exposed into that context
//! - [`llm`] — an OpenAI chat-completions client, so any provider can be used
//! - [`agent`] — the wake loop that ties them together over long horizons
//! - [`event`] / [`session`] — the durable record that makes a session
//!   attachable and resumable

pub mod agent;
pub mod browser;
pub mod cdp;
pub mod chrome;
pub mod config;
pub mod daemon;
pub mod event;
pub mod host;
pub mod ipc;
pub mod isolate;
pub mod launchd;
pub mod llm;
pub mod paths;
pub mod profile;
pub mod prompt;
pub mod relay;
pub mod session;
pub mod setup;
pub mod ui;
pub mod update;
