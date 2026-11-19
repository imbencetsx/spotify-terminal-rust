# str

A tiny, vim-flavoured, full-screen Spotify player for the terminal, written in Rust.

- **Playback** goes through [Spotify Soloist](https://developer.spotify.com/documentation/soloist)'s local WebSocket API.
- **Search and your library** go through the Spotify Web API.

```
 ┌──────────────┐  │  albums · nova › City Lights
 │  album art   │  │  [art]  ALBUM  City Lights — Nova Lane   ▶ play
 └──────────────┘  │     1  Midnight Tram          Nova Lane       2:30
   Paper Planets   │  ▶  2  Paper Planets          The Quiet Hours 2:37
 1:23 ━━━━●──── 3:07│     3  Salt & Static          Static Bloom    2:45
   ⇄  ⏮  ⏸  ⏭  ↻   │
 / !a nova                                                    albums
 NORMAL  ♫ City Lights                                        ? help
```

## Setup

Run `str`. On first launch it opens a **setup screen**:

1. Create an app at <https://developer.spotify.com/dashboard> and add the redirect URI `http://127.0.0.1:8898/callback`.
2. Paste its client id. Search and library use PKCE, so no client secret is needed.
3. Paste your Soloist API key. `str` downloads Soloist from Spotify, installs it into its own data folder, and runs it. It also updates it before the 90-day build expiry. You can trigger this manually with `str install` or `:install`.
4. Press `⏎`. The values are written into `~/.config/str/config.toml` (your comments are kept), and your browser opens so you can approve the login.

You can reopen the screen with `S`, `:setup` or `str setup`.

If you'd rather run Soloist yourself (Premium required):
```sh
soloist -n "str" -k "$SOLOIST_API_KEY" --ws 127.0.0.1:0
```
Pick the device once in any Spotify app to log it in. `str` finds it automatically.

Install with `cargo install --path .`. Building on macOS uses the Apple Command Line Tools.

## Use

```
str                 open the player
str setup           paste credentials
str login | logout
str now | play [uri] | pause | toggle | next | prev | vol 40 | shuffle on
str install         download / update Soloist
str stop            stop the Soloist daemon
```

### Search

Press `/` and type. Results update live. Start the query with a prefix to filter it, or press `Tab` to cycle through them:

| prefix | finds     |
|--------|-----------|
| `!s`   | songs     |
| `!a`   | albums    |
| `!p`   | playlists |
| `!r`   | artists   |

### Keys

| key | action | key | action |
|---|---|---|---|
| `j` `k` `gg` `G` `^d` `^u` | move | `space` | play / pause |
| `⏎` | play | `n` `p` | next / previous |
| `l` `h` | open / back | `s` `r` | shuffle / repeat |
| `o` | open the playing album | `+` `-` `m` | volume / mute |
| `a` | add to queue | `]` `[` | seek ±10s |
| `Tab` | queue | `H` | library |
| `K` `J` | move the selected queued song up or down | `Ctrl+C` | quit and stop playback |
| `P` | play the open album or playlist | `?` | help |

Commands work like vim: `:vol 50`, `:seek 1:23`, `:shuffle on`, `:repeat track`, `:play spotify:album:…`, `:q`.

The mouse works too. Click to select and double-click to play or open. Click the progress bar to seek. Use the scroll wheel on the volume bar to change volume. The ⇄ ⏮ ⏸ ⏭ ↻ buttons are clickable.

### Queue

Press `a` to add a song to the current `str` session. Open the queue with `Tab` and move a song with `K` / `J`, Shift+Up / Shift+Down, or the arrow buttons. The `↕` marks editable rows. These songs play in order before Spotify’s upcoming tracks. Playing a queued song through Soloist can change the active album or playlist context. Spotify’s own upcoming rows are read-only.

Quitting the player stops a Soloist process it started. If you started Soloist separately, quitting pauses playback on that device and leaves the daemon running.

### macOS Now Playing

The player publishes the current track, artwork, progress, and available controls to macOS Now Playing. Control Center and the media keys can play, pause, seek, and skip. A menu-bar or notch widget can show the track if it uses the system Now Playing session. Set `now_playing = false` in the top level of your config to turn this off.

Album art is drawn as text so it fits the terminal. `A` cycles the style, and `art = "…"` in the config sets the default:
- `pixel` (default): chunky square pixels with a reduced palette.
- `dots`: a dot matrix, where dot size follows brightness.
- `image`: the real picture, using kitty graphics in kitty and Ghostty or iTerm2 graphics in iTerm2 and WezTerm (`images = "…"` overrides this).

### Spotify API limits (Feb 2026 dev-mode rules)

- Search returns 10 results per page. `str` loads more pages as you scroll.
- Spotify only lists the songs of playlists you own. Other playlists still play with `⏎`.
- Soloist's `play` can't start partway into an album. So `⏎` on song N of an album plays that song and queues the rest. `⏎` on the first song plays the whole album.

## Develop

```sh
cargo run --example mock_soloist -- /tmp/fake-soloist   # fake Soloist + fake Web API
# then, using the printed STR_API_BASE, with [soloist] data_dir = "/tmp/fake-soloist":
STR_API_BASE=http://127.0.0.1:PORT/v1 cargo run
cargo test
```
