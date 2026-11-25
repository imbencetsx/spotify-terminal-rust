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

### Macs without Apple Silicon

Spotify only builds Soloist for Apple Silicon Macs. On any other Mac, `str` installs the Linux x86_64 build and runs it in a container. It goes by the hardware, so an x86_64 `str` under Rosetta still gets the native build. You need:

- a container runtime: Docker Desktop, OrbStack, Colima or Podman, and it has to be running
- PulseAudio for sound: `brew install pulseaudio`. `str` starts it and opens a listener on `127.0.0.1:4718` that only this Mac can reach. Set `PULSE_SERVER` to use a different server.

The first install also builds a small runtime image (`str-soloist-runtime`), which takes about a minute. Spotify apps can't see a device inside the container's VM, so `str` advertises it over Bonjour with `dns-sd`. Pick it in a Spotify app on your network once to log it in, as usual. `STR_SOLOIST_CONTAINER=1` forces this setup on Apple Silicon too, which is useful for testing it.

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
| `L` | show or hide lyrics | `⏎` on a synced lyric | seek to that line |

Commands work like vim: `:vol 50`, `:seek 1:23`, `:shuffle on`, `:repeat track`, `:play spotify:album:…`, `:q`.

Click a cover tile to open it or a song to play it. In the queue, click to select and double-click to play. Click the progress bar to seek. Use the scroll wheel on the volume bar to change volume. The ⇄ ⏮ ⏸ ⏭ ↻ buttons are clickable.

Select a playlist or open it and type `:pin` to pin it to the top of the home grid. Pinned playlists show a star and are saved locally. Use `:pin` again or `:unpin` to remove a pin.

Click **Sort** on Home or type `:sort` to cycle through Spotify order, A–Z, and most songs. Use `:sort spotify`, `:sort name`, or `:sort tracks` to choose directly. Pins stay at the top and the sort setting is saved.

### Queue

Press `a` to add a song to the queue and `Tab` to see what’s next. **Next in queue** lists songs you added. Move them with `K` / `J`, Shift+Up / Shift+Down, or the arrow buttons. They play before the tracks listed under **Next from**.

Press `p` to go back through the songs played since you opened `str`. When a song in your history ends, playback returns through the songs you just left before continuing the queue or collection. The queue shows those returning songs under **Resume playback**.

Click **Clear queue** or type `:clearqueue` to remove songs added in `str`. The active collection and songs queued in Spotify stay in place.

Click a track in an album, playlist, or Liked Songs to start there. The collection continues in order or shuffled, according to the shuffle toggle. Clicking a search result plays that track on its own and Soloist handles subsequent autoplay if available.

Quitting the player stops a Soloist process it started. If you started Soloist separately, quitting pauses playback on that device and leaves the daemon running.

### Lyrics

Press `L` or click **Lyrics** in the player. Synced lyrics highlight the current line and follow playback. Scroll or use `j` / `k` to read ahead. Click a timed line or press Enter to seek to it. Plain lyrics can be scrolled when synced lyrics are unavailable.

The player loads lyrics automatically and previews the previous line, the current line, and the next two lines when synced lyrics are available. Lookups send the song’s title, artist, album, and duration to [LRCLIB](https://lrclib.net/docs). Set `lyrics = false` at the top level of your config to disable lookups.

### macOS Now Playing

The player publishes the current track, artwork, progress, and available controls to macOS Now Playing. The small app badge uses the launching terminal’s icon when macOS can identify it. Control Center and the media keys can play, pause, seek, and skip. A menu-bar or notch widget can show the track if it uses the system Now Playing session. Set `now_playing = false` in the top level of your config to turn this off.

Use `:art image` for smooth artwork at the terminal’s pixel resolution. `A` cycles styles, and `art = "…"` in the config sets the default.
- `pixel` (default): pixel art scaled to the cover size. Tiny covers use a chunkier grid, and larger covers retain more detail. Terminals without image support use text blocks.
- `dots`: a dot matrix, where dot size follows brightness.
- `image`: the real picture, using kitty graphics in kitty and Ghostty or iTerm2 graphics in iTerm2 and WezTerm (`images = "…"` overrides this).
- `softpixel`: bitmap covers with a fine pixel grid, drawn in the same space as `image`. Use `:art softpixel` to try it in the current session.

List thumbnails use terminal graphics when available. Other terminals use a denser text rendering in the same four columns by two rows.

### Spotify API limits (Feb 2026 dev-mode rules)

- Search returns 10 results per page. `str` loads more pages as you scroll.
- Spotify only lists the songs of playlists you own. Other playlists still play with `⏎`.

## Develop

```sh
cargo run --example mock_soloist -- /tmp/fake-soloist   # fake Soloist + fake Web API
# then, using the printed STR_API_BASE, with [soloist] data_dir = "/tmp/fake-soloist":
STR_API_BASE=http://127.0.0.1:PORT/v1 STR_LYRICS_BASE=http://127.0.0.1:PORT/lyrics cargo run
cargo test
```
