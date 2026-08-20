//! `ax` — an autonomous, goal-oriented agent that acts by writing code.

use anyhow::Result;
use ax::session::Session;
use ax::{agent, chrome, event, host, isolate, llm, paths, relay, ui};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ax",
    version,
    about = "An autonomous agent that pursues a goal by writing and running code"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a new session pursuing a goal, rooted at the current directory.
    Run {
        /// The goal, in plain language.
        #[arg(required = true, num_args = 1..)]
        goal: Vec<String>,
        /// Model id (defaults to $AX_MODEL).
        #[arg(long)]
        model: Option<String>,
        /// Stop after this many wakes.
        #[arg(long)]
        max_wakes: Option<u64>,
        /// Stop after this many completion tokens.
        #[arg(long)]
        max_tokens: Option<u64>,
    },
    /// List sessions.
    Ls,
    /// Replay a session's history, optionally following it live.
    Log {
        id: String,
        /// Keep streaming new events as they happen.
        #[arg(short, long)]
        follow: bool,
    },
    /// Delete a session and everything it recorded.
    Rm { id: String },
    /// Show the pairing token and how to install the browser extension.
    Pair,
    /// Run JavaScript in a throwaway isolate. For debugging the harness.
    #[command(hide = true)]
    Js {
        #[arg(required = true, num_args = 1..)]
        code: Vec<String>,
        /// Seconds before the isolate interrupts the script.
        #[arg(long, default_value = "120")]
        timeout: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run { goal, model, max_wakes, max_tokens } => {
            run(goal.join(" "), model, max_wakes, max_tokens).await
        }
        Command::Ls => list(),
        Command::Log { id, follow } => show_log(&id, follow).await,
        Command::Rm { id } => remove(&id),
        Command::Pair => pair(),
        Command::Js { code, timeout } => scratch_js(&code.join(" "), timeout).await,
    }
}

/// Run one block of JavaScript in a throwaway isolate, streaming console output
/// the same way a real wake does.
async fn scratch_js(code: &str, timeout_secs: u64) -> Result<()> {
    let dir = paths::agent_home()?.join("scratch");
    paths::ensure_dir(&dir)?;
    let log = std::sync::Arc::new(event::EventLog::open(dir.join("events.jsonl"))?);
    let (exits, _exits_rx) = tokio::sync::mpsc::unbounded_channel();
    let host = std::sync::Arc::new(host::HostContext::new(
        std::env::current_dir()?,
        dir,
        log.clone(),
        exits,
        browser_manager().await?,
    )?);

    let mut console = log.subscribe();
    tokio::spawn(async move {
        while let Ok(logged) = console.recv().await {
            if matches!(logged.kind, event::EventKind::Console { .. }) {
                ui::render(&logged);
            }
        }
    });

    let isolate = isolate::Isolate::start(host.clone())?;

    // `%%` on its own line separates blocks, so one invocation can check that
    // state really does survive between calls.
    for (index, block) in code.split("\n%%\n").enumerate() {
        if index > 0 {
            println!("{}", ui::dim(&format!("── block {}", index + 1)));
        }
        let outcome = isolate
            .exec(block, std::time::Duration::from_secs(timeout_secs))
            .await?;
        // Give the console forwarder a moment to drain before the result.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if outcome.ok {
            println!("{} {}", ui::bold("→"), outcome.value);
        } else {
            println!("{} {}", ui::bold("✗"), outcome.value);
        }
        println!("{}", ui::dim(&format!("{}ms", outcome.duration_ms)));
    }

    if let Some(decision) = host.take_decision() {
        println!("{}", ui::dim(&format!("decision: {decision:?}")));
    }
    let manifest = isolate.manifest().await?;
    if !manifest.trim().is_empty() {
        println!("{}\n{}", ui::dim("manifest:"), manifest);
    }
    Ok(())
}

/// Build the browser manager and start listening for the extension.
///
/// A failure to bind the relay port is not fatal: another agent process may
/// already own it, and the managed-Chrome fallback still works.
async fn browser_manager() -> Result<std::sync::Arc<chrome::BrowserManager>> {
    let home = paths::agent_home()?;
    paths::ensure_dir(&home)?;
    let relay = relay::Relay::new(&home, relay::DEFAULT_RELAY_PORT)?;
    if let Err(err) = relay.listen().await {
        eprintln!("{}", ui::dim(&format!("browser relay unavailable: {err}")));
    }
    Ok(std::sync::Arc::new(chrome::BrowserManager::new(home, relay)))
}

async fn run(
    goal: String,
    model: Option<String>,
    max_wakes: Option<u64>,
    max_tokens: Option<u64>,
) -> Result<()> {
    let config = llm::LlmConfig::from_env(model)?;
    let cwd = std::env::current_dir()?;
    let session = Session::create(&goal, &cwd, &config.model)?;
    println!(
        "session {}  {}  {}",
        ui::bold(&session.meta.id),
        ui::dim(&config.model),
        ui::dim(&cwd.display().to_string())
    );
    // Everything the agent does is rendered from the same event log an
    // attaching client would read, so the live view and the replay agree.
    let mut live = session.log.subscribe();
    tokio::spawn(async move {
        while let Ok(logged) = live.recv().await {
            if !matches!(logged.kind, event::EventKind::ModelText { .. }) {
                ui::render(&logged);
            }
        }
    });

    let limits = agent::Limits {
        max_wakes,
        max_tokens,
        ..Default::default()
    };
    let mut agent = agent::Agent::new(
        session,
        llm::LlmClient::new(config)?,
        limits,
        browser_manager().await?,
    )?;
    agent.run().await
}

fn list() -> Result<()> {
    let sessions = Session::list()?;
    if sessions.is_empty() {
        println!("no sessions yet — start one with `ax run \"<goal>\"`");
        return Ok(());
    }
    for meta in sessions {
        println!(
            "{:<20} {:<9} {:<5} {}",
            ui::bold(&meta.id),
            ui::status_label(meta.status),
            format!("w{}", meta.wakes),
            ui::truncate_line(&meta.goal, 60),
        );
    }
    Ok(())
}

async fn show_log(id: &str, follow: bool) -> Result<()> {
    let session = Session::load(id)?;
    let mut receiver = session.log.subscribe();
    for logged in event::read_all(session.log.path())? {
        ui::render(&logged);
    }
    if !follow {
        return Ok(());
    }
    println!("{}", ui::dim("— following; ctrl-c to detach —"));
    while let Ok(logged) = receiver.recv().await {
        ui::render(&logged);
    }
    Ok(())
}

/// Print the pairing token and the one-time setup steps.
fn pair() -> Result<()> {
    let home = paths::agent_home()?;
    paths::ensure_dir(&home)?;
    let relay = relay::Relay::new(&home, relay::DEFAULT_RELAY_PORT)?;
    let extension = std::env::current_dir()?.join("extension");

    println!("{}", ui::bold("Pair the browser extension"));
    println!();
    println!("The agent drives your real browser — the one with your logins — through a");
    println!("small extension. Chrome will not expose the default profile any other way.");
    println!();
    println!("  1. Open {}", ui::bold("chrome://extensions"));
    println!("  2. Turn on {}", ui::bold("Developer mode"));
    println!("  3. {} and choose:", ui::bold("Load unpacked"));
    println!("     {}", extension.display());
    println!("  4. Open the extension and paste this token:");
    println!();
    println!("     {}", ui::bold(relay.token()));
    println!();
    println!(
        "{}",
        ui::dim(&format!(
            "The token is stored at {} and guards a socket that can drive your logged-in\nbrowser. Treat it like a password; anyone who has it can act as you.",
            relay::token_path(&home).display()
        ))
    );
    Ok(())
}

fn remove(id: &str) -> Result<()> {
    let dir = paths::session_dir(id)?;
    if !dir.exists() {
        anyhow::bail!("no such session `{id}`");
    }
    std::fs::remove_dir_all(&dir)?;
    println!("removed session {id}");
    Ok(())
}
