mod app;
mod art;
mod auth;
mod config;
mod container;
mod install;
mod lyrics;
mod model;
#[cfg(target_os = "macos")]
mod now_playing;
mod soloist;
mod spotify;
mod ui;

use anyhow::{Context, Result, bail};
use app::{App, Msg};
use config::Config;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, EventStream};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const HELP: &str = "\
str — a tiny vim-flavoured Spotify terminal player (via Spotify Soloist)

usage:
  str                 open the player
  str setup           paste your credentials (opens the setup screen)
  str login           connect your Spotify account (search & library)
  str logout          forget the saved Spotify login
  str now             show what's playing
  str play [uri]      resume, or play a spotify: URI
  str pause | toggle | next | prev
  str vol <0-100>     set volume
  str shuffle [on|off]
  str config          print the config path (and create it)
  str install         download / update Spotify Soloist (str does this itself when needed)
  str stop            stop the Soloist daemon

inside the player press ? for keys.
";

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("str: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = Arc::new(Config::load()?);
    let http = reqwest::Client::builder()
        .user_agent(concat!("str/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?;

    let sub = args.first().map(String::as_str).unwrap_or("");
    let arg = args.get(1).map(String::as_str);
    let ctl = |cmd: &str, extra: Value| vec![soloist::command(cmd, extra)];
    match sub {
        "" => tui(cfg, http, false).await,
        "setup" => tui(cfg, http, true).await,
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("str {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "config" => {
            println!("{}", config::path().display());
            Ok(())
        }
        "login" => {
            auth::login(&cfg, &http, |url| {
                eprintln!("\n  str · connecting your Spotify account\n");
                eprintln!("  Opening your browser. If nothing happens, open:\n\n  {url}\n");
            })
            .await?;
            eprintln!("  ✓ logged in");
            Ok(())
        }
        "logout" => {
            auth::logout()?;
            println!("logged out");
            Ok(())
        }
        "stop" => soloist::stop(&cfg),
        "install" | "update" => install_cli(&http).await,
        "now" => now(&cfg).await,
        "play" => oneshot(&cfg, ctl("play", arg.map_or(Value::Null, |u| json!({ "uri": u })))).await,
        "pause" => oneshot(&cfg, ctl("pause", Value::Null)).await,
        "next" | "skip" => oneshot(&cfg, ctl("skip_next", Value::Null)).await,
        "prev" | "previous" => soloist::previous(&addr(&cfg)?).await,
        "vol" | "volume" => {
            let v: u8 = arg.context("usage: str vol <0-100>")?.parse().context("volume must be 0-100")?;
            oneshot(&cfg, ctl("set_volume", json!({ "volume": v.min(100) }))).await
        }
        "shuffle" => {
            let on = !matches!(arg, Some("off" | "false" | "0" | "no"));
            oneshot(&cfg, ctl("set_shuffle", json!({ "enabled": on }))).await
        }
        "toggle" => {
            let addr = addr(&cfg)?;
            let st = soloist::oneshot(&addr, vec![], true).await?.unwrap_or_default();
            let cmd = if st["status"] == "playing" { "pause" } else { "play" };
            soloist::oneshot(&addr, ctl(cmd, Value::Null), false).await.map(|_| ())
        }
        other => bail!("unknown command `{other}` — try `str --help`"),
    }
}

async fn install_cli(http: &reqwest::Client) -> Result<()> {
    use std::io::Write;
    eprint!("downloading soloist from Spotify…");
    let v = install::install(http, |p| {
        eprint!("\rdownloading soloist from Spotify… {p:>3}%");
        let _ = std::io::stderr().flush();
    })
    .await?;
    eprintln!();
    println!("✓ installed {}", v.text);
    println!("  at {}", install::managed_bin().display());
    if let Some(d) = v.days_left() {
        println!("  this build works for {d} more days; str updates it automatically");
    }
    Ok(())
}

fn addr(cfg: &Config) -> Result<String> {
    soloist::endpoint(cfg).context(
        "can't find a running Soloist — start it with `soloist -n <name> -k <key> --ws 127.0.0.1:0`",
    )
}

async fn oneshot(cfg: &Config, cmds: Vec<Value>) -> Result<()> {
    soloist::oneshot(&addr(cfg)?, cmds, false).await.map(|_| ())
}

async fn now(cfg: &Config) -> Result<()> {
    let st = soloist::oneshot(&addr(cfg)?, vec![], true).await?.unwrap_or_default();
    let Some(it) = soloist::entity(&st["item"]) else {
        println!("nothing playing");
        return Ok(());
    };
    let icon = if st["status"] == "playing" { "▶" } else { "⏸" };
    let pos = st["position"]["position_ms"].as_u64().unwrap_or(0);
    let dur = it.duration_ms.map(model::fmt_ms).unwrap_or_default();
    println!("{icon} {} — {}  ({})  {}/{}", it.name, it.sub, it.extra, model::fmt_ms(pos), dur);
    Ok(())
}

/// Pick how album art is drawn. We avoid querying the terminal over stdin by
/// default: when a terminal doesn't answer, the query thread keeps reading
/// stdin and swallows the first keypress. Env vars are reliable enough.
fn make_picker(cfg: &Config) -> Picker {
    let mode = cfg.images.as_deref().unwrap_or("auto");
    if mode == "query" {
        return Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    }
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let proto = match mode {
        "kitty" => Some(ProtocolType::Kitty),
        "iterm" | "iterm2" => Some(ProtocolType::Iterm2),
        "sixel" => Some(ProtocolType::Sixel),
        "auto" if env("TMUX").is_empty() => {
            let (term, prog) = (env("TERM"), env("TERM_PROGRAM"));
            if !env("KITTY_WINDOW_ID").is_empty()
                || term == "xterm-kitty"
                || term == "xterm-ghostty"
                || prog.eq_ignore_ascii_case("ghostty")
            {
                Some(ProtocolType::Kitty)
            } else if prog == "iTerm.app" || prog == "WezTerm" || !env("WEZTERM_EXECUTABLE").is_empty() {
                Some(ProtocolType::Iterm2)
            } else {
                None
            }
        }
        _ => None,
    };
    // Graphics protocols need the cell size in pixels; ask the kernel, not the terminal.
    let font = crossterm::terminal::window_size()
        .ok()
        .filter(|w| w.width > 0 && w.height > 0 && w.columns > 0 && w.rows > 0)
        .map(|w| FontSize::new(w.width / w.columns, w.height / w.rows));
    match (proto, font) {
        (Some(proto), Some(font)) => {
            #[allow(deprecated)]
            let mut p = Picker::from_fontsize(font);
            p.set_protocol_type(proto);
            p
        }
        _ => Picker::halfblocks(),
    }
}

async fn tui(cfg: Arc<Config>, http: reqwest::Client, force_setup: bool) -> Result<()> {
    // Web API is optional; a saved session is used as-is, login happens in the TUI.
    let api = cfg
        .client_id
        .as_ref()
        .and(auth::load())
        .map(|t| spotify::Api::new(http.clone(), cfg.clone(), t));
    let first_run = cfg.client_id.is_none() && cfg.soloist.api_key.is_none() && soloist::endpoint(&cfg).is_none();

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let (ws_tx, ws_rx) = mpsc::unbounded_channel();
    let (sol_tx, mut sol_rx) = mpsc::unbounded_channel::<soloist::Event>();
    let connection = tokio::spawn(soloist::run(cfg.clone(), sol_tx, ws_rx));
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(e) = sol_rx.recv().await {
                if tx.send(Msg::Soloist(e)).is_err() {
                    break;
                }
            }
        });
    }

    let mut terminal = ratatui::init();
    let picker = make_picker(&cfg);
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        prev_hook(info);
    }));

    let needs_login = cfg.client_id.is_some() && api.is_none();
    let art = cfg.art.as_deref().map(art::Style::parse).unwrap_or(art::Style::Pixel);
    #[cfg(target_os = "macos")]
    let media = if cfg.now_playing.unwrap_or(true) {
        now_playing::Bridge::start(tx.clone()).await.map(Some)
    } else {
        Ok(None)
    };
    let mut app = App::new(cfg.clone(), api, None, http, tx, ws_tx, picker);
    app.art = art;
    if let Some(b) = app.cfg.background.as_deref() {
        app.bg_mode = art::BgMode::parse(b);
    }
    // Playback: start Soloist ourselves (downloading it if needed) when configured.
    app.ensure_soloist();
    app.check_update();
    if force_setup || first_run {
        app.open_setup();
    } else if needs_login {
        app.start_login();
    }
    #[cfg(target_os = "macos")]
    let media = match media {
        Ok(bridge) => bridge,
        Err(e) => {
            app.error(format!("macOS Now Playing unavailable: {e:#}"));
            None
        }
    };
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    let termination = async {
        #[cfg(unix)]
        if let Ok(signal) = &mut terminate {
            signal.recv().await;
            return;
        }
        std::future::pending::<()>().await;
    };
    tokio::pin!(termination);

    let res: Result<()> = async {
        loop {
            app.sync_art();
            app.sync_lyrics();
            terminal.draw(|f| ui::draw(f, &mut app))?;
            tokio::select! {
                signal = &mut interrupt => return signal.map_err(Into::into),
                _ = &mut termination => return Ok(()),
                ev = events.next() => match ev {
                    Some(Ok(ev)) => app.on_event(ev),
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                },
                Some(m) = rx.recv() => app.on_msg(m),
                _ = tick.tick() => app.on_tick(),
            }
            if app.quit {
                return Ok(());
            }
            while let Ok(m) = rx.try_recv() {
                app.on_msg(m);
            }
            #[cfg(target_os = "macos")]
            if let Some(media) = &media {
                media.sync(&app.player, app.shuffle_enabled());
            }
        }
    }
    .await;

    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    connection.abort();
    let _ = connection.await;
    let stop_playback = async {
        if let Some(child) = app.soloist_child.take() {
            soloist::shutdown_child(child)
                .await
                .context("stopping Soloist")
        } else if let Some(addr) = soloist::endpoint(&cfg) {
            match tokio::time::timeout(Duration::from_secs(2), soloist::pause_for_exit(&addr)).await
            {
                Ok(Err(_)) | Err(_) if !app.player.connected && !app.player.playing() => Ok(()),
                result => result
                    .context("timed out stopping playback")
                    .and_then(|r| r),
            }
        } else {
            Ok(())
        }
    };
    #[cfg(target_os = "macos")]
    let (cleanup, ()) = tokio::join!(stop_playback, async {
        if let Some(media) = media {
            media.stop().await;
        }
    });
    #[cfg(not(target_os = "macos"))]
    let cleanup = stop_playback.await;
    res.and(cleanup)
}
