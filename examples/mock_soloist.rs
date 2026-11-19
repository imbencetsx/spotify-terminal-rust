//! A fake Spotify Soloist for developing str without Spotify.
//!
//!   cargo run --example mock_soloist -- /tmp/fake-soloist
//!   SOLOIST_WS=… or: str with `[soloist] data_dir = "/tmp/fake-soloist"`
//!
//! Speaks the WebSocket protocol (auth_state, playback_state, granular events),
//! and serves a generated cover image over HTTP.

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

struct State {
    track: usize,
    playing: bool,
    pos: u64,
    ts: u64,
    volume: u64,
    shuffle: bool,
    repeat: &'static str,
    queue: Vec<String>,
    cover_base: String,
}

const TRACKS: &[(&str, &str, &str, u64)] = &[
    ("Midnight Tram", "Nova Lane", "City Lights", 214_000),
    ("Paper Planets", "The Quiet Hours", "Orbit", 187_000),
    ("Salt & Static", "Nova Lane", "City Lights", 241_000),
];

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn entity(s: &State, i: usize) -> Value {
    let (name, artist, album, dur) = TRACKS[i % TRACKS.len()];
    json!({
        "uri": format!("spotify:track:mock{i}"), "entity_type": "track",
        "decorations": {
            "identity": {"name": name},
            "visual_identity": {"cover": [{"url": format!("{}/cover{}.png", s.cover_base, i % TRACKS.len()), "size": "large"}]},
            "parent": {"entity": {"uri": "spotify:album:mockalbum", "entity_type": "album", "decorations": {"identity": {"name": album}}}},
            "creators": [{"uri": "spotify:artist:a", "entity_type": "artist", "decorations": {"identity": {"name": artist}}}],
            "playback": {"duration_ms": dur}
        }
    })
}

fn position(s: &State) -> Value {
    json!({"position_ms": s.pos, "timestamp_ms": s.ts, "speed": if s.playing {1.0} else {0.0}})
}

fn full(s: &State) -> Value {
    json!({
        "type": "playback_state",
        "status": if s.playing {"playing"} else {"paused"},
        "item": entity(s, s.track),
        "context": {"uri": "spotify:album:mockalbum", "entity_type": "album", "decorations": {"identity": {"name": "City Lights"}}},
        "position": position(s),
        "volume": s.volume, "is_active": true,
        "options": {"shuffle": s.shuffle, "repeat": s.repeat, "playback_speed": 1.0},
        "available_actions": {}
    })
}

fn freeze(s: &mut State) {
    if s.playing {
        s.pos += now() - s.ts;
    }
    s.ts = now();
}

fn cover(i: u32) -> Vec<u8> {
    // MOCK_COVER=/path/to.png serves a real picture instead of the generated ones
    if let Some(b) = std::env::var_os("MOCK_COVER").and_then(|p| std::fs::read(p).ok()) {
        return b;
    }
    let palettes = [[(255, 94, 98), (82, 40, 160)], [(30, 215, 96), (10, 40, 90)], [(255, 196, 92), (200, 40, 90)]];
    let [a, b] = palettes[i as usize % 3];
    let img = image::RgbImage::from_fn(300, 300, |x, y| {
        let t = (x + y) as f32 / 600.0;
        let ring = ((x as f32 - 150.0).hypot(y as f32 - 150.0) / 18.0) as u32;
        let k = if ring.is_multiple_of(2) { 1.0 } else { 0.6 };
        let mix = |p: u8, q: u8| ((p as f32 * (1.0 - t) + q as f32 * t) * k) as u8;
        image::Rgb([mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2)])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// Just enough of the Spotify Web API to drive the UI.
fn web_api(base: &str, path: &str) -> Value {
    let img = |i: usize| json!([{"url": format!("{base}/cover{}.png", i % 3), "width": 300}]);
    let artist = |i: usize| json!({"uri": format!("spotify:artist:ar{i}"), "name": (["Nova Lane", "The Quiet Hours", "Static Bloom"][i % 3]), "images": img(i), "genres": ["dream pop", "indie"]});
    let album = |i: usize| json!({"uri": format!("spotify:album:al{i}"), "name": (["City Lights", "Orbit", "Low Tide", "Glass Houses"][i % 4]), "artists": [artist(i)], "images": img(i), "release_date": format!("20{:02}-01-01", 10 + i), "total_tracks": 9 + i, "album_type": (if i % 3 == 2 {"single"} else {"album"})});
    let track = |i: usize| json!({"uri": format!("spotify:track:mock{i}"), "type": "track", "name": (["Midnight Tram", "Paper Planets", "Salt & Static", "Northern Wires", "Slow Machines", "Velvet Hour"][i % 6]), "artists": [artist(i)], "album": album(i % 4), "duration_ms": 150_000 + i as u64 * 7_919});
    let playlist = |i: usize| json!({"uri": format!("spotify:playlist:pl{i}"), "name": (["Late Night Drive", "Focus Flow", "Sunday Slow", "Indie Mix"][i % 4]), "owner": {"display_name": "you"}, "images": img(i), "items": {"total": 20 + i}});
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let offset: usize = query.split('&').find_map(|kv| kv.strip_prefix("offset=")).and_then(|v| v.parse().ok()).unwrap_or(0);
    let page = |items: Vec<Value>, more: bool| json!({"items": items, "next": (if more { json!("more") } else { Value::Null }), "total": 120});
    match route.trim_start_matches("/v1") {
        "/me/playlists" => page((0..4).map(playlist).collect(), false),
        "/me/tracks" => page((offset..offset + 50).map(|i| json!({"track": track(i)})).collect(), offset < 100),
        "/search" => {
            let types = query.split('&').find_map(|kv| kv.strip_prefix("type=")).unwrap_or("track").replace("%2C", ",");
            let single = !types.contains(',');
            let n = if single { 10 } else { 5 };
            let mut out = json!({});
            for t in types.split(',') {
                let items: Vec<Value> = (offset..offset + n).map(|i| match t { "album" => album(i), "artist" => artist(i), "playlist" => playlist(i), _ => track(i) }).collect();
                out[format!("{t}s")] = page(items, offset < 40);
            }
            out
        }
        r if r.starts_with("/albums/") && r.ends_with("/tracks") => page((offset..offset + 5).map(track).collect(), false),
        r if r.starts_with("/albums/") => {
            let mut a = album(0);
            a["tracks"] = page((0..9).map(|i| { let mut t = track(i); t.as_object_mut().unwrap().remove("album"); t }).collect(), false);
            a
        }
        r if r.starts_with("/artists/") && r.ends_with("/albums") => page((offset..offset + 10).map(album).collect(), offset < 10),
        r if r.starts_with("/artists/") => artist(0),
        r if r.starts_with("/playlists/") => {
            // owned playlists list their songs (Feb 2026 shape: items.items[].item)
            let n: usize = r.trim_start_matches("/playlists/pl").split('/').next().and_then(|d| d.parse().ok()).unwrap_or(0);
            let mut p = playlist(n);
            let songs: Vec<Value> = (n * 3..n * 3 + 12 + n * 4).map(|i| json!({"item": track(i)})).collect();
            p["items"] = page(songs, false);
            if r.ends_with("/items") { p["items"].clone() } else { p }
        }
        _ => json!({"error": {"status": 404, "message": "mock: no route"}}),
    }
}

#[tokio::main]
async fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or("/tmp/fake-soloist".into()));
    std::fs::create_dir_all(&dir).unwrap();

    // cover image server
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cover_base = format!("http://{}", http.local_addr().unwrap());
    let base = cover_base.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = http.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let n = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (ctype, body) = if let Some(rest) = path.strip_prefix("/cover") {
                ("image/png", cover(rest[..1].parse().unwrap_or(0)))
            } else {
                ("application/json", web_api(&base, &path).to_string().into_bytes())
            };
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await;
            let _ = s.write_all(&body).await;
        }
    });
    eprintln!("mock web api: STR_API_BASE={cover_base}/v1");

    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ws.local_addr().unwrap().port();
    std::fs::write(dir.join("ws.addr"), "127.0.0.1").unwrap();
    std::fs::write(dir.join("ws.port"), port.to_string()).unwrap();
    eprintln!("mock soloist on ws://127.0.0.1:{port} (data dir {})", dir.display());

    let st = Arc::new(Mutex::new(State {
        track: 0, playing: true, pos: 61_000, ts: now(), volume: 65, shuffle: false, repeat: "off",
        queue: vec![], cover_base,
    }));
    let (bus, _) = broadcast::channel::<String>(64);

    loop {
        let (sock, _) = ws.accept().await.unwrap();
        let st = st.clone();
        let bus = bus.clone();
        tokio::spawn(async move {
            let Ok(ws) = tokio_tungstenite::accept_async(sock).await else { return };
            let (mut w, mut r) = ws.split();
            let mut rx = bus.subscribe();
            let hello = [
                json!({"type": "auth_state", "logged_in": true, "is_active": true, "device_name": "mock speaker"}),
                full(&st.lock().unwrap()),
            ];
            for h in hello {
                let _ = w.send(Message::text(h.to_string())).await;
            }
            loop {
                tokio::select! {
                    m = r.next() => {
                        let Some(Ok(Message::Text(t))) = m else { if m.is_none() { return } else { continue } };
                        let v: Value = serde_json::from_str(t.as_str()).unwrap_or_default();
                        let cmd = v["command"].as_str().unwrap_or("").to_string();
                        let mut out = vec![];
                        let mut broadcast = vec![];
                        {
                            let mut s = st.lock().unwrap();
                            match cmd.as_str() {
                                "get_state" => out.push(full(&s)),
                                "get_auth_state" => out.push(json!({"type":"auth_state","logged_in":true,"is_active":true,"device_name":"mock speaker"})),
                                "get_queue" => out.push(json!({"type":"queue_changed","previous":[],"upcoming": s.queue.iter().enumerate().map(|(i, _)| json!({"uid": i.to_string(), "source":"queue", "item": entity(&s, i + 1)})).collect::<Vec<_>>()})),
                                "play" => {
                                    freeze(&mut s);
                                    if v["uri"].is_string() { s.track += 1; s.pos = 0; broadcast.push(json!({"type":"track_changed","item": entity(&s, s.track)})); }
                                    s.playing = true;
                                    broadcast.push(json!({"type":"playback_changed","status":"playing"}));
                                    broadcast.push(json!({"type":"position_sync","position": position(&s)}));
                                }
                                "pause" => {
                                    freeze(&mut s);
                                    s.playing = false;
                                    broadcast.push(json!({"type":"playback_changed","status":"paused"}));
                                    broadcast.push(json!({"type":"position_sync","position": position(&s)}));
                                }
                                "skip_next" | "skip_prev" => {
                                    s.track = if cmd == "skip_next" { s.track + 1 } else { s.track + TRACKS.len() - 1 };
                                    s.pos = 0; s.ts = now();
                                    broadcast.push(json!({"type":"track_changed","item": entity(&s, s.track)}));
                                    broadcast.push(json!({"type":"position_sync","position": position(&s)}));
                                }
                                "seek" => { s.pos = v["position_ms"].as_u64().unwrap_or(0); s.ts = now(); broadcast.push(json!({"type":"position_sync","position": position(&s)})); }
                                "set_volume" => { s.volume = v["volume"].as_u64().unwrap_or(50); broadcast.push(json!({"type":"volume_changed","volume": s.volume})); }
                                "set_shuffle" => { s.shuffle = v["enabled"].as_bool().unwrap_or(false); broadcast.push(json!({"type":"options_changed","options":{"shuffle": s.shuffle, "repeat": s.repeat}})); }
                                "set_repeat_context" | "set_repeat_track" => {
                                    let on = v["enabled"].as_bool().unwrap_or(false);
                                    s.repeat = match (cmd.as_str(), on) { ("set_repeat_track", true) => "track", ("set_repeat_context", true) => "context", (_, false) if (s.repeat == "track") == (cmd == "set_repeat_track") => "off", _ => s.repeat };
                                    broadcast.push(json!({"type":"options_changed","options":{"shuffle": s.shuffle, "repeat": s.repeat}}));
                                }
                                "add_to_queue" => { s.queue.push(v["uri"].as_str().unwrap_or("").into()); }
                                "activate" | "deactivate" => {}
                                _ => out.push(json!({"type":"error","message": format!("unknown command: {cmd}")})),
                            }
                            if !cmd.starts_with("get_") && !out.iter().any(|o| o["type"] == "error") {
                                out.push(json!({"type":"command_result","command": cmd}));
                            }
                        }
                        for o in out { let _ = w.send(Message::text(o.to_string())).await; }
                        for b in broadcast { let _ = bus.send(b.to_string()); }
                    }
                    b = rx.recv() => if let Ok(b) = b { let _ = w.send(Message::text(b)).await; },
                }
            }
        });
    }
}
