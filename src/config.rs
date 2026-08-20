//! Configuration the daemon can read without a shell.
//!
//! launchd starts the daemon with almost no environment, so the API key and
//! model cannot come from the user's profile. They live in `~/.ax/env`
//! instead — mode 600, one `KEY=value` per line — which also keeps secrets out
//! of the launchd plist, where they would sit world-readable.

use crate::paths;
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Variables worth carrying across into the daemon.
const CARRIED: &[&str] = &[
    "AX_API_KEY",
    "AX_BASE_URL",
    "AX_MODEL",
    "AX_CHROME",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
];

pub fn env_file() -> Result<PathBuf> {
    Ok(paths::agent_home()?.join("env"))
}

/// Apply `~/.ax/env`, without overriding anything already set in the real
/// environment — an explicit variable in the shell should always win.
pub fn load_env_file() -> Result<()> {
    let path = env_file()?;
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        // Safety: called once during startup, before any threads are spawned.
        unsafe { std::env::set_var(key, value) };
    }
    Ok(())
}

/// Capture the current shell's configuration for the daemon to use later.
/// Returns the variables written.
pub fn save_env_file() -> Result<Vec<String>> {
    let path = env_file()?;
    paths::ensure_dir(path.parent().unwrap())?;

    let mut saved = Vec::new();
    let mut body = String::from("# Written by `ax install`. Read by the daemon at startup.\n");
    for key in CARRIED {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                body.push_str(&format!("{key}={value}\n"));
                saved.push(key.to_string());
            }
        }
    }

    std::fs::write(&path, body).with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // It holds an API key.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(saved)
}
