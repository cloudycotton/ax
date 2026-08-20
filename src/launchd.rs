//! Installing the daemon as a per-user launchd agent, so goals keep running
//! whenever the machine is on and the user is logged in.

use crate::paths;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

pub const LABEL: &str = "com.ax.daemon";

pub fn plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn plist(binary: &std::path::Path, home: &std::path::Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{home}/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>{home}/daemon.log</string>
</dict>
</plist>
"#,
        LABEL = LABEL,
        binary = binary.display(),
        home = home.display(),
    )
}

/// Write the plist and load it. The daemon starts immediately and again at
/// every login.
pub fn install() -> Result<PathBuf> {
    let binary = std::env::current_exe().context("could not locate the ax binary")?;
    let home = paths::agent_home()?;
    paths::ensure_dir(&home)?;

    let path = plist_path()?;
    paths::ensure_dir(path.parent().unwrap())?;
    std::fs::write(&path, plist(&binary, &home))
        .with_context(|| format!("could not write {}", path.display()))?;

    // Replace any previous registration; `bootout` failing just means it was
    // not loaded to begin with.
    let target = format!("gui/{}", unsafe { libc::getuid() });
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{target}/{LABEL}")])
        .output();

    let output = std::process::Command::new("launchctl")
        .args(["bootstrap", &target, &path.to_string_lossy()])
        .output()
        .context("could not run launchctl")?;
    if !output.status.success() {
        bail!(
            "launchctl refused to load the agent: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(path)
}

/// Unload and remove the launchd agent. Sessions themselves are untouched.
pub fn uninstall() -> Result<()> {
    let target = format!("gui/{}", unsafe { libc::getuid() });
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{target}/{LABEL}")])
        .output();
    let path = plist_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn plist_is_well_formed_and_points_at_the_binary() {
        let rendered = plist(Path::new("/usr/local/bin/ax"), Path::new("/Users/x/.ax"));

        assert!(rendered.starts_with("<?xml version=\"1.0\""));
        assert!(rendered.contains(&format!("<string>{LABEL}</string>")));
        assert!(rendered.contains("<string>/usr/local/bin/ax</string>"));
        assert!(rendered.contains("<string>daemon</string>"));
        // Without these the agent would not survive login or a crash.
        assert!(rendered.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(rendered.contains("<key>KeepAlive</key>\n    <true/>"));
        // Logs must land inside the agent's own directory.
        assert!(rendered.contains("<string>/Users/x/.ax/daemon.log</string>"));

        // Every opened tag closes: a malformed plist is silently ignored by
        // launchd, which is a maddening way to fail.
        assert_eq!(rendered.matches("<dict>").count(), rendered.matches("</dict>").count());
        assert_eq!(rendered.matches("<array>").count(), rendered.matches("</array>").count());
        assert_eq!(rendered.matches("<string>").count(), rendered.matches("</string>").count());
    }
}
