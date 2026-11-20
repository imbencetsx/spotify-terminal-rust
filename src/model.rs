//! Shared data types: things you can see in a list, and where lists come from.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Kind {
    Track,
    Episode,
    Album,
    Artist,
    Playlist,
    /// The user's "Liked Songs" collection (pseudo item on the home view).
    Liked,
}

impl Kind {
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Track => "song",
            Kind::Episode => "pod",
            Kind::Album => "album",
            Kind::Artist => "artist",
            Kind::Playlist => "list",
            Kind::Liked => "liked",
        }
    }

    pub fn api_type(self) -> &'static str {
        match self {
            Kind::Track | Kind::Episode => "track",
            Kind::Album => "album",
            Kind::Artist => "artist",
            Kind::Playlist | Kind::Liked => "playlist",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::Track | Kind::Episode => "songs",
            Kind::Album => "albums",
            Kind::Artist => "artists",
            Kind::Playlist | Kind::Liked => "playlists",
        }
    }

    pub fn is_container(self) -> bool {
        matches!(self, Kind::Album | Kind::Artist | Kind::Playlist | Kind::Liked)
    }

    pub fn from_entity_type(s: &str) -> Kind {
        match s {
            "episode" => Kind::Episode,
            "album" => Kind::Album,
            "artist" => Kind::Artist,
            "playlist" => Kind::Playlist,
            _ => Kind::Track,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Item {
    pub kind: Kind,
    pub uri: String,
    pub id: String,
    pub name: String,
    /// Artists, owner, ...
    pub sub: String,
    /// Album name, year, track count, ...
    pub extra: String,
    pub duration_ms: Option<u64>,
    pub image: Option<String>,
    /// A small version of the artwork for list thumbnails (falls back to `image`).
    pub thumb: Option<String>,
    /// For tracks: the album they belong to.
    pub album_uri: Option<String>,
}

impl Item {
    pub fn new(kind: Kind, uri: impl Into<String>, name: impl Into<String>) -> Self {
        let uri = uri.into();
        let id = uri.rsplit(':').next().unwrap_or_default().to_string();
        Item {
            kind,
            uri,
            id,
            name: name.into(),
            sub: String::new(),
            extra: String::new(),
            duration_ms: None,
            image: None,
            thumb: None,
            album_uri: None,
        }
    }
}

/// One page of results from the Web API.
#[derive(Default, Debug)]
pub struct Page {
    pub items: Vec<Item>,
    /// Offset of the next page, if there is one.
    pub next: Option<usize>,
    /// The container being shown (album / playlist / artist), with its cover.
    pub header: Option<Item>,
    /// Something worth telling the user about this list.
    pub note: Option<String>,
}

/// Where a list's contents come from (used for paging).
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Home,
    Search { text: String, kinds: Vec<Kind> },
    Album(String),
    Playlist(String),
    Artist(String),
    Liked,
    Queue,
    /// Lyrics of whatever is playing.
    Lyrics,
}

/// Parse `!a foo` style search input into (kinds, text).
pub fn parse_query(q: &str) -> (Vec<Kind>, String) {
    let all = vec![Kind::Track, Kind::Album, Kind::Artist, Kind::Playlist];
    let q = q.trim();
    let Some(rest) = q.strip_prefix('!') else {
        return (all, q.to_string());
    };
    let (tok, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let kind = match tok.to_ascii_lowercase().as_str() {
        "s" | "t" | "song" | "songs" | "track" | "tracks" => Kind::Track,
        "a" | "al" | "album" | "albums" => Kind::Album,
        "p" | "pl" | "playlist" | "playlists" => Kind::Playlist,
        "r" | "ar" | "artist" | "artists" => Kind::Artist,
        _ => return (all, q.to_string()),
    };
    (vec![kind], text.trim().to_string())
}

pub fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        assert_eq!(parse_query("!a dark side"), (vec![Kind::Album], "dark side".into()));
        assert_eq!(parse_query("!p chill"), (vec![Kind::Playlist], "chill".into()));
        assert_eq!(parse_query("!s  hey jude "), (vec![Kind::Track], "hey jude".into()));
        assert_eq!(parse_query("!r"), (vec![Kind::Artist], "".into()));
        assert_eq!(parse_query("radiohead").0.len(), 4);
        assert_eq!(parse_query("!x foo").1, "!x foo");
    }

    #[test]
    fn time() {
        assert_eq!(fmt_ms(0), "0:00");
        assert_eq!(fmt_ms(83_000), "1:23");
        assert_eq!(fmt_ms(3_723_000), "1:02:03");
    }
}
