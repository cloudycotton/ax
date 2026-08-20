//! Configuration, as a conversation with the terminal.
//!
//! Three principles here. Every answer is written to disk the moment it is
//! given, so an abandoned wizard loses nothing and re-running resumes. Models
//! are chosen from what the provider actually offers, never typed from memory.
//! And the prompts render inline — no alternate screen, no cleared scrollback —
//! so a session reads back afterwards like any other terminal output.

use crate::llm::{LlmClient, LlmConfig};
use crate::profile::Config;
use crate::ui;
use anyhow::{Result, bail};
use inquire::ui::{Color, RenderConfig, StyleSheet, Styled};
use inquire::{Confirm, InquireError, Password, PasswordDisplayMode, Select, Text};
use std::io::IsTerminal;

/// A known provider, so the common cases are two keystrokes.
struct Preset {
    label: &'static str,
    slug: &'static str,
    base_url: &'static str,
    fallback_model: &'static str,
}

const PRESETS: &[Preset] = &[
    Preset {
        label: "OpenAI",
        slug: "openai",
        base_url: "https://api.openai.com/v1",
        fallback_model: "gpt-4.1",
    },
    Preset {
        label: "OpenRouter",
        slug: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        fallback_model: "anthropic/claude-sonnet-4.5",
    },
    Preset {
        label: "Groq",
        slug: "groq",
        base_url: "https://api.groq.com/openai/v1",
        fallback_model: "llama-3.3-70b-versatile",
    },
    Preset {
        label: "Local (Ollama, llama.cpp, vLLM)",
        slug: "local",
        base_url: "http://127.0.0.1:11434/v1",
        fallback_model: "qwen2.5-coder",
    },
];

/// Add or reconfigure a provider.
pub async fn run(_force: bool) -> Result<()> {
    require_terminal()?;
    let mut config = Config::load()?;

    // 1. Where requests go.
    let mut choices: Vec<String> = PRESETS
        .iter()
        .map(|p| format!("{}  ({})", p.label, p.base_url))
        .collect();
    choices.push("Something else (any OpenAI-compatible endpoint)".into());

    let (index, _) = select("Where should ax send requests?", choices, 0)?;

    let (slug, base_url, fallback_model) = if index < PRESETS.len() {
        let preset = &PRESETS[index];
        (
            preset.slug.to_string(),
            preset.base_url.to_string(),
            preset.fallback_model.to_string(),
        )
    } else {
        let url = text("Base URL", "https://api.openai.com/v1")?;
        ("custom".to_string(), url, String::new())
    };
    let base_url = base_url.trim_end_matches('/').to_string();
    let name = config.unique_name(&slug);

    // Saved before the key is even asked for: nothing typed is ever lost.
    config.patch(&name, |profile| profile.base_url = base_url.clone())?;

    // 2. The key.
    let key = password("API key")?;
    if key.is_empty() {
        config.remove(&name).ok();
        bail!("an API key is required");
    }
    config.patch(&name, |profile| profile.api_key = key.clone())?;
    println!("{}", ui::dim(&format!("saved as profile `{name}`")));

    // 3. The model, chosen from what this provider actually serves.
    let model = pick_model(&base_url, &key, &fallback_model, None).await?;
    config.patch(&name, |profile| profile.model = model.clone())?;
    config.set_active(&name)?;

    verify(&base_url, &key, &model).await;
    summary(&config)?;
    Ok(())
}

/// Switch the active profile's model.
pub async fn choose_model(name: Option<String>) -> Result<()> {
    let mut config = Config::load()?;
    let active = config.active.clone();
    let Some(profile) = config.active_profile().cloned() else {
        bail!("nothing is configured yet — run `ax setup`");
    };

    let model = match name {
        Some(name) => name,
        None => {
            require_terminal()?;
            pick_model(
                &profile.base_url,
                &profile.api_key,
                &profile.model,
                Some(&profile.model),
            )
            .await?
        }
    };

    config.patch(&active, |p| p.model = model.clone())?;
    println!("{} {}", ui::ok("model:"), ui::bold(&model));
    Ok(())
}

/// Switch between saved providers.
pub async fn choose_provider(name: Option<String>) -> Result<()> {
    let mut config = Config::load()?;

    if let Some(name) = name {
        config.set_active(&name)?;
        println!("{} {}", ui::ok("provider:"), ui::bold(&name));
        return Ok(());
    }

    require_terminal()?;
    if config.profiles.is_empty() {
        println!("{}", ui::dim("no providers saved yet"));
        return run(true).await;
    }

    const ADD: &str = "+ Add another provider…";
    let mut labels: Vec<String> = config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let marker = if *name == config.active { "●" } else { " " };
            format!(
                "{marker} {name}  {}  {}",
                profile.model,
                ui::dim(&profile.base_url)
            )
        })
        .collect();
    labels.push(ADD.to_string());

    let start = config
        .profiles
        .keys()
        .position(|n| *n == config.active)
        .unwrap_or(0);
    let (index, picked) = select("Which provider?", labels, start)?;
    if picked == ADD {
        return run(true).await;
    }
    let chosen = config
        .profiles
        .keys()
        .nth(index)
        .cloned()
        .unwrap_or_default();
    config.set_active(&chosen)?;
    println!("{} {}", ui::ok("provider:"), ui::bold(&chosen));
    Ok(())
}

/// Print what is configured, without revealing keys.
pub fn show_config() -> Result<()> {
    let config = Config::load()?;
    if config.profiles.is_empty() {
        println!("{}", ui::dim("nothing configured yet — run `ax setup`"));
        return Ok(());
    }

    for (name, profile) in &config.profiles {
        let marker = if *name == config.active {
            ui::ok("●")
        } else {
            ui::dim("○")
        };
        println!("{marker} {}", ui::bold(name));
        println!("    endpoint  {}", profile.base_url);
        println!("    key       {}", ui::dim(&profile.redacted_key()));
        println!("    model     {}", profile.model);
        println!(
            "    vision    {}",
            if crate::vision::model_sees(&profile.model) {
                ui::dim("yes — can look at screenshots and images")
            } else {
                ui::dim("no — works from the accessibility tree only")
            }
        );
    }
    println!();
    println!(
        "{}",
        ui::dim(&crate::profile::config_path()?.display().to_string())
    );
    Ok(())
}

/// Offer the provider's own model list, falling back to typing a name.
async fn pick_model(
    base_url: &str,
    api_key: &str,
    fallback: &str,
    current: Option<&str>,
) -> Result<String> {
    print!("fetching models… ");
    flush();

    let client = LlmClient::new(LlmConfig {
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        model: fallback.to_string(),
        max_tokens: None,
        temperature: None,
        include_usage: false,
    })?;

    match client.list_models().await {
        Ok(models) => {
            println!("{}", ui::ok(&format!("{} available", models.len())));
            // Start on the model already in use, so re-running is a no-op if
            // you just press enter.
            let start = current
                .and_then(|c| models.iter().position(|m| m == c))
                .unwrap_or(0);
            select("Model  (type to filter)", models, start).map(|(_, model)| model)
        }
        Err(err) => {
            println!("{}", ui::dim("unavailable"));
            println!("  {}", ui::dim(&err.to_string()));
            let default = current.unwrap_or(fallback);
            text("Model", default)
        }
    }
}

async fn verify(base_url: &str, api_key: &str, model: &str) {
    print!("checking… ");
    flush();
    let client = LlmClient::new(LlmConfig {
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        model: model.to_string(),
        max_tokens: None,
        temperature: None,
        include_usage: false,
    });
    match client {
        Ok(client) => match client.probe().await {
            Ok(()) => println!("{}", ui::ok("works")),
            Err(err) => {
                println!("{}", ui::bad("failed"));
                println!("  {err}");
                println!(
                    "{}",
                    ui::dim("saved anyway — fix it with `ax setup` or `ax model`")
                );
            }
        },
        Err(err) => println!("{} {err}", ui::bad("failed")),
    }
}

fn summary(config: &Config) -> Result<()> {
    let Some(profile) = config.active_profile() else {
        return Ok(());
    };
    println!();
    println!(
        "{} {}  {}",
        ui::ok("ready"),
        ui::bold(&profile.model),
        ui::dim(&format!("via {}", config.active))
    );
    println!();
    println!("  {}   start a goal", ui::bold("ax run \"...\""));
    println!("  {}             switch model", ui::bold("ax model"));
    println!("  {}          switch provider", ui::bold("ax provider"));
    Ok(())
}

// ------------------------------------------------------------------ prompts

fn require_terminal() -> Result<()> {
    // inquire drives the terminal through crossterm, which reads /dev/tty
    // rather than stdin, so a redirected stdin alone is not disqualifying.
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        return Ok(());
    }
    bail!(
        "this needs a terminal. Set AX_API_KEY, AX_BASE_URL, and AX_MODEL in the environment \
instead, or edit ~/.ax/config.toml directly."
    )
}

/// Match the colours the rest of ax already uses, so prompts do not look like
/// a different program. Honours NO_COLOR by leaving inquire's plain default.
pub fn install_render_config() {
    if std::env::var_os("NO_COLOR").is_some() {
        return;
    }
    let config = RenderConfig::default_colored()
        .with_prompt_prefix(Styled::new("›").with_fg(Color::LightGreen))
        .with_answered_prompt_prefix(Styled::new("✓").with_fg(Color::LightGreen))
        .with_highlighted_option_prefix(Styled::new("›").with_fg(Color::LightCyan))
        .with_help_message(StyleSheet::new().with_fg(Color::DarkGrey))
        .with_answer(StyleSheet::new().with_fg(Color::LightCyan));
    inquire::set_global_render_config(config);
}

/// Cancelling a prompt is a normal thing to do, not an error to shout about.
fn handle(err: InquireError) -> anyhow::Error {
    match err {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => {
            anyhow::anyhow!("cancelled — anything already answered has been saved")
        }
        InquireError::NotTTY => {
            anyhow::anyhow!("this needs a terminal; edit ~/.ax/config.toml instead")
        }
        other => anyhow::anyhow!(other),
    }
}

/// Returns the chosen option together with its index in the list as given,
/// which is what lets callers map a selection back to their own data.
fn select(message: &str, options: Vec<String>, start: usize) -> Result<(usize, String)> {
    if options.is_empty() {
        bail!("nothing to choose from");
    }
    // An out-of-range cursor is a hard error inside inquire.
    let start = start.min(options.len() - 1);
    Select::new(message, options)
        .with_starting_cursor(start)
        .with_page_size(12)
        .raw_prompt()
        .map_err(handle)
        .map(|choice| (choice.index, choice.value))
}

fn text(message: &str, default: &str) -> Result<String> {
    let prompt = Text::new(message);
    let prompt = if default.is_empty() {
        prompt
    } else {
        prompt.with_default(default)
    };
    prompt
        .prompt()
        .map_err(handle)
        .map(|v| v.trim().to_string())
}

fn password(message: &str) -> Result<String> {
    Password::new(message)
        .without_confirmation()
        // Show something as they type: a blank line reads as a frozen terminal.
        .with_display_mode(PasswordDisplayMode::Masked)
        .prompt()
        .map_err(handle)
        .map(|v| v.trim().to_string())
}

#[allow(dead_code)]
fn confirm(message: &str, default: bool) -> Result<bool> {
    Confirm::new(message)
        .with_default(default)
        .prompt()
        .map_err(handle)
}

fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}
