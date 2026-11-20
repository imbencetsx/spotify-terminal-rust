//! Song lyrics from LRCLIB (lrclib.net), a free, open lyrics database.
//! Spotify's own APIs don't offer lyrics. Synced (LRC) lyrics carry a
//! timestamp per line, so the current line can follow playback.

use anyhow::{Result, bail};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct LyricLine {
    pub at_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    /// Lines have real timestamps (otherwise they're all 0).
    pub synced: bool,
}

impl Lyrics {
    /// Index of the line being sung at `pos_ms` (None before the first line,
    /// or for unsynced lyrics).
    pub fn current(&self, pos_ms: u64) -> Option<usize> {
        if !self.synced {
            return None;
        }
        // small lead so the highlight lands with the voice, not after it
        let p = pos_ms.saturating_add(150);
        self.lines.partition_point(|l| l.at_ms <= p).checked_sub(1)
    }
}

pub enum State {
    Loading,
    Ready(Lyrics),
    Instrumental,
    Missing,
    Failed(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
    duration: Option<f64>,
}

fn base() -> String {
    // STR_LYRICS_BASE lets the mock in examples/ stand in during development.
    std::env::var("STR_LYRICS_BASE").unwrap_or_else(|_| "https://lrclib.net/api".into())
}

/// Look a song up. `artist` may be a comma-separated list; the first one is used.
pub async fn fetch(http: &reqwest::Client, track: &str, artist: &str, album: &str, dur_ms: u64) -> Result<State> {
    let artist = artist.split(", ").next().unwrap_or(artist);
    let secs = (dur_ms as f64 / 1000.0).round() as u64;
    let mut q: Vec<(&str, String)> = vec![("track_name", track.into()), ("artist_name", artist.into())];
    if !album.is_empty() {
        q.push(("album_name", album.into()));
    }
    if secs > 0 {
        q.push(("duration", secs.to_string()));
    }
    let resp = http.get(format!("{}/get", base())).query(&q).timeout(Duration::from_secs(15)).send().await?;
    match resp.status().as_u16() {
        200 => {
            let r: Record = resp.json().await?;
            if let Some(s) = to_state(&r) {
                return Ok(s);
            }
        }
        404 => {}
        429 => bail!("lyrics: too many requests, try again in a bit"),
        s => bail!("lyrics: HTTP {s}"),
    }
    // no exact match (album or length differ a little): search, keep the
    // closest length within 3s, preferring synced lyrics
    let resp = http
        .get(format!("{}/search", base()))
        .query(&[("track_name", track), ("artist_name", artist)])
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    match resp.status().as_u16() {
        200 => {}
        404 => return Ok(State::Missing),
        429 => bail!("lyrics: too many requests, try again in a bit"),
        s => bail!("lyrics: HTTP {s}"),
    }
    let mut found: Vec<Record> = resp.json().await?;
    found.retain(|r| secs == 0 || r.duration.is_some_and(|d| (d - secs as f64).abs() <= 3.0));
    found.sort_by_key(|r| (r.synced_lyrics.is_none(), r.duration.map(|d| ((d - secs as f64).abs() * 10.0) as u64)));
    Ok(found.iter().find_map(to_state).unwrap_or(State::Missing))
}

fn to_state(r: &Record) -> Option<State> {
    if r.instrumental {
        return Some(State::Instrumental);
    }
    if let Some(s) = r.synced_lyrics.as_deref() {
        let lines = parse_lrc(s);
        if !lines.is_empty() {
            return Some(State::Ready(Lyrics { lines, synced: true }));
        }
    }
    let plain = r.plain_lyrics.as_deref().filter(|p| !p.trim().is_empty())?;
    let lines = plain.lines().map(|l| LyricLine { at_ms: 0, text: l.trim().to_string() }).collect();
    Some(State::Ready(Lyrics { lines, synced: false }))
}

/// Parse LRC: `[mm:ss.xx]text`, possibly several stamps per line.
pub fn parse_lrc(s: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for raw in s.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(r) = rest.strip_prefix('[') {
            let Some((tag, after)) = r.split_once(']') else { break };
            // anything that isn't a time ([ar:…] metadata) ends the stamps
            let Some(ms) = stamp(tag) else { break };
            stamps.push(ms);
            rest = after;
        }
        let text = rest.trim().to_string();
        out.extend(stamps.into_iter().map(|at_ms| LyricLine { at_ms, text: text.clone() }));
    }
    out.sort_by_key(|l| l.at_ms);
    // drop leading blank lines (an empty "[00:00.00]" is common)
    let first = out.iter().position(|l| !l.text.is_empty()).unwrap_or(out.len());
    out.drain(..first);
    out
}

fn stamp(tag: &str) -> Option<u64> {
    let (m, s) = tag.split_once(':')?;
    let m: u64 = m.trim().parse().ok()?;
    let s: f64 = s.trim().parse().ok()?;
    if !s.is_finite() || !(0.0..60.0).contains(&s) {
        return None;
    }
    m.checked_mul(60_000)?.checked_add((s * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc() {
        let l = parse_lrc("[ar:Someone]\n[00:00.00]\n[00:12.50]Hello\n[00:15.00][01:02.25]Again\n[00:20.10] \n");
        assert_eq!(
            l,
            vec![
                LyricLine { at_ms: 12_500, text: "Hello".into() },
                LyricLine { at_ms: 15_000, text: "Again".into() },
                LyricLine { at_ms: 20_100, text: "".into() },
                LyricLine { at_ms: 62_250, text: "Again".into() },
            ]
        );
    }

    #[test]
    fn follows_position() {
        let ly = Lyrics { lines: parse_lrc("[00:10.00]a\n[00:20.00]b\n[00:30.00]c"), synced: true };
        assert_eq!(ly.current(0), None);
        assert_eq!(ly.current(9_900), Some(0));
        assert_eq!(ly.current(25_000), Some(1));
        assert_eq!(ly.current(99_000), Some(2));
    }
}
