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

/// Is there enough configuration to talk to a model? Checks the live
/// environment and the env file, so it is accurate before `load_env_file`.
pub fn is_configured() -> bool {
    if std::env::var("AX_API_KEY").is_ok() || std::env::var("OPENAI_API_KEY").is_ok() {
        return true;
    }
    let Ok(values) = read_env_file() else {
        return false;
    };
    values.iter().any(|(key, value)| {
        matches!(key.as_str(), "AX_API_KEY" | "OPENAI_API_KEY") && !value.is_empty()
    })
}

/// The env file's contents, in file order. Comments and blanks are dropped.
pub fn read_env_file() -> Result<Vec<(String, String)>> {
    let path = env_file()?;
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    Ok(contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect())
}

/// Merge values into the env file, leaving keys we were not asked about
/// untouched. An empty value removes the key.
pub fn write_env_values(updates: &[(String, String)]) -> Result<()> {
    let mut values = read_env_file()?;
    for (key, value) in updates {
        match values.iter_mut().find(|(existing, _)| existing == key) {
            Some(slot) => slot.1 = value.clone(),
            None => values.push((key.clone(), value.clone())),
        }
    }
    values.retain(|(_, value)| !value.is_empty());
    write_all(&values)
}

fn write_all(values: &[(String, String)]) -> Result<()> {
    let path = env_file()?;
    paths::ensure_dir(path.parent().unwrap())?;
    let mut body =
        String::from("# Written by `ax setup`. Read at startup, including by the daemon.\n");
    for (key, value) in values {
        body.push_str(&format!("{key}={value}\n"));
    }
    std::fs::write(&path, body).with_context(|| format!("could not write {}", path.display()))?;
    restrict(&path);
    Ok(())
}

/// The file holds an API key, so it must not be readable by other users.
fn restrict(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

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
    let mut updates = Vec::new();
    for key in CARRIED {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            updates.push((key.to_string(), value));
        }
    }
    write_env_values(&updates)?;
    Ok(updates.into_iter().map(|(key, _)| key).collect())
}
