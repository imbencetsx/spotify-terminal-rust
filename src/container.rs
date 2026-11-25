//! Soloist on Macs that aren't Apple Silicon.
//!
//! Spotify only publishes Soloist for Apple Silicon on macOS, so on any other
//! Mac str installs the Linux x86_64 build and runs it in a container (Docker
//! Desktop, OrbStack, Colima or Podman). Two things don't cross the container
//! boundary on their own, so str handles them:
//!
//! - **Sound.** The Linux build plays through PulseAudio, so the container
//!   talks to a PulseAudio server on the Mac over TCP (`brew install pulseaudio`).
//! - **The Spotify Connect login.** Spotify apps find the device over Bonjour,
//!   which can't leave the container's VM. str pins Soloist's login listener to
//!   a published port range and advertises it from the Mac with `dns-sd`.
//!
//! A Linux executable on macOS is what switches this on, so a hand-configured
//! `[soloist] bin` pointing at a Linux build works too.

use anyhow::{Context, Result, bail};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Name of the container str runs Soloist in.
const NAME: &str = "str-soloist";
/// Runtime image: just the libraries the Linux build loads. Soloist itself is
/// mounted in, so updating it doesn't need a rebuild. Bump the tag when the
/// Dockerfile changes.
const IMAGE: &str = "str-soloist-runtime:1";
const DOCKERFILE: &str = "\
FROM debian:trixie-slim
RUN apt-get update \\
 && apt-get install -y --no-install-recommends libatomic1 libpulse0 ca-certificates \\
 && rm -rf /var/lib/apt/lists/*
";
/// Where str's own PulseAudio listener sits on the Mac (4713 is the usual
/// one, so a setup that already uses it is left alone).
const PULSE_PORT: u16 = 4718;
/// Ports Soloist may pick for its Connect login listener.
const LOGIN_PORTS: u16 = 32;
/// Where to look for those ports: below macOS's own ephemeral range, so the
/// WebSocket port (which the OS picks) never lands inside it.
const LOGIN_PORTS_FROM: u16 = 47130;
const LOGIN_PORTS_TO: u16 = 49151;

/// Set once str has started the container, so it can clean up after it.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Should str install the Linux build and run it in a container? Yes on a Mac
/// that isn't Apple Silicon (whatever str itself was built for), or when
/// `STR_SOLOIST_CONTAINER=1` asks for it.
pub fn wanted() -> bool {
    static WANTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WANTED.get_or_init(|| {
        if let Some(v) = std::env::var_os("STR_SOLOIST_CONTAINER") {
            return v == "1";
        }
        cfg!(target_os = "macos") && !apple_silicon()
    })
}

/// Is this Mac's CPU Apple Silicon? (`hw.optional.arm64` is 1 there even for
/// a process running under Rosetta.)
fn apple_silicon() -> bool {
    if cfg!(target_arch = "aarch64") {
        return true;
    }
    Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.optional.arm64"])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.stdout.trim_ascii() == b"1")
}

/// Does running this soloist need a container? (A Linux build on macOS.)
pub fn needed(bin: &Path) -> bool {
    cfg!(target_os = "macos") && is_linux_exe(bin)
}

fn is_linux_exe(p: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(p)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok_and(|_| magic == *b"\x7fELF")
}

/// Find a program on PATH or in the places container tools and Homebrew
/// install to (a GUI-launched terminal can have a short PATH).
fn find(name: &str) -> Option<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let extra = [
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        home.join(".docker/bin"),
        home.join(".orbstack/bin"),
        PathBuf::from("/Applications/Docker.app/Contents/Resources/bin"),
        PathBuf::from("/opt/podman/bin"),
    ];
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(extra)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// The container CLI: `docker` (Docker Desktop, OrbStack, Colima) or `podman`.
fn cli() -> Result<PathBuf> {
    find("docker").or_else(|| find("podman")).context(
        "Spotify doesn't make Soloist for Intel Macs, so str runs the Linux build in a \
         container — install Docker Desktop, OrbStack, Colima or Podman",
    )
}

fn is_podman(cli: &Path) -> bool {
    cli.file_name().is_some_and(|n| n == "podman")
}

/// Run a short CLI command, turning "the daemon isn't up" into a clear error.
fn output(cli: &Path, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new(cli)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", cli.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
        if err.contains("cannot connect")
            || err.contains("daemon running")
            || err.contains("connection refused")
            || err.contains("unable to connect")
        {
            bail!(
                "{} isn't running — start Docker Desktop, OrbStack, `colima start` or `podman machine start`",
                cli.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
    Ok(out)
}

/// Is the runtime image there? Errors when the container daemon is down.
pub fn image_ready() -> Result<bool> {
    let cli = cli()?;
    Ok(output(&cli, &["image", "inspect", IMAGE])?.status.success())
}

/// Build the runtime image (needs the network the first time).
pub fn build_image() -> Result<()> {
    let cli = cli()?;
    output(&cli, &["version"])?;
    let mut child = Command::new(&cli)
        .args(["build", "--platform", "linux/amd64", "-t", IMAGE, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", cli.display()))?;
    std::io::Write::write_all(child.stdin.as_mut().expect("piped"), DOCKERFILE.as_bytes())?;
    drop(child.stdin.take());
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        bail!("couldn't build the Soloist container image: {why}");
    }
    Ok(())
}

/// Mount for the soloist executable's folder.
fn exe_mount(bin: &Path) -> Result<(String, String)> {
    let dir = bin.parent().context("soloist has no parent folder")?;
    let file = bin.file_name().context("soloist has no file name")?;
    Ok((
        format!("{}:/opt/soloist:ro", dir.display()),
        format!("/opt/soloist/{}", file.to_string_lossy()),
    ))
}

/// `soloist --version`, run in a throwaway container.
pub fn version_command(bin: &Path) -> Result<Command> {
    let (mount, exe) = exe_mount(bin)?;
    let mut cmd = Command::new(cli()?);
    cmd.args(["run", "--rm", "--platform", "linux/amd64", "-v"])
        .arg(mount)
        .arg(IMAGE)
        .args([exe.as_str(), "--version"]);
    Ok(cmd)
}

/// Is str's Soloist container running?
pub fn running() -> bool {
    let Ok(cli) = cli() else { return false };
    let filter = format!("name=^{NAME}$");
    output(&cli, &["ps", "-q", "--filter", &filter])
        .is_ok_and(|o| o.status.success() && !o.stdout.trim_ascii().is_empty())
}

pub fn stop() -> Result<()> {
    let cli = cli()?;
    // Like `kill <pid>`: ask Soloist to quit and return straight away.
    let out = output(&cli, &["kill", "--signal", "TERM", NAME])?;
    if !out.status.success() {
        bail!("soloist doesn't seem to be running");
    }
    Ok(())
}

/// After str gave up waiting for its Soloist to exit, make sure the
/// container doesn't outlive it.
pub async fn cleanup() {
    if !STARTED.load(Ordering::Relaxed) {
        return;
    }
    let _ = tokio::task::spawn_blocking(|| {
        if let Ok(cli) = cli() {
            let _ = output(&cli, &["rm", "-f", NAME]);
        }
    })
    .await;
}

fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Make sure a PulseAudio server on the Mac takes the container's sound, and
/// return the `PULSE_SERVER` the container should use.
fn ensure_audio(cli: &Path) -> Result<String> {
    if let Some(s) = std::env::var("PULSE_SERVER").ok().filter(|s| !s.is_empty()) {
        return Ok(s);
    }
    let host = if is_podman(cli) {
        "host.containers.internal"
    } else {
        "host.docker.internal"
    };
    let server = format!("tcp:{host}:{PULSE_PORT}");
    if port_open(PULSE_PORT) {
        return Ok(server);
    }
    let Some(pulse) = find("pulseaudio") else {
        bail!(
            "Soloist on an Intel Mac plays its sound through PulseAudio — install it with \
             `brew install pulseaudio`"
        );
    };
    let quiet = |c: &mut Command| {
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    if !quiet(Command::new(&pulse).arg("--check")).is_ok_and(|s| s.success()) {
        quiet(Command::new(&pulse).args(["--start", "--exit-idle-time=-1"]))
            .context("starting PulseAudio")?;
    }
    // Only reachable from this Mac; Docker forwards the container's
    // host.docker.internal connections to the Mac's loopback.
    let pactl = pulse.with_file_name("pactl");
    let _ = quiet(Command::new(if pactl.is_file() { pactl.as_path() } else { Path::new("pactl") }).args([
        "load-module",
        "module-native-protocol-tcp",
        &format!("port={PULSE_PORT}"),
        "listen=127.0.0.1",
        "auth-anonymous=1",
    ]));
    for _ in 0..20 {
        if port_open(PULSE_PORT) {
            return Ok(server);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("PulseAudio isn't taking connections on 127.0.0.1:{PULSE_PORT} — try `pulseaudio -k` and start str again")
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port())
}

/// A run of ports nothing on the Mac is using, for the Connect login listener.
fn free_login_ports() -> Result<(u16, u16)> {
    let mut lo = LOGIN_PORTS_FROM;
    while lo + LOGIN_PORTS - 1 <= LOGIN_PORTS_TO {
        let hi = lo + LOGIN_PORTS - 1;
        if (lo..=hi).all(|p| TcpListener::bind((Ipv4Addr::UNSPECIFIED, p)).is_ok()) {
            return Ok((lo, hi));
        }
        lo += LOGIN_PORTS;
    }
    bail!("no free ports for Soloist's Spotify Connect login between {LOGIN_PORTS_FROM} and {LOGIN_PORTS_TO}")
}

/// Runs the container, and once Soloist's Connect login listener answers,
/// advertises it over Bonjour for as long as the container runs (even if
/// this script is killed). Signals go to the container CLI, which passes
/// them on to Soloist. Args: device name, first and last login port, then
/// the container command.
const WRAPPER: &str = r#"
name=$1 lo=$2 hi=$3
shift 3
"$@" &
c=$!
trap 'kill -TERM $c 2>/dev/null' TERM INT HUP
(
  command -v dns-sd >/dev/null 2>&1 || exit 0
  i=0
  while [ $i -lt 120 ] && kill -0 $c 2>/dev/null; do
    p=$lo
    while [ $p -le $hi ]; do
      if curl -sf -m 1 -o /dev/null "http://127.0.0.1:$p/zc?action=getInfo" 2>/dev/null; then
        dns-sd -R "$name" _spotify-connect._tcp local "$p" CPath=/zc VERSION=1.0 Stack=SP >/dev/null 2>&1 &
        d=$!
        while kill -0 $c 2>/dev/null; do sleep 1; done
        kill $d 2>/dev/null
        exit 0
      fi
      p=$((p + 1))
    done
    sleep 1
    i=$((i + 1))
  done
) &
wait $c
s=$?
while kill -0 $c 2>/dev/null; do
  wait $c
  s=$?
done
exit $s
"#;

/// The command that runs `bin` (a Linux soloist) in a container, like
/// `soloist -n <name> -k <key> --ws …`. Soloist writes `ws.addr`/`ws.port`
/// into `data_dir` as usual, so str finds it the same way.
pub fn command(bin: &Path, name: &str, key: &str, data_dir: &Path) -> Result<tokio::process::Command> {
    let cli = cli()?;
    let pulse = ensure_audio(&cli)?;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    // Stale discovery files from a container that didn't exit cleanly.
    let _ = std::fs::remove_file(data_dir.join("ws.addr"));
    let _ = std::fs::remove_file(data_dir.join("ws.port"));
    let ws = free_port()?;
    let (lo, hi) = free_login_ports()?;
    let (mount, exe) = exe_mount(bin)?;

    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(WRAPPER)
        .arg("str-soloist")
        .arg(name)
        .arg(lo.to_string())
        .arg(hi.to_string())
        .arg(&cli)
        .args(["run", "--rm", "--init", "--name", NAME, "--platform", "linux/amd64"])
        // Soloist binds its login listener to port 0, so the kernel picks it
        // from this range — the same ports that are published below.
        .arg("--sysctl")
        .arg(format!("net.ipv4.ip_local_port_range={lo} {hi}"))
        .arg("-p")
        .arg(format!("127.0.0.1:{ws}:{ws}"))
        .arg("-p")
        .arg(format!("{lo}-{hi}:{lo}-{hi}"))
        .arg("-e")
        .arg(format!("PULSE_SERVER={pulse}"))
        .arg("-v")
        .arg(mount)
        .arg("-v")
        .arg(format!("{}:/data", data_dir.display()))
        // The key goes in through the environment so it isn't in `ps`.
        .args(["-e", "STR_SOLOIST_KEY"])
        .arg(IMAGE)
        .args(["/bin/sh", "-c", r#"exec "$0" -k "$STR_SOLOIST_KEY" "$@""#])
        .arg(exe)
        .args(["-n", name, "-D", "/data", "--ws"])
        .arg(format!("0.0.0.0:{ws}"))
        .env("STR_SOLOIST_KEY", key);
    STARTED.store(true, Ordering::Relaxed);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_linux_executables() {
        let dir = std::env::temp_dir().join(format!("str-elf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let elf = dir.join("elf");
        std::fs::write(&elf, b"\x7fELF\x02\x01\x01").unwrap();
        let script = dir.join("script");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();
        assert!(is_linux_exe(&elf));
        assert!(!is_linux_exe(&script));
        assert!(!is_linux_exe(&dir.join("missing")));
        assert_eq!(needed(&elf), cfg!(target_os = "macos"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn login_ports_stay_below_the_ephemeral_range() {
        let (lo, hi) = free_login_ports().unwrap();
        assert_eq!(hi - lo + 1, LOGIN_PORTS);
        assert!(lo >= LOGIN_PORTS_FROM && hi <= LOGIN_PORTS_TO);
    }
}
