//! Making the active profile visible to the rest of the program.
//!
//! Configuration lives in `~/.ax/config.toml` (see [`crate::profile`]). Every
//! command applies the active profile to the process environment at startup,
//! which keeps one rule true everywhere: an explicit variable in the shell
//! always wins over the saved profile. That matters most for the daemon, which
//! launchd starts with almost no environment at all.

use crate::profile::Config;
use anyhow::Result;

/// Apply the active profile to this process, without overriding anything the
/// shell already set.
pub fn apply_active_profile() -> Result<()> {
    let config = Config::load()?;
    let Some(profile) = config.active_profile() else {
        return Ok(());
    };
    set_if_absent("AX_BASE_URL", &profile.base_url);
    set_if_absent("AX_API_KEY", &profile.api_key);
    set_if_absent("AX_MODEL", &profile.model);
    Ok(())
}

fn set_if_absent(key: &str, value: &str) {
    if value.is_empty() || std::env::var_os(key).is_some() {
        return;
    }
    // Safety: called once during startup, before any threads are spawned.
    unsafe { std::env::set_var(key, value) };
}

/// Is there a usable profile, or credentials in the environment?
pub fn is_configured() -> bool {
    if std::env::var("AX_API_KEY").is_ok() || std::env::var("OPENAI_API_KEY").is_ok() {
        return true;
    }
    Config::load()
        .map(|config| config.active_profile().is_some_and(|p| p.is_usable()))
        .unwrap_or(false)
}

/// If the shell has credentials but nothing is saved, capture them so the
/// daemon — which gets no shell environment — can use them too.
pub fn capture_shell_credentials() -> Result<Option<String>> {
    let mut config = Config::load()?;
    if config.active_profile().is_some_and(|p| p.is_usable()) {
        return Ok(None);
    }

    let key = std::env::var("AX_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .unwrap_or_default();
    if key.is_empty() {
        return Ok(None);
    }

    let name = config.unique_name("shell");
    config.upsert(
        &name,
        crate::profile::Profile {
            base_url: std::env::var("AX_BASE_URL")
                .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            api_key: key,
            model: std::env::var("AX_MODEL").unwrap_or_else(|_| "gpt-4.1".into()),
        },
    )?;
    Ok(Some(name))
}
