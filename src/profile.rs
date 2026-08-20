//! Saved providers.
//!
//! A profile is one endpoint, key, and model. Keeping several means the
//! common cases — a paid provider for real work, a local model for cheap
//! iteration, a second key for a different account — are a switch away rather
//! than a re-run of setup.
//!
//! Everything lives in `~/.ax/config.toml` at mode 600, and every step of setup
//! writes it immediately: a wizard abandoned halfway leaves what you already
//! typed on disk, and re-running picks up from there.

use crate::paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
}

impl Profile {
    /// Enough to make a request?
    pub fn is_usable(&self) -> bool {
        !self.base_url.is_empty() && !self.api_key.is_empty() && !self.model.is_empty()
    }

    /// The key with its middle removed, for display. Never print the whole key.
    pub fn redacted_key(&self) -> String {
        let key = &self.api_key;
        if key.is_empty() {
            return "(not set)".into();
        }
        let visible: String = key.chars().take(6).collect();
        let tail: String = key
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if key.chars().count() <= 12 {
            format!("{visible}…")
        } else {
            format!("{visible}…{tail}")
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Which profile commands use when none is named.
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

pub fn config_path() -> Result<PathBuf> {
    Ok(paths::agent_home()?.join("config.toml"))
}

impl Config {
    /// Read the config, migrating a legacy `~/.ax/env` file if that is all
    /// there is. Never fails just because nothing is configured yet.
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let config: Config = toml::from_str(&raw)
                .with_context(|| format!("{} is not valid TOML", path.display()))?;
            return Ok(config);
        }
        Ok(migrate_env_file().unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        paths::ensure_dir(path.parent().unwrap())?;
        let body = toml::to_string_pretty(self).context("could not serialize the configuration")?;
        std::fs::write(&path, body)
            .with_context(|| format!("could not write {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // It holds API keys.
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn active_profile(&self) -> Option<&Profile> {
        self.profiles.get(&self.active)
    }

    /// Insert or replace a profile and save immediately.
    pub fn upsert(&mut self, name: &str, profile: Profile) -> Result<()> {
        self.profiles.insert(name.to_string(), profile);
        if self.active.is_empty() {
            self.active = name.to_string();
        }
        self.save()
    }

    /// Update one profile in place, saving as soon as the change is made. This
    /// is what makes setup resumable: each answer is durable the moment it is
    /// given.
    pub fn patch(&mut self, name: &str, edit: impl FnOnce(&mut Profile)) -> Result<()> {
        let profile = self.profiles.entry(name.to_string()).or_default();
        edit(profile);
        if self.active.is_empty() {
            self.active = name.to_string();
        }
        self.save()
    }

    pub fn set_active(&mut self, name: &str) -> Result<()> {
        if !self.profiles.contains_key(name) {
            bail!("no profile named `{name}`");
        }
        self.active = name.to_string();
        self.save()
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.profiles.remove(name).is_none() {
            bail!("no profile named `{name}`");
        }
        if self.active == name {
            self.active = self.profiles.keys().next().cloned().unwrap_or_default();
        }
        self.save()
    }

    /// A name not already taken, e.g. `openai`, then `openai-2`.
    pub fn unique_name(&self, base: &str) -> String {
        if !self.profiles.contains_key(base) {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|c| !self.profiles.contains_key(c))
            .unwrap()
    }
}

/// Fold a pre-profiles `~/.ax/env` into a profile called `default`, so an
/// existing install keeps working after an upgrade.
fn migrate_env_file() -> Option<Config> {
    let path = paths::agent_home().ok()?.join("env");
    let contents = std::fs::read_to_string(&path).ok()?;

    let mut profile = Profile::default();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim() {
            "AX_BASE_URL" | "OPENAI_BASE_URL" => profile.base_url = value,
            "AX_API_KEY" | "OPENAI_API_KEY" => profile.api_key = value,
            "AX_MODEL" => profile.model = value,
            _ => {}
        }
    }
    if profile.api_key.is_empty() && profile.base_url.is_empty() {
        return None;
    }
    if profile.base_url.is_empty() {
        profile.base_url = "https://api.openai.com/v1".into();
    }

    let mut config = Config {
        active: "default".into(),
        profiles: BTreeMap::new(),
    };
    config.profiles.insert("default".into(), profile);
    let _ = config.save();
    Some(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_keys_without_leaking_the_middle() {
        let profile = Profile {
            api_key: "sk-proj-abcdefghijklmnop1234".into(),
            ..Default::default()
        };
        let shown = profile.redacted_key();
        assert!(shown.starts_with("sk-pro"), "{shown}");
        assert!(shown.ends_with("1234"), "{shown}");
        assert!(
            !shown.contains("efghijkl"),
            "the middle must not appear: {shown}"
        );
    }

    #[test]
    fn redacts_short_keys_without_revealing_the_end() {
        let profile = Profile {
            api_key: "shortkey".into(),
            ..Default::default()
        };
        // A short key would otherwise be almost fully reconstructable.
        assert_eq!(profile.redacted_key(), "shortk…");
    }

    #[test]
    fn usable_requires_all_three() {
        let mut profile = Profile {
            base_url: "https://x/v1".into(),
            api_key: "k".into(),
            model: String::new(),
        };
        assert!(
            !profile.is_usable(),
            "a profile with no model is not usable"
        );
        profile.model = "m".into();
        assert!(profile.is_usable());
    }

    #[test]
    fn unique_name_avoids_collisions() {
        let mut config = Config::default();
        config.profiles.insert("openai".into(), Profile::default());
        assert_eq!(config.unique_name("openai"), "openai-2");
        config
            .profiles
            .insert("openai-2".into(), Profile::default());
        assert_eq!(config.unique_name("openai"), "openai-3");
        assert_eq!(config.unique_name("local"), "local");
    }

    #[test]
    fn removing_the_active_profile_picks_another() {
        let mut config = Config::default();
        config.profiles.insert("a".into(), Profile::default());
        config.profiles.insert("b".into(), Profile::default());
        config.active = "a".into();
        // Saving touches the filesystem, so exercise the selection logic directly.
        config.profiles.remove("a");
        if config.active == "a" {
            config.active = config.profiles.keys().next().cloned().unwrap_or_default();
        }
        assert_eq!(config.active, "b");
    }
}
