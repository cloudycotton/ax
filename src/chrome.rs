//! Getting a browser to drive.
//!
//! Preference order, decided at connect time:
//!
//! 1. **The user's real browser**, reached through our Chrome extension. This
//!    is the one that matters: it has their sessions, their cookies, their
//!    logins. See [`crate::relay`].
//! 2. **A browser we launch ourselves**, on a dedicated profile under
//!    `~/.ax/chrome`, with a debugging port. Always available, and anything
//!    the agent signs into there persists — but it starts out logged into
//!    nothing.
//!
//! We never try to attach a debugging port to the user's default profile:
//! Chrome 136+ refuses that combination outright, and working around it by
//! copying their cookie store would be both fragile and exactly what malware
//! does.

use crate::cdp::CdpClient;
use crate::relay::Relay;
use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// How long to wait for the extension after opening the browser. Long enough
/// for a cold start and the service worker waking up, short enough that a
/// missing extension does not stall a wake.
const EXTENSION_WAIT: Duration = Duration::from_secs(20);

const CHROME_CANDIDATES: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
];

/// Holds whichever browser connection is currently live.
pub struct BrowserManager {
    agent_home: PathBuf,
    client: Mutex<Option<Arc<CdpClient>>>,
    relay: Arc<Relay>,
}

impl BrowserManager {
    pub fn new(agent_home: PathBuf, relay: Arc<Relay>) -> Self {
        Self {
            agent_home,
            client: Mutex::new(None),
            relay,
        }
    }

    /// Return a live CDP connection, establishing one if needed.
    pub async fn ensure(&self) -> Result<Arc<CdpClient>> {
        let mut slot = self.client.lock().await;
        if let Some(existing) = slot.as_ref()
            && existing.is_open()
        {
            return Ok(existing.clone());
        }

        // The extension gives us the user's actual browser, with their logins.
        // Always prefer it.
        if self.relay.has_extension() {
            let client = self.relay.connect().await?;
            *slot = Some(client.clone());
            return Ok(client);
        }

        // Paired before, but nothing connected right now: their browser is
        // simply closed. Open it and give the extension a moment to dial in,
        // rather than silently falling back to a profile with no logins.
        if self.relay.has_paired_before() {
            match open_user_browser() {
                Ok(app) => {
                    eprintln!("\x1b[2mopening {app} and waiting for the ax extension…\x1b[0m");
                    if self.relay.wait_for_extension(EXTENSION_WAIT).await {
                        let client = self.relay.connect().await?;
                        *slot = Some(client.clone());
                        return Ok(client);
                    }
                    eprintln!(
                        "\x1b[33m! the extension did not connect; using ax's own browser \
profile instead (no logins). Check chrome://extensions.\x1b[0m"
                    );
                }
                Err(err) => eprintln!("\x1b[33m! could not open your browser: {err}\x1b[0m"),
            }
        }

        let client = self.launch_managed().await?;
        *slot = Some(client.clone());
        Ok(client)
    }

    /// Describe the current state without changing it.
    pub async fn status(&self) -> serde_json::Value {
        let connected = self
            .client
            .lock()
            .await
            .as_ref()
            .map(|c| (c.is_open(), c.kind));
        serde_json::json!({
            "connected": connected.map(|(open, _)| open).unwrap_or(false),
            "transport": connected.map(|(_, kind)| kind),
            "extension_available": self.relay.has_extension(),
            "profile": self.profile_dir().to_string_lossy(),
        })
    }

    fn profile_dir(&self) -> PathBuf {
        self.agent_home.join("chrome")
    }

    /// Reuse the agent's own Chrome if it is already running, otherwise start
    /// it. The profile persists, so logins survive across sessions.
    async fn launch_managed(&self) -> Result<Arc<CdpClient>> {
        let profile = self.profile_dir();
        std::fs::create_dir_all(&profile)
            .with_context(|| format!("could not create {}", profile.display()))?;

        // An already-running instance leaves its endpoint behind; try it first.
        if let Some(url) = read_endpoint(&profile) {
            if let Ok(client) = CdpClient::connect(&url, "managed").await {
                return Ok(client);
            }
            // Stale file from a browser that has since exited.
            let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
        }

        let binary = find_browser().ok_or_else(|| {
            anyhow!(
                "no Chromium-based browser found. Install Google Chrome, or set \
AX_CHROME to the executable path."
            )
        })?;

        let mut command = tokio::process::Command::new(&binary);
        command
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-networking")
            .arg("--disable-features=Translate,MediaRouter")
            // Start on a blank page rather than restoring whatever was open.
            .arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Outlive this wake: the browser should still be there next time.
            .process_group(0);

        if std::env::var("AX_CHROME_HEADLESS").is_ok() {
            command.arg("--headless=new");
        }

        command
            .spawn()
            .with_context(|| format!("could not start {}", binary.display()))?;

        // Chrome writes the port only once it is actually listening.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(url) = read_endpoint(&profile)
                && let Ok(client) = CdpClient::connect(&url, "managed").await
            {
                return Ok(client);
            }
            if std::time::Instant::now() > deadline {
                bail!("the browser started but never opened a debugging port");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Bring the user's own browser up, without a debugging port — the extension
/// is what reaches inside it. On macOS this goes through `open -a` so the
/// running instance is reused rather than a second one started.
fn open_user_browser() -> Result<String> {
    let binary =
        find_browser().ok_or_else(|| anyhow!("no Chromium-based browser found to open"))?;

    #[cfg(target_os = "macos")]
    {
        // /Applications/Google Chrome.app/Contents/MacOS/… -> the .app bundle
        let app = binary
            .ancestors()
            .find(|path| path.extension().is_some_and(|ext| ext == "app"))
            .ok_or_else(|| {
                anyhow!(
                    "could not find the application bundle for {}",
                    binary.display()
                )
            })?;
        let name = app
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "your browser".into());
        let status = std::process::Command::new("open")
            .arg("-a")
            .arg(app)
            .status()
            .context("could not run `open`")?;
        if !status.success() {
            bail!("`open -a` failed for {}", app.display());
        }
        Ok(name)
    }

    #[cfg(not(target_os = "macos"))]
    {
        // `process_group` lives on the unix extension trait, not on Command.
        use std::os::unix::process::CommandExt;

        let name = binary
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "your browser".into());
        std::process::Command::new(&binary)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .with_context(|| format!("could not start {}", binary.display()))?;
        Ok(name)
    }
}

/// Chrome records `<port>\n<ws path>` here once its debugging server is up.
fn read_endpoint(profile: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let mut lines = raw.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some(format!("ws://127.0.0.1:{port}{path}"))
}

fn find_browser() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("AX_CHROME") {
        let path = PathBuf::from(custom);
        if path.exists() {
            return Some(path);
        }
    }
    CHROME_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}
