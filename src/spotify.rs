//! Spotify Web API client: search + browsing. Playback goes through Soloist.
//!
//! Note the Feb 2026 dev-mode limits: search returns ≤10 per type per request
//! (we page), and playlist items are only visible for playlists you own.

use crate::auth::{self, Token};
use crate::config::Config;
use crate::model::{Item, Kind, Page, Source};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

fn base() -> String {
    // STR_API_BASE lets the mock in examples/ stand in for Spotify during development.
    std::env::var("STR_API_BASE").unwrap_or_else(|_| "https://api.spotify.com/v1".into())
}
const SEARCH_LIMIT: usize = 10;
const PAGE: usize = 50;

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    cfg: Arc<Config>,
    token: Arc<Mutex<Token>>,
}

impl Api {
    pub fn new(http: reqwest::Client, cfg: Arc<Config>, token: Token) -> Self {
        Api { http, cfg, token: Arc::new(Mutex::new(token)) }
    }

    async fn bearer(&self, force: bool) -> Result<String> {
        let mut t = self.token.lock().await;
        if force || t.expired() {
            *t = auth::refresh(&self.cfg, &self.http, &t).await?;
        }
        Ok(t.access_token.clone())
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let mut force = false;
        loop {
            let tok = self.bearer(force).await?;
            let r = self
                .http
                .get(format!("{}{path}", base()))
                .bearer_auth(tok)
                .query(query)
                .send()
                .await
                .context("network error")?;
            let status = r.status();
            if status.as_u16() == 401 && !force {
                force = true;
                continue;
            }
            if status.as_u16() == 429 {
                let wait = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("a few");
                bail!("rate limited by Spotify — try again in {wait}s");
            }
            if !status.is_success() {
                let body: Value = r.json().await.unwrap_or(Value::Null);
                let msg = body["error"]["message"].as_str().unwrap_or("request failed");
                bail!("spotify {}: {msg}", status.as_u16());
            }
            return Ok(r.json().await?);
        }
    }

    pub async fn fetch(&self, src: &Source, offset: usize) -> Result<Page> {
        match src {
            Source::Home => self.home(offset).await,
            Source::Search { text, kinds } => self.search(text, kinds, offset).await,
            Source::Album(id) => self.album(id, offset).await,
            Source::Playlist(id) => self.playlist(id, offset).await,
            Source::Artist(id) => self.artist(id, offset).await,
            Source::Liked => self.liked(offset).await,
            Source::Queue | Source::Lyrics => Ok(Page::default()),
        }
    }

    async fn search(&self, text: &str, kinds: &[Kind], offset: usize) -> Result<Page> {
        // Mixed searches get a few of each; single-type searches page by 10.
        let limit = if kinds.len() == 1 { SEARCH_LIMIT } else { 5 };
        let types = kinds.iter().map(|k| k.api_type()).collect::<Vec<_>>().join(",");
        let v = self
            .get(
                "/search",
                &[
                    ("q", text.to_string()),
                    ("type", types),
                    ("limit", limit.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        let mut page = Page::default();
        let mut more = false;
        for k in kinds {
            let sec = &v[format!("{}s", k.api_type())];
            for it in sec["items"].as_array().into_iter().flatten() {
                let parsed = match k {
                    Kind::Track => track(it, None),
                    Kind::Album => album(it),
                    Kind::Artist => artist(it),
                    Kind::Playlist => playlist(it),
                    _ => None,
                };
                page.items.extend(parsed);
            }
            more |= sec["next"].is_string();
        }
        if kinds.len() == 1 && more {
            page.next = Some(offset + limit);
        }
        Ok(page)
    }

    async fn album(&self, id: &str, offset: usize) -> Result<Page> {
        if offset == 0 {
            let v = self.get(&format!("/albums/{id}"), &[]).await?;
            let header = album(&v).context("bad album")?;
            let mut page = tracks_page(&v["tracks"], Some(&header), 0);
            page.header = Some(header);
            return Ok(page);
        }
        let v = self
            .get(
                &format!("/albums/{id}/tracks"),
                &[("limit", PAGE.to_string()), ("offset", offset.to_string())],
            )
            .await?;
        let stub = Item::new(Kind::Album, format!("spotify:album:{id}"), "");
        Ok(tracks_page(&v, Some(&stub), offset))
    }

    async fn playlist(&self, id: &str, offset: usize) -> Result<Page> {
        if offset == 0 {
            let v = self.get(&format!("/playlists/{id}"), &[]).await?;
            let header = playlist(&v).context("bad playlist")?;
            let items = v.get("items").filter(|x| x.is_object()).or_else(|| v.get("tracks"));
            let mut page = match items {
                Some(p) if p["items"].is_array() => tracks_page(p, None, 0),
                _ => Page {
                    note: Some(
                        "Spotify only lets apps list the songs of playlists you own. Press ⏎ to play it anyway."
                            .into(),
                    ),
                    ..Page::default()
                },
            };
            page.header = Some(header);
            return Ok(page);
        }
        let v = self
            .get(
                &format!("/playlists/{id}/items"),
                &[("limit", PAGE.to_string()), ("offset", offset.to_string())],
            )
            .await?;
        Ok(tracks_page(&v, None, offset))
    }

    async fn artist(&self, id: &str, offset: usize) -> Result<Page> {
        let limit = 10;
        let header = if offset == 0 {
            let v = self.get(&format!("/artists/{id}"), &[]).await?;
            artist(&v)
        } else {
            None
        };
        let v = self
            .get(
                &format!("/artists/{id}/albums"),
                &[
                    ("include_groups", "album,single".to_string()),
                    ("limit", limit.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        let raw = v["items"].as_array().cloned().unwrap_or_default();
        Ok(Page {
            items: raw.iter().filter_map(album).collect(),
            next: v["next"].is_string().then(|| offset + raw.len()),
            header,
            note: None,
        })
    }

    async fn home(&self, offset: usize) -> Result<Page> {
        let v = self
            .get(
                "/me/playlists",
                &[("limit", PAGE.to_string()), ("offset", offset.to_string())],
            )
            .await?;
        let raw = v["items"].as_array().cloned().unwrap_or_default();
        let mut items = Vec::new();
        if offset == 0 {
            let mut liked = Item::new(Kind::Liked, "", "Liked Songs");
            liked.sub = "your library".into();
            if let Ok(t) = self.get("/me/tracks", &[("limit", "1".to_string())]).await
                && let Some(n) = t["total"].as_u64()
            {
                liked.extra = format!("{n} songs");
            }
            items.push(liked);
        }
        items.extend(raw.iter().filter_map(playlist));
        Ok(Page {
            items,
            next: v["next"].is_string().then(|| offset + raw.len()),
            header: None,
            note: None,
        })
    }

    async fn liked(&self, offset: usize) -> Result<Page> {
        let v = self
            .get(
                "/me/tracks",
                &[("limit", PAGE.to_string()), ("offset", offset.to_string())],
            )
            .await?;
        let mut page = tracks_page(&v, None, offset);
        if offset == 0 {
            let mut h = Item::new(Kind::Liked, "", "Liked Songs");
            h.sub = "your library".into();
            if let Some(t) = v["total"].as_u64() {
                h.extra = format!("{t} songs");
            }
            page.header = Some(h);
        }
        Ok(page)
    }
}

// ---------------------------------------------------------------- parsing --

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

fn names(v: &Value) -> String {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["name"].as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The smallest image that's still ≥ 48px — enough for a list thumbnail.
pub fn pick_thumb(imgs: &Value) -> Option<String> {
    let arr = imgs.as_array()?;
    arr.iter()
        .filter_map(|i| Some((i["width"].as_u64().unwrap_or(640), i["url"].as_str()?)))
        .filter(|(w, _)| *w >= 48)
        .min_by_key(|(w, _)| *w)
        .map(|(_, u)| u.to_string())
}

/// Pick an image around 300px — sharp enough for a terminal, cheap to fetch.
pub fn pick_image(imgs: &Value) -> Option<String> {
    let arr = imgs.as_array()?;
    let mut best: Option<(u64, &str)> = None;
    for i in arr {
        let Some(url) = i["url"].as_str() else { continue };
        let w = i["width"].as_u64().unwrap_or(300);
        let score = w.abs_diff(320);
        if best.is_none_or(|(b, _)| score < b) {
            best = Some((score, url));
        }
    }
    best.map(|(_, u)| u.to_string())
}

/// A page of tracks (album tracks, playlist items, saved tracks).
fn tracks_page(p: &Value, album_ctx: Option<&Item>, offset: usize) -> Page {
    let raw = p["items"].as_array().cloned().unwrap_or_default();
    let items = raw
        .iter()
        .filter_map(|e| {
            // playlist/saved entries wrap the track: {item|track: {...}}
            let t = e
                .get("item")
                .filter(|x| x.is_object())
                .or_else(|| e.get("track").filter(|x| x.is_object()))
                .unwrap_or(e);
            track(t, album_ctx)
        })
        .collect();
    Page {
        items,
        next: p["next"].is_string().then(|| offset + raw.len()),
        ..Page::default()
    }
}

fn track(v: &Value, album_ctx: Option<&Item>) -> Option<Item> {
    let uri = v["uri"].as_str().filter(|u| !u.is_empty())?;
    let kind = if v["type"] == "episode" { Kind::Episode } else { Kind::Track };
    let mut it = Item::new(kind, uri, s(v, "name"));
    it.sub = if kind == Kind::Episode { s(&v["show"], "name") } else { names(&v["artists"]) };
    it.duration_ms = v["duration_ms"].as_u64();
    let al = &v["album"];
    if al.is_object() {
        it.extra = s(al, "name");
        it.image = pick_image(&al["images"]);
        it.thumb = pick_thumb(&al["images"]);
        it.album_uri = al["uri"].as_str().map(String::from);
    } else if let Some(a) = album_ctx {
        it.extra = a.name.clone();
        it.image = a.image.clone();
        it.thumb = a.thumb.clone();
        it.album_uri = Some(a.uri.clone());
    }
    if kind == Kind::Episode && it.image.is_none() {
        it.image = pick_image(&v["images"]);
        it.thumb = pick_thumb(&v["images"]);
    }
    Some(it)
}

fn album(v: &Value) -> Option<Item> {
    let uri = v["uri"].as_str().filter(|u| !u.is_empty())?;
    let mut it = Item::new(Kind::Album, uri, s(v, "name"));
    it.sub = names(&v["artists"]);
    let year = s(v, "release_date").chars().take(4).collect::<String>();
    let n = v["total_tracks"].as_u64();
    let typ = s(v, "album_type");
    it.extra = [
        (typ != "album" && !typ.is_empty()).then_some(typ),
        (!year.is_empty()).then_some(year),
        n.map(|n| format!("{n} songs")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    it.image = pick_image(&v["images"]);
    it.thumb = pick_thumb(&v["images"]);
    Some(it)
}

fn artist(v: &Value) -> Option<Item> {
    let uri = v["uri"].as_str().filter(|u| !u.is_empty())?;
    let mut it = Item::new(Kind::Artist, uri, s(v, "name"));
    it.sub = v["genres"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| g.as_str())
        .take(3)
        .collect::<Vec<_>>()
        .join(", ");
    it.extra = "artist".into();
    it.image = pick_image(&v["images"]);
    it.thumb = pick_thumb(&v["images"]);
    Some(it)
}

fn playlist(v: &Value) -> Option<Item> {
    let uri = v["uri"].as_str().filter(|u| !u.is_empty())?;
    let mut it = Item::new(Kind::Playlist, uri, s(v, "name"));
    it.sub = v["owner"]["display_name"]
        .as_str()
        .map(|o| format!("by {o}"))
        .unwrap_or_default();
    let total = v["items"]["total"].as_u64().or(v["tracks"]["total"].as_u64());
    it.extra = total.map(|n| format!("{n} songs")).unwrap_or_default();
    it.image = pick_image(&v["images"]);
    it.thumb = pick_thumb(&v["images"]);
    Some(it)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_playlist_items_new_and_old_shape() {
        let new = json!({"items":[{"item":{"uri":"spotify:track:1","name":"A","artists":[{"name":"X"}],"duration_ms":1000}}],"next":null});
        let old = json!({"items":[{"track":{"uri":"spotify:track:2","name":"B","artists":[{"name":"Y"},{"name":"Z"}]}}],"next":"http://x"});
        let a = tracks_page(&new, None, 0);
        assert_eq!(a.items[0].name, "A");
        assert_eq!(a.next, None);
        let b = tracks_page(&old, None, 50);
        assert_eq!(b.items[0].sub, "Y, Z");
        assert_eq!(b.next, Some(51));
    }

    #[test]
    fn image_pick_prefers_mid_size() {
        let imgs = json!([{"url":"big","width":640},{"url":"mid","width":300},{"url":"small","width":64}]);
        assert_eq!(pick_image(&imgs).as_deref(), Some("mid"));
    }

    #[test]
    fn skips_null_playlists() {
        assert!(playlist(&Value::Null).is_none());
    }
}
