use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// Spotify app client id (developer.spotify.com/dashboard) — used for search & library.
    pub client_id: Option<String>,
    /// Port for the OAuth callback (`http://127.0.0.1:<port>/callback`).
    pub redirect_port: Option<u16>,
    /// Album art look: pixel | dots | image
    pub art: Option<String>,
    /// Player background: spin | still | off
    pub background: Option<String>,
    /// For `art = "image"`: auto | kitty | iterm | sixel | halfblocks | query
    pub images: Option<String>,
    /// Look up lyrics on lrclib.net (sends the song's title/artist/album/length).
    pub lyrics: Option<bool>,
    /// Publish playback state and controls to macOS Now Playing.
    pub now_playing: Option<bool>,
    pub soloist: SoloistCfg,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct SoloistCfg {
    /// Explicit WebSocket endpoint, e.g. "127.0.0.1:9090". Auto-discovered when unset.
    pub ws: Option<String>,
    /// Soloist data directory (for discovery). Platform default when unset.
    pub data_dir: Option<PathBuf>,
    /// Auto-start: path to the soloist executable (default: `soloist` on PATH).
    pub bin: Option<PathBuf>,
    /// Auto-start: your Soloist API key. When set, str starts Soloist if it isn't running.
    pub api_key: Option<String>,
    /// Auto-start: Spotify Connect device name.
    pub device_name: Option<String>,
}

pub const TEMPLATE: &str = r#"# str — config
#
# 1. Spotify Web API (search + your library)
#    Create an app at https://developer.spotify.com/dashboard,
#    add the redirect URI  http://127.0.0.1:8898/callback
#    and paste its client id here. Then run `str login` (or just `str`).
# client_id = "your-client-id"
# redirect_port = 8898

# Album art: pixel (default) | dots | image   (A cycles it in the app)
# art = "pixel"
# Player background, a blurred copy of the cover: spin (default) | still | off
# background = "spin"
# For art = "image": auto | kitty | iterm | sixel | halfblocks | query
# images = "auto"
# Lyrics (L) come from lrclib.net, which gets the playing song's title,
# artist, album and length. Turn off with:
# lyrics = false
# Use macOS Control Center and media keys:
# now_playing = true

[soloist]
# 2. Playback goes through Spotify Soloist's WebSocket API.
#    str finds a running `soloist --ws 127.0.0.1:0` automatically.
#    Or point it somewhere explicitly:
# ws = "127.0.0.1:9090"
# data_dir = "/path/to/soloist/data"
#
#    Let str start Soloist for you when it isn't running:
# api_key = "your-soloist-api-key"
# bin = "/path/to/soloist"
# device_name = "str"
"#;

pub fn dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("str")
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

/// The config path for showing to humans (`~/…`).
pub fn display_path() -> String {
    let p = path();
    match dirs::home_dir().and_then(|h| p.strip_prefix(h).ok().map(|r| r.to_path_buf())) {
        Some(rel) => format!("~/{}", rel.display()),
        None => p.display().to_string(),
    }
}

/// Write values into config.toml, keeping comments and everything else intact.
/// `section` None = top level. A `None` value comments the key out.
pub fn save_values(values: &[(Option<&str>, &str, Option<&str>)]) -> Result<()> {
    let p = path();
    let mut text = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_string(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
    };
    for (section, key, val) in values {
        text = set_in(&text, *section, key, *val);
    }
    // make sure we didn't produce something unreadable
    toml::from_str::<Config>(&text).context("refusing to write an invalid config")?;
    std::fs::create_dir_all(dir())?;
    std::fs::write(&p, text).with_context(|| format!("writing {}", p.display()))?;
    #[cfg(unix)]
    {
        // it holds an API key
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Line-based TOML edit: replace `key = …` (or its commented-out template line)
/// inside `section`, or append it there.
fn set_in(text: &str, section: Option<&str>, key: &str, val: Option<&str>) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let header = |l: &str| {
        let t = l.trim();
        (t.starts_with('[') && t.ends_with(']')).then(|| t[1..t.len() - 1].trim().to_string())
    };
    // [start, end) of the section's body
    let (start, end, has_header) = match section {
        None => (0, lines.iter().position(|l| header(l).is_some()).unwrap_or(lines.len()), true),
        Some(name) => match lines.iter().position(|l| header(l).as_deref() == Some(name)) {
            Some(h) => {
                let end = lines[h + 1..]
                    .iter()
                    .position(|l| header(l).is_some())
                    .map_or(lines.len(), |i| h + 1 + i);
                (h + 1, end, true)
            }
            None => (lines.len(), lines.len(), false),
        },
    };
    let is_key = |l: &str, commented: bool| {
        let mut t = l.trim_start();
        if commented {
            let Some(rest) = t.strip_prefix('#') else { return false };
            t = rest.trim_start();
        }
        t.strip_prefix(key).is_some_and(|r| r.trim_start().starts_with('='))
    };
    let live = (start..end).find(|&i| is_key(&lines[i], false));
    let commented = (start..end).find(|&i| is_key(&lines[i], true));
    match val {
        Some(v) => {
            let line = format!("{key} = {}", toml::Value::String(v.to_string()));
            match live.or(commented) {
                Some(i) => lines[i] = line,
                None if !has_header => {
                    lines.push(String::new());
                    lines.push(format!("[{}]", section.unwrap_or_default()));
                    lines.push(line);
                }
                None => {
                    // after the last non-blank line of the section
                    let mut at = end;
                    while at > start && lines[at - 1].trim().is_empty() {
                        at -= 1;
                    }
                    lines.insert(at, line);
                }
            }
        }
        None => {
            if let Some(i) = live {
                lines[i] = format!("# {}", lines[i].trim_start());
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

impl Config {
    pub fn load() -> Result<Self> {
        let p = path();
        let mut cfg: Config = match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", p.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First run: drop a commented template so there's something to edit.
                let _ = std::fs::create_dir_all(dir());
                let _ = std::fs::write(&p, TEMPLATE);
                Config::default()
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
        };
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("SPOTIFY_CLIENT_ID") {
            cfg.client_id = Some(v);
        }
        if let Some(v) = env("SOLOIST_WS") {
            cfg.soloist.ws = Some(v);
        }
        if let Some(v) = env("SOLOIST_API_KEY") {
            cfg.soloist.api_key = Some(v);
        }
        cfg.client_id = cfg.client_id.filter(|s| !s.trim().is_empty());
        Ok(cfg)
    }

    pub fn redirect_port(&self) -> u16 {
        self.redirect_port.unwrap_or(8898)
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.redirect_port())
    }

    pub fn soloist_data_dir(&self) -> PathBuf {
        if let Some(d) = &self.soloist.data_dir {
            return d.clone();
        }
        #[cfg(windows)]
        {
            return dirs::data_local_dir()
                .unwrap_or_default()
                .join("soloist")
                .join("data");
        }
        #[cfg(target_os = "linux")]
        if let Some(d) = std::env::var_os("STATE_DIRECTORY") {
            return PathBuf::from(d);
        }
        #[allow(unreachable_code)]
        dirs::data_dir().unwrap_or_default().join("soloist")
    }
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("str")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_template_lines() {
        let t = set_in(TEMPLATE, None, "client_id", Some("abc"));
        let t = set_in(&t, Some("soloist"), "api_key", Some("k\"ey"));
        let t = set_in(&t, Some("soloist"), "device_name", Some("desk"));
        let c: Config = toml::from_str(&t).unwrap();
        assert_eq!(c.client_id.as_deref(), Some("abc"));
        assert_eq!(c.soloist.api_key.as_deref(), Some("k\"ey"));
        assert_eq!(c.soloist.device_name.as_deref(), Some("desk"));
        assert!(t.contains("# 1. Spotify Web API"), "comments survive");
        // updating again replaces instead of duplicating
        let t2 = set_in(&t, None, "client_id", Some("xyz"));
        assert_eq!(t2.matches("client_id = ").count(), 1);
        // clearing comments it out
        let t3 = set_in(&t2, Some("soloist"), "api_key", None);
        assert!(toml::from_str::<Config>(&t3).unwrap().soloist.api_key.is_none());
    }

    #[test]
    fn appends_missing_keys_and_sections() {
        let t = set_in("redirect_port = 9000\n", Some("soloist"), "api_key", Some("k"));
        let t = set_in(&t, None, "client_id", Some("c"));
        let c: Config = toml::from_str(&t).unwrap();
        assert_eq!(c.client_id.as_deref(), Some("c"));
        assert_eq!(c.redirect_port, Some(9000));
        assert_eq!(c.soloist.api_key.as_deref(), Some("k"));
    }
}
