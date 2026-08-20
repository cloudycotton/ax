//! `ax` — an autonomous, goal-oriented agent that acts by writing code.

use anyhow::{Context, Result};
use ax::session::Session;
use ax::{
    agent, chrome, config, daemon, event, host, ipc, isolate, launchd, llm, paths, relay, setup,
    ui, update,
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ax",
    version,
    about = "An autonomous agent that pursues a goal by writing and running code"
)]
struct Cli {
    /// Omitted on purpose: running `ax` bare walks a new user through setup
    /// rather than printing a usage error at them.
    #[command(subcommand)]
    command: Option<Command>,
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
        /// Start the session and return, instead of following it.
        #[arg(long)]
        detach: bool,
        /// Run in this process rather than handing the session to the daemon.
        #[arg(long)]
        foreground: bool,
    },
    /// Attach to a session: replay everything it has done, then follow it live.
    Attach {
        id: String,
        /// Replay from this event onwards (default: the beginning).
        #[arg(long, default_value = "0")]
        from: u64,
    },
    /// Send a message to a running session, waking it.
    Say {
        id: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Stop supervising a session.
    Stop { id: String },
    /// Run the supervisor in the foreground. launchd invokes this.
    Daemon,
    /// Install the daemon so sessions keep running whenever you are logged in.
    Install,
    /// Remove the daemon. Sessions on disk are left alone.
    Uninstall,
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
    /// Configure a provider: endpoint, API key, and model.
    Setup,
    /// Switch model, choosing from what the provider offers.
    Model {
        /// Set this model directly instead of choosing from a list.
        name: Option<String>,
    },
    /// Switch between saved providers, or add and remove them.
    Provider {
        /// Activate this profile directly instead of choosing from a list.
        name: Option<String>,
    },
    /// Show the current configuration.
    Config,
    /// Update ax to the latest release.
    Update {
        /// Report whether an update exists without installing it.
        #[arg(long)]
        check: bool,
    },
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
    // Every command reads its credentials the same way, including when launchd
    // starts the daemon with no shell environment at all.
    config::apply_active_profile()?;
    setup::install_render_config();

    let Some(command) = cli.command else {
        return welcome().await;
    };

    match command {
        Command::Run {
            goal,
            model,
            max_wakes,
            max_tokens,
            detach,
            foreground,
        } => {
            run(
                goal.join(" "),
                model,
                max_wakes,
                max_tokens,
                detach,
                foreground,
            )
            .await
        }
        Command::Attach { id, from } => attach(&id, from).await,
        Command::Say { id, text } => say(&id, &text.join(" ")).await,
        Command::Stop { id } => stop(&id).await,
        Command::Daemon => daemon::Daemon::start().await,
        Command::Install => install(),
        Command::Uninstall => {
            launchd::uninstall()?;
            println!("daemon removed");
            Ok(())
        }
        Command::Ls => list(),
        Command::Log { id, follow } => show_log(&id, follow).await,
        Command::Rm { id } => remove(&id),
        Command::Setup => setup::run(true).await,
        Command::Model { name } => setup::choose_model(name).await,
        Command::Provider { name } => setup::choose_provider(name).await,
        Command::Config => setup::show_config(),
        Command::Update { check } => update::run(check).await,
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
    Ok(std::sync::Arc::new(chrome::BrowserManager::new(
        home, relay,
    )))
}

/// `ax` with no arguments. A first-time user gets setup; everyone else gets a
/// short status and the commands worth knowing.
async fn welcome() -> Result<()> {
    if !config::is_configured() {
        println!("{}", ui::bold("Welcome to ax."));
        println!(
            "{}",
            ui::dim("An autonomous agent that pursues a goal by writing and running code.")
        );
        println!();
        return setup::run(false).await;
    }

    let sessions = Session::list()?;
    let live = sessions
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                ax::session::Status::Running | ax::session::Status::Sleeping
            )
        })
        .count();

    println!(
        "{} {}  {}",
        ui::bold("ax"),
        update::current_version(),
        ui::dim(&format!(
            "{} session{}, {live} active",
            sessions.len(),
            if sessions.len() == 1 { "" } else { "s" }
        ))
    );
    println!();
    println!("  {}   start a goal here", ui::bold("ax run \"...\""));
    println!("  {}              what is running", ui::bold("ax ls"));
    println!("  {}      watch one live", ui::bold("ax attach <id>"));
    println!();
    println!("{}", ui::dim("ax --help for everything else"));
    Ok(())
}

async fn run(
    goal: String,
    model: Option<String>,
    max_wakes: Option<u64>,
    max_tokens: Option<u64>,
    detach: bool,
    foreground: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;

    if !foreground {
        // Hand the session to the daemon so it outlives this terminal.
        ensure_daemon().await?;
        let id = match daemon::request(&ipc::Request::Create {
            goal: goal.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            model,
            max_wakes,
            max_tokens,
        })
        .await?
        {
            ipc::Response::Created { id } => id,
            ipc::Response::Error { message } => anyhow::bail!(message),
            _ => anyhow::bail!("the daemon gave an unexpected answer"),
        };

        println!(
            "session {}  {}",
            ui::bold(&id),
            ui::dim(&cwd.display().to_string())
        );
        if detach {
            println!("{}", ui::dim(&format!("following: ax attach {id}")));
            return Ok(());
        }
        println!(
            "{}",
            ui::dim("— attached; ctrl-c detaches, the session keeps running —")
        );
        return attach(&id, 0).await;
    }

    // Foreground mode: useful for debugging, and for running without a daemon.
    let config = llm::LlmConfig::from_env(model)?;
    let session = Session::create(&goal, &cwd, &config.model)?;
    println!(
        "session {}  {}  {}",
        ui::bold(&session.meta.id),
        ui::dim(&config.model),
        ui::dim(&cwd.display().to_string())
    );

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

/// Start the daemon if it is not already answering.
async fn ensure_daemon() -> Result<()> {
    if daemon::is_running().await {
        return Ok(());
    }

    // `process_group` lives on the unix extension trait.
    use std::os::unix::process::CommandExt;

    let binary = std::env::current_exe()?;
    std::process::Command::new(&binary)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Detach it from this terminal, or closing the window would take the
        // daemon — and every session — down with it.
        .process_group(0)
        .spawn()
        .context("could not start the ax daemon")?;

    for _ in 0..100 {
        if daemon::is_running().await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("the daemon did not come up; try `ax daemon` to see why")
}

/// Replay a session, then follow it. Ctrl-C detaches without stopping it.
async fn attach(id: &str, from: u64) -> Result<()> {
    if !daemon::is_running().await {
        // Without a daemon there is nothing live to follow, but the log on
        // disk is still the complete story.
        return show_log(id, false).await;
    }

    let socket = paths::daemon_socket()?;
    let stream = tokio::net::UnixStream::connect(&socket).await?;
    let (reader, mut writer) = stream.into_split();

    let mut line = serde_json::to_string(&ipc::Request::Attach {
        id: id.to_string(),
        from_seq: from,
    })?;
    line.push('\n');
    tokio::io::AsyncWriteExt::write_all(&mut writer, line.as_bytes()).await?;
    tokio::io::AsyncWriteExt::flush(&mut writer).await?;

    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(reader));
    while let Some(line) = lines.next_line().await? {
        match serde_json::from_str::<ipc::Response>(&line) {
            Ok(ipc::Response::Event { event }) => ui::render(&event),
            Ok(ipc::Response::Error { message }) => anyhow::bail!(message),
            _ => {}
        }
    }
    Ok(())
}

async fn say(id: &str, text: &str) -> Result<()> {
    match daemon::request(&ipc::Request::Say {
        id: id.to_string(),
        text: text.to_string(),
    })
    .await?
    {
        ipc::Response::Error { message } => anyhow::bail!(message),
        _ => {
            println!("{}", ui::dim("delivered; the session will wake"));
            Ok(())
        }
    }
}

async fn stop(id: &str) -> Result<()> {
    match daemon::request(&ipc::Request::Stop { id: id.to_string() }).await? {
        ipc::Response::Error { message } => anyhow::bail!(message),
        _ => {
            println!("stopped {id}");
            Ok(())
        }
    }
}

/// Capture the current configuration and register the launchd agent.
fn install() -> Result<()> {
    // The daemon reads ~/.ax/config.toml itself, so there is nothing to copy
    // unless credentials exist only in this shell.
    match config::capture_shell_credentials()? {
        Some(name) => println!("saved this shell's credentials as profile `{name}`"),
        None if !config::is_configured() => println!(
            "{}",
            ui::dim("warning: ax is not configured yet — run `ax setup` so the daemon can work")
        ),
        None => {}
    }

    let path = launchd::install()?;
    println!("daemon installed: {}", path.display());
    println!(
        "{}",
        ui::dim("it starts now and at every login; `ax uninstall` removes it")
    );
    Ok(())
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
