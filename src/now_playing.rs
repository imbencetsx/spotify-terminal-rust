//! macOS media metadata and commands. The native helper owns the system session,
//! while all playback decisions stay in the TUI.

use crate::{
    app::Msg,
    config,
    soloist::{Player, Repeat},
};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc::UnboundedSender, watch},
    task::JoinHandle,
};

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/str-now-playing"));
const PLIST: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>io.str-player.now-playing</string>
<key>CFBundleName</key><string>str</string>
<key>CFBundleExecutable</key><string>str-now-playing</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSUIElement</key><true/>
</dict></plist>"#;

#[derive(Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum Action {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Seek(f64),
    Shuffle(bool),
    Repeat(String),
}

pub struct Bridge {
    child: Child,
    state: watch::Sender<Value>,
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
}

impl Bridge {
    pub fn start(tx: UnboundedSender<Msg>) -> Result<Self> {
        let contents = config::cache_dir().join("str Now Playing.app/Contents");
        let bin = contents.join("MacOS/str-now-playing");
        std::fs::create_dir_all(contents.join("MacOS"))?;
        write_if_new(&contents.join("Info.plist"), PLIST, 0o644)?;
        write_if_new(&bin, HELPER, 0o755)?;

        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(config::cache_dir().join("now-playing.log"))?;
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .context("starting the macOS media helper")?;
        let mut stdin = child.stdin.take().context("missing media helper stdin")?;
        let stdout = child.stdout.take().context("missing media helper stdout")?;
        let (state, mut states) = watch::channel(json!({}));
        let writer = tokio::spawn(async move {
            while states.changed().await.is_ok() {
                let line = states.borrow_and_update().to_string() + "\n";
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(action) = serde_json::from_str(&line)
                    && tx.send(Msg::Media(action)).is_err()
                {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            state,
            writer,
            reader,
        })
    }

    pub fn sync(&self, player: &Player, shuffle: bool) {
        let item = player.item.as_ref().filter(|_| {
            player.connected
                && player.logged_in
                && player.active
                && matches!(player.status.as_str(), "playing" | "buffering" | "paused")
        });
        let state = item.map_or_else(
            || json!({}),
            |item| {
                json!({
                    "title": item.name,
                    "artist": item.sub,
                    "album": item.extra,
                    "cover": item.image,
                    "duration": player.duration() as f64 / 1000.0,
                    "position": player.pos_ms as f64 / 1000.0,
                    "timestamp": player.pos_ts,
                    "rate": if player.playing() { player.speed } else { 0.0 },
                    "playing": player.playing(),
                    "shuffle": shuffle,
                    "repeat": match player.repeat {
                        Repeat::Off => "off",
                        Repeat::Context => "context",
                        Repeat::Track => "track",
                    },
                })
            },
        );
        self.state.send_if_modified(|old| {
            if *old == state {
                return false;
            }
            *old = state;
            true
        });
    }

    pub async fn stop(self) {
        let Self {
            mut child,
            state,
            mut writer,
            reader,
        } = self;
        drop(state);
        if tokio::time::timeout(Duration::from_secs(1), &mut writer)
            .await
            .is_err()
        {
            writer.abort();
            let _ = writer.await;
        }
        if !matches!(
            tokio::time::timeout(Duration::from_secs(1), child.wait()).await,
            Ok(Ok(_))
        ) {
            let _ = child.kill().await;
        }
        let _ = reader.await;
    }
}

fn write_if_new(path: &std::path::Path, bytes: &[u8], mode: u32) -> Result<()> {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, bytes)?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(temp, path)?;
    Ok(())
}
