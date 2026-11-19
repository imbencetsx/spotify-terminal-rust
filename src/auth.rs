//! Spotify Web API login: Authorization Code + PKCE with a loopback redirect.
//! Only a client id is needed (no secret). Tokens live in ~/.config/str/token.json.

use crate::config::{self, Config};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SCOPES: &str = "playlist-read-private playlist-read-collaborative user-library-read";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: u64,
}

fn now_secs() -> u64 {
    crate::model::now_ms() / 1000
}

pub fn token_path() -> PathBuf {
    config::dir().join("token.json")
}

pub fn load() -> Option<Token> {
    let s = std::fs::read_to_string(token_path()).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save(t: &Token) -> Result<()> {
    let p = token_path();
    std::fs::create_dir_all(config::dir())?;
    std::fs::write(&p, serde_json::to_string_pretty(t)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn logout() -> Result<()> {
    match std::fs::remove_file(token_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

fn random_b64(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("random: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let cmd = ("xdg-open", vec![url]);
    let _ = std::process::Command::new(cmd.0)
        .args(cmd.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Browser login. `on_url` gets the authorize URL (the browser is opened too).
pub async fn login(cfg: &Config, http: &reqwest::Client, on_url: impl FnOnce(&str)) -> Result<Token> {
    let client_id = cfg
        .client_id
        .as_deref()
        .context("no client_id configured — edit ~/.config/str/config.toml")?;
    let verifier = random_b64(64)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64(16)?;
    let redirect = cfg.redirect_uri();

    let listener = TcpListener::bind(("127.0.0.1", cfg.redirect_port()))
        .await
        .with_context(|| format!("binding 127.0.0.1:{}", cfg.redirect_port()))?;

    let url = reqwest::Url::parse_with_params(
        "https://accounts.spotify.com/authorize",
        &[
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
            ("state", &state),
            ("scope", SCOPES),
        ],
    )?;
    on_url(url.as_str());
    open_browser(url.as_str());

    let wait = async {
        loop {
            let (mut sock, _) = listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            if !path.starts_with("/callback") {
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                continue;
            }
            let u = reqwest::Url::parse(&format!("http://127.0.0.1{path}"))?;
            let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
            let (ok, body) = match (q.get("error"), q.get("state"), q.get("code")) {
                (Some(e), _, _) => (Err(anyhow!("authorization denied: {e}")), "Login cancelled."),
                (_, Some(s), Some(c)) if *s == state => (Ok(c.clone()), "Connected! You can close this tab and go back to your terminal."),
                _ => (Err(anyhow!("invalid callback (state mismatch)")), "Something went wrong."),
            };
            let html = format!(
                "<!doctype html><meta charset=utf-8><title>str</title><body style=\"font:16px system-ui;background:#111;color:#eee;display:grid;place-items:center;height:100vh;margin:0\"><div><b style=\"color:#1ed760\">str</b> &middot; {body}</div>"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                html.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
            return ok;
        }
    };
    let code = tokio::time::timeout(std::time::Duration::from_secs(300), wait)
        .await
        .context("timed out waiting for the browser login")??;

    let tok = token_request(
        http,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("client_id", client_id),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    save(&tok)?;
    Ok(tok)
}

pub async fn refresh(cfg: &Config, http: &reqwest::Client, old: &Token) -> Result<Token> {
    let client_id = cfg.client_id.as_deref().context("no client_id")?;
    let rt = old
        .refresh_token
        .as_deref()
        .context("session expired — run `str login`")?;
    let mut tok = token_request(
        http,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", rt),
            ("client_id", client_id),
        ],
    )
    .await?;
    if tok.refresh_token.is_none() {
        tok.refresh_token = old.refresh_token.clone();
    }
    save(&tok)?;
    Ok(tok)
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<Token> {
    #[derive(Deserialize)]
    struct Resp {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: u64,
    }
    let r = http.post(TOKEN_URL).form(form).send().await?;
    let status = r.status();
    if !status.is_success() {
        let body = r.text().await.unwrap_or_default();
        bail!("token request failed ({status}): {body}");
    }
    let r: Resp = r.json().await?;
    Ok(Token {
        access_token: r.access_token,
        refresh_token: r.refresh_token,
        expires_at: now_secs() + r.expires_in,
    })
}

impl Token {
    pub fn expired(&self) -> bool {
        self.expires_at <= now_secs() + 60
    }
}
