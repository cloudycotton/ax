//! First-run configuration.
//!
//! `ax` needs three things to work: an endpoint, a key, and a model. This asks
//! for them, checks they actually work by making one real request, and writes
//! them to `~/.ax/env` where every later command — including the daemon, which
//! launchd starts with no shell environment — can read them.

use crate::config;
use crate::llm::{LlmClient, LlmConfig};
use crate::ui;
use anyhow::{Result, bail};
use std::io::{self, BufRead, Write};

/// A known provider, so the common cases are one keystroke.
struct Preset {
    label: &'static str,
    base_url: &'static str,
    model: &'static str,
    key_hint: &'static str,
}

const PRESETS: &[Preset] = &[
    Preset {
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        model: "gpt-4.1",
        key_hint: "sk-…",
    },
    Preset {
        label: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        model: "anthropic/claude-sonnet-4.5",
        key_hint: "sk-or-…",
    },
    Preset {
        label: "Local (Ollama, llama.cpp, vLLM)",
        base_url: "http://127.0.0.1:11434/v1",
        model: "qwen2.5-coder",
        key_hint: "often anything, e.g. `local`",
    },
];

/// Run the interactive setup. `force` re-asks even if already configured.
pub async fn run(force: bool) -> Result<()> {
    if !is_interactive() {
        bail!(
            "setup needs a terminal. Set AX_API_KEY, AX_BASE_URL, and AX_MODEL in the \
environment instead, or write them to ~/.ax/env as KEY=value lines."
        );
    }

    let existing = config::read_env_file()?;
    let current = |key: &str| {
        existing
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };

    if config::is_configured() && !force {
        println!(
            "{}",
            ui::dim("Already configured. Re-running setup will replace it.")
        );
    }

    println!("{}", ui::bold("Where should ax send requests?"));
    for (index, preset) in PRESETS.iter().enumerate() {
        println!(
            "  {}. {} {}",
            index + 1,
            preset.label,
            ui::dim(preset.base_url)
        );
    }
    println!(
        "  {}. Something else (any OpenAI-compatible endpoint)",
        PRESETS.len() + 1
    );
    println!();

    let choice = ask("Choice", "1")?;
    let picked: usize = choice.parse().unwrap_or(1);

    let (base_url, default_model, key_hint) = if picked >= 1 && picked <= PRESETS.len() {
        let preset = &PRESETS[picked - 1];
        (
            preset.base_url.to_string(),
            preset.model.to_string(),
            preset.key_hint,
        )
    } else {
        let url = ask(
            "Base URL",
            &current("AX_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into()),
        )?;
        (
            url,
            current("AX_MODEL").unwrap_or_default(),
            "your provider's key",
        )
    };
    let base_url = base_url.trim_end_matches('/').to_string();

    println!();
    let key = read_secret(&format!("API key ({key_hint}): "))?;
    let key = if key.is_empty() {
        match current("AX_API_KEY") {
            Some(existing) if !existing.is_empty() => {
                println!("{}", ui::dim("keeping the key already saved"));
                existing
            }
            _ => bail!("an API key is required"),
        }
    } else {
        key
    };

    let model_default = if default_model.is_empty() {
        current("AX_MODEL").unwrap_or_else(|| "gpt-4.1".into())
    } else {
        default_model
    };
    let model = ask("Model", &model_default)?;

    // Prove it works before writing anything, so a typo is caught here rather
    // than at 3am in the middle of a long-running goal.
    print!("\nChecking… ");
    io::stdout().flush().ok();
    let probe = LlmConfig {
        base_url: base_url.clone(),
        api_key: key.clone(),
        model: model.clone(),
        max_tokens: None,
        temperature: None,
        include_usage: false,
    };
    match LlmClient::new(probe)?.probe().await {
        Ok(()) => println!("{}", ui::ok("works")),
        Err(err) => {
            println!("{}", ui::bad("failed"));
            println!("  {err}");
            println!();
            if !confirm("Save anyway?", false)? {
                bail!("setup cancelled; nothing was written");
            }
        }
    }

    config::write_env_values(&[
        ("AX_BASE_URL".into(), base_url),
        ("AX_API_KEY".into(), key),
        ("AX_MODEL".into(), model.clone()),
    ])?;

    let path = config::env_file()?;
    println!();
    println!(
        "{} {}",
        ui::ok("saved"),
        ui::dim(&path.display().to_string())
    );
    println!("{}", ui::dim("mode 600 — it holds your API key"));
    println!();
    println!("Start a goal:");
    println!("  {}", ui::bold("ax run \"...\""));
    println!();
    println!(
        "{}",
        ui::dim("Optional: `ax install` keeps sessions running in the background,")
    );
    println!(
        "{}",
        ui::dim("and `ax pair` connects ax to your own browser.")
    );
    Ok(())
}

fn is_interactive() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// Prompt with a default shown in brackets.
fn ask(prompt: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{prompt}: ");
    } else {
        print!("{prompt} [{}]: ", ui::dim(default));
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() {
        default.to_string()
    } else {
        line.to_string()
    })
}

fn confirm(prompt: &str, default: bool) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    let answer = ask(&format!("{prompt} ({hint})"), "")?;
    Ok(match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    })
}

/// Read a line with the terminal's echo turned off, so the key never appears
/// on screen or in a screen recording.
fn read_secret(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;

    let fd = libc::STDIN_FILENO;
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    let restore = unsafe { libc::tcgetattr(fd, &mut original) } == 0;
    if restore {
        let mut quiet = original;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    }

    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);

    // Always put the terminal back, even if the read failed.
    if restore {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
        println!();
    }
    read?;
    Ok(line.trim().to_string())
}
