//! Download and install Spotify Soloist from Spotify's CDN into str's own
//! data folder. Builds expire 90 days after they're built, so we also keep
//! track of that and update when needed.
//!
//! The archive comes straight from Spotify to this machine (nothing is
//! re-hosted), per the Soloist "don't redistribute" terms.

use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CDN: &str = "https://soloist-builds.spotifycdn.com";
const LIFETIME_SECS: u64 = 90 * 24 * 3600;
/// Update a str-managed install when it has fewer days left than this.
pub const UPDATE_WITHIN_DAYS: i64 = 14;

#[cfg(windows)]
pub const EXE: &str = "soloist.exe";
#[cfg(not(windows))]
pub const EXE: &str = "soloist";

/// Archive name for this platform, if Spotify publishes one.
pub fn asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("soloist_release_osx_arm64.tar.gz"),
        ("linux", "x86_64") => Some("soloist_release_x86_64.tar.gz"),
        ("linux", "aarch64") => Some("soloist_release_arm64.tar.gz"),
        ("linux", "arm") => Some("soloist_release_arm32.tar.gz"),
        ("windows", "x86_64") => Some("soloist_release_windows_x86_64.zip"),
        _ => None,
    }
}

/// str's data folder (`$XDG_DATA_HOME/str`, or the platform's local data dir).
pub fn root() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("str")
}

pub fn managed_dir() -> PathBuf {
    root().join("soloist")
}

pub fn managed_bin() -> PathBuf {
    managed_dir().join(EXE)
}

pub fn is_managed(p: &Path) -> bool {
    p == managed_bin()
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Which soloist to run: the configured one, one on PATH, or ours.
pub fn find_bin(cfg: &Config) -> Option<PathBuf> {
    if let Some(b) = &cfg.soloist.bin {
        return Some(b.clone());
    }
    on_path(EXE).or_else(|| Some(managed_bin()).filter(|p| p.is_file()))
}

#[derive(Debug, Clone)]
pub struct Version {
    pub text: String,
    /// Unix seconds the build was made.
    pub build: Option<u64>,
}

impl Version {
    pub fn parse(out: &str) -> Version {
        let text = out.lines().next().unwrap_or("").trim().to_string();
        // "soloist 1.3.10.85 build 1795068072 (20261119) (g…) (macos/aarch64)"
        let build = text
            .split_whitespace()
            .skip_while(|w| *w != "build")
            .nth(1)
            .and_then(|n| n.parse().ok());
        Version { text, build }
    }

    pub fn number(&self) -> &str {
        self.text.split_whitespace().nth(1).unwrap_or("?")
    }

    /// Days until this build stops working (negative = expired).
    pub fn days_left(&self) -> Option<i64> {
        let now = crate::model::now_ms() / 1000;
        self.build
            .map(|b| (b as i64 + LIFETIME_SECS as i64 - now as i64) / 86_400)
    }
}

pub fn version(bin: &Path) -> Option<Version> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| Version::parse(&String::from_utf8_lossy(&out.stdout)))
}

/// Download the latest build for this platform and install it as
/// `managed_bin()`. `progress` gets 0–100.
pub async fn install(http: &reqwest::Client, progress: impl Fn(u8)) -> Result<Version> {
    let Some(name) = asset() else {
        bail!(
            "Spotify doesn't publish Soloist for {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    let root = root();
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

    // download
    let archive = root.join(name);
    let mut resp = http
        .get(format!("{CDN}/{name}"))
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .context("couldn't reach Spotify's download server")?;
    if !resp.status().is_success() {
        bail!("download failed: HTTP {}", resp.status());
    }
    let total = resp.content_length();
    let mut file = std::fs::File::create(&archive)?;
    let (mut got, mut last) = (0u64, u8::MAX);
    while let Some(chunk) = resp.chunk().await.context("download interrupted")? {
        std::io::Write::write_all(&mut file, &chunk)?;
        got += chunk.len() as u64;
        if let Some(t) = total.filter(|t| *t > 0) {
            let pct = (got * 100 / t).min(100) as u8;
            if pct != last {
                progress(pct);
                last = pct;
            }
        }
    }
    drop(file);

    // extract into a staging folder, check it runs, then swap it in
    let staging = root.join("soloist.new");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let (a, s) = (archive.clone(), staging.clone());
    let status = tokio::task::spawn_blocking(move || {
        // bsdtar/GNU tar on macOS & Linux; Windows 10+ ships bsdtar, which also reads zip
        std::process::Command::new("tar")
            .arg("-xf")
            .arg(&a)
            .arg("-C")
            .arg(&s)
            .status()
    })
    .await?
    .context("couldn't run `tar` to unpack the download")?;
    let _ = std::fs::remove_file(&archive);
    if !status.success() {
        bail!("unpacking the download failed");
    }
    let exe = staging.join(EXE);
    if !exe.is_file() {
        bail!("the download didn't contain {EXE}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
    }
    let exe2 = exe.clone();
    let ver = tokio::task::spawn_blocking(move || version(&exe2))
        .await?
        .context("the downloaded soloist doesn't run on this machine")?;

    let dir = managed_dir();
    let old = root.join("soloist.old");
    let _ = std::fs::remove_dir_all(&old);
    if dir.exists() {
        std::fs::rename(&dir, &old)
            .context("couldn't replace the old soloist (is it still running?)")?;
    }
    std::fs::rename(&staging, &dir)?;
    let _ = std::fs::remove_dir_all(&old);
    Ok(ver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_line() {
        let v = Version::parse("soloist 1.3.10.85 build 1795068072 (20261119) (g19412df6ea) (macos/aarch64)\n");
        assert_eq!(v.number(), "1.3.10.85");
        assert_eq!(v.build, Some(1_795_068_072));
        let v = Version::parse("garbage");
        assert_eq!(v.build, None);
        assert_eq!(v.days_left(), None);
    }

    #[test]
    fn this_platform_has_an_asset() {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(asset(), Some("soloist_release_osx_arm64.tar.gz"));
        }
    }
}
