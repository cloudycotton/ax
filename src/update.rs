//! Self-update from GitHub Releases.
//!
//! Downloads the build for this platform, checks it against the release's
//! published SHA-256, and swaps it in. The checksum matters: this replaces the
//! binary that will later run with the user's credentials and their browser
//! session, so a corrupted or substituted download must not survive.

use crate::ui;
use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Overridable so a fork can point somewhere else.
fn repo() -> String {
    std::env::var("AX_REPO").unwrap_or_else(|_| "cloudycotton/ax".to_string())
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The release-asset name for the platform we were built for.
pub fn target_triple() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        (os, arch) => bail!("no prebuilt binaries for {os}/{arch}; build from source with cargo"),
    })
}

/// Check for a newer release, and install it unless `check_only`.
pub async fn run(check_only: bool) -> Result<()> {
    let target = target_triple()?;
    let current = current_version();
    println!("{} {}", ui::dim("installed"), current);

    let release = latest_release().await?;
    let latest = release.tag.trim_start_matches('v').to_string();
    println!("{} {}", ui::dim("latest   "), latest);

    if !is_newer(&latest, current) {
        println!("\n{}", ui::ok("already up to date"));
        return Ok(());
    }
    println!(
        "\n{} {} → {}",
        ui::bold("update available:"),
        current,
        latest
    );
    if check_only {
        println!("{}", ui::dim("run `ax update` to install it"));
        return Ok(());
    }

    let exe = std::env::current_exe().context("could not find the running binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    writable_or_explain(&exe)?;

    let asset_name = format!("ax-{latest}-{target}.tar.gz");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| anyhow!("release {} has no build for {target}", release.tag))?;

    print!("downloading {asset_name}… ");
    flush();
    let bytes = download(&asset.url).await?;
    println!("{}", ui::ok(&format!("{} KB", bytes.len() / 1024)));

    // Verify against the checksums published alongside the archive.
    match release.assets.iter().find(|a| a.name == "checksums.txt") {
        Some(sums) => {
            print!("verifying… ");
            flush();
            let expected = expected_digest(
                &String::from_utf8_lossy(&download(&sums.url).await?),
                &asset_name,
            )
            .ok_or_else(|| anyhow!("{asset_name} is not listed in checksums.txt"))?;
            let actual = hex_digest(&bytes);
            if actual != expected {
                bail!(
                    "checksum mismatch for {asset_name}\n  expected {expected}\n  got      {actual}"
                );
            }
            println!("{}", ui::ok("sha256 ok"));
        }
        // Refuse rather than silently installing something unverified.
        None => bail!(
            "release {} publishes no checksums.txt; refusing to install",
            release.tag
        ),
    }

    install_over(&exe, &bytes)?;
    println!(
        "\n{} {} {}",
        ui::ok("updated to"),
        latest,
        ui::dim(&exe.display().to_string())
    );
    Ok(())
}

struct Release {
    tag: String,
    assets: Vec<Asset>,
}

struct Asset {
    name: String,
    /// The API URL, which works for private repositories too.
    url: String,
}

async fn latest_release() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let body: serde_json::Value =
        serde_json::from_slice(&http(&url, "application/vnd.github+json").await?)
            .context("GitHub returned something that was not JSON")?;

    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no releases published yet for {}", repo()))?
        .to_string();
    let assets = body
        .get("assets")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a.get("name")?.as_str()?.to_string(),
                        url: a.get("url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { tag, assets })
}

async fn download(url: &str) -> Result<Vec<u8>> {
    http(url, "application/octet-stream").await
}

/// One authenticated GitHub request. A token is only needed while the
/// repository is private, but is used whenever present.
async fn http(url: &str, accept: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut request = client
        .get(url)
        .header("accept", accept)
        // GitHub rejects requests without one.
        .header("user-agent", format!("ax/{}", current_version()));

    if let Some(token) = github_token() {
        request = request.bearer_auth(token);
    }

    let response = request.send().await.with_context(|| format!("GET {url}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response.bytes().await?.to_vec());
    }

    // Say what actually went wrong. The two common failures look nothing alike
    // to a user but both arrive as an error status.
    let authenticated = github_token().is_some();
    match status.as_u16() {
        404 => bail!(
            "{} has no releases, or is private.\n\
             If it is private, set GITHUB_TOKEN to a token with `repo` scope.",
            repo()
        ),
        403 | 429 if !authenticated => bail!(
            "GitHub rate-limited this IP (unauthenticated requests are capped at 60/hour).\n\
             Wait a few minutes, or set GITHUB_TOKEN."
        ),
        403 | 429 => {
            bail!("GitHub refused the request ({status}); the token may lack `repo` scope.")
        }
        _ => bail!("GitHub returned {status} for {url}"),
    }
}

fn github_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN", "AX_GITHUB_TOKEN"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
}

fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Pull one file's digest out of a `sha256sum`-style listing.
pub fn expected_digest(listing: &str, filename: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == filename).then(|| digest.to_lowercase())
    })
}

/// Compare dotted versions numerically, so 0.10.0 beats 0.9.0.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    let (a, b) = (parts(candidate), parts(current));
    for index in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(index).copied().unwrap_or(0),
            b.get(index).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

fn writable_or_explain(exe: &Path) -> Result<()> {
    let dir = exe.parent().unwrap_or(Path::new("/"));
    let probe = dir.join(".ax-write-test");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(_) => bail!(
            "{} is not writable by this user.\n\
             Either re-run with sudo, or install ax somewhere you own (~/.local/bin).",
            dir.display()
        ),
    }
}

/// Unpack the archive and move the new binary into place.
///
/// The rename is what makes this safe: on Unix it is atomic, and replacing the
/// running binary's directory entry does not disturb the running process.
fn install_over(exe: &Path, archive: &[u8]) -> Result<()> {
    let staging = exe
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".ax-update-{}", std::process::id()));
    std::fs::create_dir_all(&staging)?;

    let result = (|| -> Result<()> {
        let tarball = staging.join("ax.tar.gz");
        std::fs::write(&tarball, archive)?;

        let status = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&staging)
            .status()
            .context("could not run tar")?;
        if !status.success() {
            bail!("tar failed to unpack the release archive");
        }

        // Refresh the extension too, so a pinned old copy never drifts from
        // the protocol the binary speaks.
        let unpacked_extension = staging.join("extension");
        if unpacked_extension.join("manifest.json").exists()
            && let Ok(home) = crate::paths::agent_home()
        {
            {
                let target = home.join("extension");
                let _ = std::fs::remove_dir_all(&target);
                if let Err(err) = copy_dir(&unpacked_extension, &target) {
                    eprintln!("\x1b[33m! could not refresh the browser extension: {err}\x1b[0m");
                }
            }
        }

        let unpacked = find_binary(&staging)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&unpacked, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&unpacked, exe)
            .with_context(|| format!("could not replace {}", exe.display()))?;
        Ok(())
    })();

    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn find_binary(dir: &Path) -> Result<PathBuf> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if let Ok(found) = find_binary(&path) {
                return Ok(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some("ax") {
            return Ok(path);
        }
    }
    bail!("the release archive did not contain an `ax` binary")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"), "must not compare as strings");
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.1", "0.1"));
    }

    #[test]
    fn reads_a_checksum_listing() {
        let listing = "abc123  ax-0.1.0-aarch64-apple-darwin.tar.gz\ndef456  ax-0.1.0-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            expected_digest(listing, "ax-0.1.0-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("def456")
        );
        assert_eq!(expected_digest(listing, "missing.tar.gz"), None);
    }

    #[test]
    fn handles_binary_marked_checksums() {
        // `shasum -a 256 -b` writes the name with a leading asterisk.
        let listing = "aa11  *ax-0.1.0-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            expected_digest(listing, "ax-0.1.0-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("aa11")
        );
    }

    #[test]
    fn digests_match_known_value() {
        // Well-known SHA-256 of the empty input.
        assert_eq!(
            hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
