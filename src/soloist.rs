//! Spotify Soloist WebSocket client + player state.

use crate::config::{self, Config};
use crate::model::{Item, Kind, now_ms};
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ------------------------------------------------------------ discovery --

/// Find the WebSocket endpoint of a local Soloist via `ws.addr` / `ws.port`.
pub fn discover(dir: &Path) -> Option<String> {
    let addr = std::fs::read_to_string(dir.join("ws.addr")).ok()?;
    let addr = addr.trim();
    let port_file = std::fs::read_to_string(dir.join("ws.port"))
        .ok()
        .and_then(|p| p.trim().parse::<u16>().ok());
    let (ip, port) = if let Ok(sa) = addr.parse::<SocketAddr>() {
        (sa.ip(), port_file.unwrap_or(sa.port()))
    } else {
        let ip: IpAddr = addr.trim_matches(['[', ']']).parse().ok()?;
        (ip, port_file?)
    };
    if port == 0 {
        return None;
    }
    let ip = match ip {
        IpAddr::V4(v) if v.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v) if v.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    Some(SocketAddr::new(ip, port).to_string())
}

pub fn endpoint(cfg: &Config) -> Option<String> {
    cfg.soloist
        .ws
        .clone()
        .or_else(|| discover(&cfg.soloist_data_dir()))
}

pub enum Spawn {
    /// No API key, or it's already running.
    NotNeeded,
    Started(tokio::process::Child),
    /// We'd start it, but there's no soloist executable yet.
    Missing,
}

pub fn log_path() -> std::path::PathBuf {
    config::cache_dir().join("soloist.log")
}

/// Last meaningful line of the soloist log (for error messages).
pub fn log_tail() -> Option<String> {
    let s = std::fs::read_to_string(log_path()).ok()?;
    s.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

/// Is a soloist (per its pid file) alive right now?
pub fn daemon_running(cfg: &Config) -> bool {
    let Ok(pid) = std::fs::read_to_string(cfg.soloist_data_dir().join("soloist.pid")) else {
        return false;
    };
    let pid = pid.trim();
    if pid.is_empty() {
        return false;
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Start Soloist in the background when the config has an API key.
pub fn maybe_spawn(cfg: &Config) -> Result<Spawn> {
    let Some(key) = cfg.soloist.api_key.as_deref() else {
        return Ok(Spawn::NotNeeded);
    };
    if endpoint(cfg).is_some() || daemon_running(cfg) {
        return Ok(Spawn::NotNeeded);
    }
    let Some(bin) = crate::install::find_bin(cfg) else {
        return Ok(Spawn::Missing);
    };
    let name = cfg.soloist.device_name.as_deref().unwrap_or("str");
    std::fs::create_dir_all(config::cache_dir())?;
    let log = std::fs::File::create(log_path())?;
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(["-n", name, "-k", key, "--ws", "127.0.0.1:0"]);
    if let Some(d) = &cfg.soloist.data_dir {
        cmd.arg("-D").arg(d);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true);
    // The TUI handles Ctrl-C and shuts the child down itself.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    match cmd.spawn() {
        Ok(child) => Ok(Spawn::Started(child)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("`{}` not found — fix [soloist] bin in the config", bin.display())
        }
        Err(e) => Err(e).with_context(|| format!("running {}", bin.display())),
    }
}

/// Stop a Soloist started on this machine (via its pid file).
pub fn stop(cfg: &Config) -> Result<()> {
    let pid = std::fs::read_to_string(cfg.soloist_data_dir().join("soloist.pid"))
        .context("soloist doesn't seem to be running")?;
    let pid = pid.trim();
    #[cfg(unix)]
    let st = std::process::Command::new("kill").arg(pid).status()?;
    #[cfg(windows)]
    let st = std::process::Command::new("taskkill")
        .args(["/PID", pid])
        .status()?;
    if !st.success() {
        bail!("couldn't stop soloist (pid {pid})");
    }
    Ok(())
}

// ------------------------------------------------------------ connection --

pub enum Event {
    Up,
    Down(String),
    Msg(Value),
    ControlError(String),
}

pub enum Request {
    Send(Value),
    Previous,
    PlayCollection { uri: String, shuffle: bool },
}

/// Keep a connection to Soloist alive forever, forwarding events and commands.
pub async fn run(
    cfg: std::sync::Arc<Config>,
    tx: UnboundedSender<Event>,
    mut cmds: UnboundedReceiver<Request>,
) {
    let mut last_down = String::new();
    loop {
        let down = match endpoint(&cfg) {
            None => "soloist not running".to_string(),
            Some(addr) => match connect_async(format!("ws://{addr}")).await {
                Err(e) => format!("can't reach soloist at {addr}: {e}"),
                Ok((mut ws, _)) => {
                    while cmds.try_recv().is_ok() {} // drop stale commands
                    if tx.send(Event::Up).is_err() {
                        return;
                    }
                    last_down.clear();
                    loop {
                        tokio::select! {
                            m = ws.next() => match m {
                                Some(Ok(Message::Text(t))) => {
                                    if let Ok(v) = serde_json::from_str::<Value>(t.as_str())
                                        && tx.send(Event::Msg(v)).is_err() {
                                        return;
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => break "soloist closed the connection".to_string(),
                                Some(Err(e)) => break format!("soloist connection lost: {e}"),
                                _ => {}
                            },
                            c = cmds.recv() => match c {
                                Some(Request::Send(v)) => if let Err(e) = ws.send(Message::text(v.to_string())).await {
                                    break format!("soloist connection lost: {e}");
                                },
                                Some(Request::Previous) => {
                                    let result = previous_on(&mut ws, |v| {
                                        let _ = tx.send(Event::Msg(v.clone()));
                                    }).await;
                                    if let Err(e) = result {
                                        let _ = tx.send(Event::ControlError(format!("{e:#}")));
                                    }
                                }
                                Some(Request::PlayCollection { uri, shuffle }) => {
                                    let result = play_collection(&mut ws, &uri, shuffle, |v| {
                                        let _ = tx.send(Event::Msg(v.clone()));
                                    }).await;
                                    if let Err(e) = result {
                                        let _ = tx.send(Event::ControlError(format!("{e:#}")));
                                    }
                                }
                                None => return,
                            },
                        }
                    }
                }
            },
        };
        if down != last_down {
            if tx.send(Event::Down(down.clone())).is_err() {
                return;
            }
            last_down = down;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

pub fn command(name: &str, extra: Value) -> Value {
    let mut v = json!({ "type": "command", "command": name });
    if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
        o.extend(e);
    }
    v
}

// --------------------------------------------------------------- player --

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Repeat {
    #[default]
    Off,
    Context,
    Track,
}

#[derive(Default, Debug)]
pub struct Player {
    pub connected: bool,
    pub down_reason: String,
    pub logged_in: bool,
    pub active: bool,
    pub device: String,
    pub status: String,
    pub item: Option<Item>,
    pub context: Option<Item>,
    pub pos_ms: u64,
    pub pos_ts: u64,
    pub speed: f64,
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub queue: Vec<Item>,
}

/// Turn a Soloist entity envelope into a list item.
pub fn entity(v: &Value) -> Option<Item> {
    let uri = v["uri"].as_str().filter(|u| !u.is_empty())?;
    let d = &v["decorations"];
    let kind = Kind::from_entity_type(v["entity_type"].as_str().unwrap_or(""));
    let mut it = Item::new(kind, uri, d["identity"]["name"].as_str().unwrap_or(""));
    it.sub = d["creators"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            c["decorations"]["identity"]["name"]
                .as_str()
                .or(c["entity"]["decorations"]["identity"]["name"].as_str())
        })
        .collect::<Vec<_>>()
        .join(", ");
    let parent = if d["parent"]["entity"].is_object() { &d["parent"]["entity"] } else { &d["parent"] };
    it.extra = parent["decorations"]["identity"]["name"]
        .as_str()
        .unwrap_or("")
        .to_string();
    it.album_uri = parent["uri"].as_str().filter(|u| !u.is_empty()).map(String::from);
    it.duration_ms = d["playback"]["duration_ms"].as_u64();
    it.thumb = small_cover(&d["visual_identity"]["cover"])
        .or_else(|| small_cover(&parent["decorations"]["visual_identity"]["cover"]));
    it.image = cover(&d["visual_identity"]["cover"])
        .or_else(|| cover(&parent["decorations"]["visual_identity"]["cover"]));
    Some(it)
}

fn small_cover(v: &Value) -> Option<String> {
    v.as_array()?
        .iter()
        .find(|c| c["size"] == "small")
        .and_then(|c| c["url"].as_str())
        .map(String::from)
}

fn cover(v: &Value) -> Option<String> {
    let arr = v.as_array()?;
    for want in ["large", "default", "xlarge", "small"] {
        if let Some(u) = arr
            .iter()
            .find(|c| c["size"] == want)
            .and_then(|c| c["url"].as_str())
        {
            return Some(u.to_string());
        }
    }
    arr.first()?["url"].as_str().map(String::from)
}

impl Player {
    pub fn playing(&self) -> bool {
        self.status == "playing" || self.status == "buffering"
    }

    pub fn duration(&self) -> u64 {
        self.item.as_ref().and_then(|i| i.duration_ms).unwrap_or(0)
    }

    /// Interpolated position.
    pub fn position(&self) -> u64 {
        let elapsed = now_ms().saturating_sub(self.pos_ts) as f64 * self.speed;
        let p = self.pos_ms + elapsed.max(0.0) as u64;
        match self.duration() {
            0 => p,
            d => p.min(d),
        }
    }

    fn set_position(&mut self, p: &Value) {
        if p.is_object() {
            self.pos_ms = p["position_ms"].as_u64().unwrap_or(0);
            self.pos_ts = p["timestamp_ms"].as_u64().unwrap_or_else(now_ms);
            self.speed = p["speed"].as_f64().unwrap_or(0.0);
        }
    }

    fn set_options(&mut self, o: &Value) {
        if let Some(s) = o["shuffle"].as_bool() {
            self.shuffle = s;
        }
        self.repeat = match o["repeat"].as_str() {
            Some("context") => Repeat::Context,
            Some("track") => Repeat::Track,
            Some("off") => Repeat::Off,
            _ => self.repeat,
        };
    }

    fn set_status(&mut self, s: &str) {
        self.status = s.to_string();
        if !self.playing() {
            // freeze interpolation at the current spot
            self.pos_ms = self.position();
            self.pos_ts = now_ms();
            self.speed = 0.0;
        }
    }

    /// Apply a server event. Returns what kind of thing changed.
    pub fn apply(&mut self, v: &Value) -> Change {
        match v["type"].as_str().unwrap_or("") {
            "auth_state" => {
                let was = self.logged_in;
                self.logged_in = v["logged_in"].as_bool().unwrap_or(false);
                self.active = v["is_active"].as_bool().unwrap_or(false);
                if let Some(d) = v["device_name"].as_str() {
                    self.device = d.to_string();
                }
                if self.logged_in && !was { Change::LoggedIn } else { Change::Other }
            }
            "playback_state" => {
                self.item = entity(&v["item"]);
                self.context = entity(&v["context"]);
                self.set_position(&v["position"]);
                self.set_status(v["status"].as_str().unwrap_or("idle"));
                if let Some(vol) = v["volume"].as_u64() {
                    self.volume = vol.min(100) as u8;
                }
                if let Some(a) = v["is_active"].as_bool() {
                    self.active = a;
                }
                self.set_options(&v["options"]);
                Change::Other
            }
            "track_changed" => {
                let previous_uri = self.item.as_ref().map(|i| i.uri.clone());
                self.item = entity(&v["item"]);
                if self.item.as_ref().map(|i| &i.uri) != previous_uri.as_ref() {
                    self.pos_ms = 0;
                    self.pos_ts = now_ms();
                    self.speed = 0.0;
                }
                Change::Other
            }
            "playback_changed" => {
                self.set_status(v["status"].as_str().unwrap_or("idle"));
                Change::Other
            }
            "volume_changed" => {
                if let Some(vol) = v["volume"].as_u64() {
                    self.volume = vol.min(100) as u8;
                }
                Change::Other
            }
            "device_changed" => {
                self.active = v["is_active"].as_bool().unwrap_or(self.active);
                if let Some(d) = v["device_name"].as_str() {
                    self.device = d.to_string();
                }
                Change::Other
            }
            "context_changed" => {
                self.context = entity(&v["context"]);
                Change::Other
            }
            "options_changed" => {
                self.set_options(&v["options"]);
                Change::Other
            }
            "position_sync" => {
                self.set_position(&v["position"]);
                Change::Other
            }
            "queue_changed" => {
                self.queue = v["upcoming"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| entity(&e["item"]))
                    .collect();
                Change::Queue
            }
            "error" => Change::Error(v["message"].as_str().unwrap_or("soloist error").to_string()),
            _ => Change::Other,
        }
    }
}

pub enum Change {
    LoggedIn,
    Queue,
    Error(String),
    Other,
}

// ------------------------------------------------------- one-shot (CLI) --

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(ws: &mut Ws) -> Result<Value> {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .context("soloist didn't answer")?
            .context("connection closed")??;
        if let Message::Text(t) = m {
            return Ok(serde_json::from_str(t.as_str())?);
        }
    }
}

async fn send(ws: &mut Ws, name: &str, extra: Value) -> Result<()> {
    ws.send(Message::text(command(name, extra).to_string()))
        .await?;
    Ok(())
}

async fn connect(addr: &str) -> Result<(Ws, Value)> {
    let (mut ws, _) = connect_async(format!("ws://{addr}"))
        .await
        .with_context(|| format!("can't reach soloist at {addr}"))?;
    loop {
        let v = next_json(&mut ws).await?;
        if v["type"] == "auth_state" {
            return Ok((ws, v));
        }
    }
}

pub async fn previous(addr: &str) -> Result<()> {
    let (mut ws, auth) = connect(addr).await?;
    if auth["logged_in"] != true {
        bail!("soloist isn't logged in yet");
    }
    previous_on(&mut ws, |_| {}).await
}

async fn play_collection(
    ws: &mut Ws,
    uri: &str,
    shuffle: bool,
    mut observe: impl FnMut(&Value),
) -> Result<()> {
    // A command result confirms dispatch. Confirm the actual option before
    // starting the context so its first track is picked in the requested mode.
    let set_shuffle = async {
        let state = playback_state(ws, &mut observe).await?;
        if state["options"]["shuffle"].as_bool() == Some(shuffle) {
            return Ok(());
        }
        send(ws, "set_shuffle", json!({ "enabled": shuffle })).await?;
        let mut accepted = false;
        let mut enabled = None;
        loop {
            let v = next_json(ws).await?;
            observe(&v);
            match v["type"].as_str() {
                Some("command_result") if v["command"] == "set_shuffle" => {
                    accepted = true;
                    if enabled != Some(shuffle) {
                        send(ws, "get_state", Value::Null).await?;
                    }
                }
                Some("options_changed" | "playback_state") => {
                    enabled = v["options"]["shuffle"].as_bool().or(enabled);
                }
                Some("error") => bail!(
                    "{}",
                    v["message"].as_str().unwrap_or("couldn't set shuffle")
                ),
                _ => {}
            }
            if accepted && enabled == Some(shuffle) {
                return Ok(());
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(3), set_shuffle)
        .await
        .context("soloist didn't confirm the shuffle setting")??;
    send(ws, "play", json!({ "uri": uri })).await
}

async fn playback_state(ws: &mut Ws, observe: &mut impl FnMut(&Value)) -> Result<Value> {
    send(ws, "get_state", Value::Null).await?;
    loop {
        let v = next_json(ws).await?;
        observe(&v);
        match v["type"].as_str() {
            Some("playback_state") => return Ok(v),
            Some("error") => bail!(
                "{}",
                v["message"].as_str().unwrap_or("couldn't get playback state")
            ),
            _ => {}
        }
    }
}

/// Soloist restarts the current track if it's already well underway. Seek to
/// the start and wait for the position update before dispatching Previous.
async fn previous_on(ws: &mut Ws, mut observe: impl FnMut(&Value)) -> Result<()> {
    let run = async {
        let state = playback_state(ws, &mut observe).await?;
        let mut player = Player::default();
        player.apply(&state);
        if player.position() > 1_000 {
            send(ws, "seek", json!({ "position_ms": 0 })).await?;
            loop {
                let v = next_json(ws).await?;
                observe(&v);
                match v["type"].as_str() {
                    Some("position_sync" | "playback_state")
                        if v["position"]["position_ms"]
                            .as_u64()
                            .is_some_and(|ms| ms < 1_000) =>
                    {
                        break;
                    }
                    Some("error") => bail!("{}", v["message"].as_str().unwrap_or("couldn't seek")),
                    Some("track_changed") if v["item"]["uri"] != state["item"]["uri"] => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        send(ws, "skip_prev", Value::Null).await?;
        loop {
            let v = next_json(ws).await?;
            observe(&v);
            match v["type"].as_str() {
                Some("command_result") if v["command"] == "skip_prev" => return Ok(()),
                Some("error") => bail!("{}", v["message"].as_str().unwrap_or("couldn't skip back")),
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .context("timed out skipping back")?
}

pub async fn pause_for_exit(addr: &str) -> Result<()> {
    let (mut ws, auth) = connect(addr).await?;
    if auth["logged_in"] != true || auth["is_active"] != true {
        return Ok(());
    }
    let already_stopped =
        |v: &Value| matches!(v["status"].as_str(), Some("paused" | "idle" | "stopped"));
    // Logged-in connections receive a playback snapshot immediately after auth.
    let initial = next_json(&mut ws).await?;
    if initial["type"] == "playback_state" && already_stopped(&initial) {
        return Ok(());
    }
    send(&mut ws, "pause", Value::Null).await?;
    let mut accepted = false;
    let mut stopped = false;
    while !(accepted && stopped) {
        let v = next_json(&mut ws).await?;
        match v["type"].as_str() {
            Some("command_result") if v["command"] == "pause" => {
                accepted = true;
                if !stopped {
                    send(&mut ws, "get_state", Value::Null).await?;
                }
            }
            Some("playback_state" | "playback_changed") => {
                stopped = already_stopped(&v);
            }
            Some("error") => bail!(
                "{}",
                v["message"].as_str().unwrap_or("couldn't stop playback")
            ),
            _ => {}
        }
    }
    Ok(())
}

pub async fn shutdown_child(mut child: tokio::process::Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if let Ok(status) = tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            status?;
            return Ok(());
        }
    }
    child.kill().await?;
    Ok(())
}

/// Connect, wait for auth, optionally fetch state, send commands, wait for acks.
pub async fn oneshot(addr: &str, cmds: Vec<Value>, want_state: bool) -> Result<Option<Value>> {
    let (mut ws, auth) = connect(addr).await?;
    if auth["logged_in"] != true {
        bail!("soloist isn't logged in yet — select its device once in a Spotify app");
    }
    let mut state = None;
    if want_state {
        ws.send(Message::text(command("get_state", Value::Null).to_string()))
            .await?;
        loop {
            let v = next_json(&mut ws).await?;
            match v["type"].as_str() {
                Some("playback_state") => {
                    state = Some(v);
                    break;
                }
                Some("error") => bail!("{}", v["message"].as_str().unwrap_or("error")),
                _ => {}
            }
        }
    }
    for c in cmds {
        ws.send(Message::text(c.to_string())).await?;
        loop {
            let v = next_json(&mut ws).await?;
            match v["type"].as_str() {
                Some("command_result") => break,
                Some("error") => bail!("{}", v["message"].as_str().unwrap_or("error")),
                _ => {}
            }
        }
    }
    let _ = ws.close(None).await;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entity() {
        let v = json!({
            "uri": "spotify:track:abc", "entity_type": "track",
            "decorations": {
                "identity": {"name": "Song"},
                "visual_identity": {"cover": [{"url": "s", "size": "small"}, {"url": "l", "size": "large"}]},
                "parent": {"entity": {"uri": "spotify:album:x", "decorations": {"identity": {"name": "Album"}}}},
                "creators": [{"uri": "spotify:artist:1", "decorations": {"identity": {"name": "A"}}}],
                "playback": {"duration_ms": 1234}
            }
        });
        let e = entity(&v).unwrap();
        assert_eq!(e.name, "Song");
        assert_eq!(e.sub, "A");
        assert_eq!(e.extra, "Album");
        assert_eq!(e.album_uri.as_deref(), Some("spotify:album:x"));
        assert_eq!(e.image.as_deref(), Some("l"));
        assert_eq!(e.duration_ms, Some(1234));
        assert!(entity(&json!({"uri": ""})).is_none());
    }

    #[test]
    fn command_merges_fields() {
        let c = command("seek", json!({"position_ms": 5}));
        assert_eq!(c, json!({"type":"command","command":"seek","position_ms":5}));
        assert_eq!(command("pause", Value::Null), json!({"type":"command","command":"pause"}));
    }

    #[tokio::test]
    async fn collection_waits_for_the_confirmed_shuffle_setting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let uri = "spotify:playlist:test";
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let read = |m: Message| serde_json::from_str::<Value>(m.to_text().unwrap()).unwrap();
            let message = |v: Value| Message::text(v.to_string());
            for shuffle in [true, false] {
                let old = message(json!({
                    "type": "playback_state", "options": {"shuffle": !shuffle}
                }));
                assert_eq!(read(ws.next().await.unwrap().unwrap())["command"], "get_state");
                ws.send(old.clone()).await.unwrap();
                assert_eq!(
                    read(ws.next().await.unwrap().unwrap()),
                    command("set_shuffle", json!({"enabled": shuffle}))
                );
                ws.send(message(json!({"type": "command_result", "command": "set_shuffle"}))).await.unwrap();
                assert_eq!(read(ws.next().await.unwrap().unwrap())["command"], "get_state");
                ws.send(old).await.unwrap();
                assert!(tokio::time::timeout(Duration::from_millis(30), ws.next()).await.is_err());
                ws.send(message(json!({"type": "options_changed", "options": {"shuffle": shuffle}}))).await.unwrap();
                assert_eq!(
                    read(ws.next().await.unwrap().unwrap()),
                    command("play", json!({"uri": uri}))
                );
            }
        });
        let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        play_collection(&mut ws, uri, true, |_| {}).await.unwrap();
        play_collection(&mut ws, uri, false, |_| {}).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn previous_waits_for_the_seek_to_land() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let read = |m: Message| serde_json::from_str::<Value>(m.to_text().unwrap()).unwrap();
            let message = |v: Value| Message::text(v.to_string());
            ws.send(message(json!({"type": "auth_state", "logged_in": true})))
                .await
                .unwrap();
            assert_eq!(
                read(ws.next().await.unwrap().unwrap())["command"],
                "get_state"
            );
            ws.send(message(json!({
                "type": "playback_state", "status": "playing",
                "position": {"position_ms": 20_000}
            })))
            .await
            .unwrap();
            assert_eq!(
                read(ws.next().await.unwrap().unwrap()),
                command("seek", json!({"position_ms": 0}))
            );
            ws.send(message(
                json!({"type": "command_result", "command": "seek"}),
            ))
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(30), ws.next())
                    .await
                    .is_err()
            );
            ws.send(message(
                json!({"type": "position_sync", "position": {"position_ms": 0}}),
            ))
            .await
            .unwrap();
            assert_eq!(
                read(ws.next().await.unwrap().unwrap())["command"],
                "skip_prev"
            );
            ws.send(message(
                json!({"type": "command_result", "command": "skip_prev"}),
            ))
            .await
            .unwrap();
        });
        previous(&addr.to_string()).await.unwrap();
        server.await.unwrap();
    }

    #[test]
    fn discovery_files() {
        let d = std::env::temp_dir().join(format!("str-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("ws.addr"), "0.0.0.0\n").unwrap();
        std::fs::write(d.join("ws.port"), "41234\n").unwrap();
        assert_eq!(discover(&d).as_deref(), Some("127.0.0.1:41234"));
        std::fs::write(d.join("ws.addr"), "127.0.0.1:0").unwrap();
        assert_eq!(discover(&d).as_deref(), Some("127.0.0.1:41234"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
