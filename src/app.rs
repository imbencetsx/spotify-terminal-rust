//! App state + input handling.

use crate::config::{Config, PlaylistSort};
use crate::model::{Item, Kind, Page, Source, parse_query};
use crate::soloist::{self, Change, Player, Repeat};
use crate::spotify::Api;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use image::DynamicImage;
use ratatui::layout::{Position, Rect};
use ratatui_image::Resize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

pub enum Msg {
    Soloist(soloist::Event),
    Page {
        view: u64,
        offset: usize,
        result: Result<Box<Page>, String>,
    },
    Image {
        url: String,
        img: Option<DynamicImage>,
    },
    Queued {
        items: Vec<Item>,
        name: String,
        revision: u64,
    },
    ContextPage {
        context: u64,
        result: Result<Box<Page>, String>,
    },
    LoginUrl(String),
    InstallProgress(u8),
    Installed { result: Result<String, String>, then_start: bool },
    NeedsUpdate(i64),
    StartSoloist,
    Login(Result<crate::auth::Token, String>),
    Lyrics { uri: String, state: crate::lyrics::State },
    #[cfg(target_os = "macos")]
    Media(crate::now_playing::Action),
}

/// The setup screen: paste credentials, save them to config.toml, log in.
pub struct Setup {
    /// client id, soloist api key, device name
    pub fields: [Input; 3],
    /// 0..3 = fields, 3 = save, 4 = cancel
    pub focus: usize,
    pub stage: SetupStage,
    pub reveal: bool,
}

pub enum SetupStage {
    Editing,
    LoggingIn(Option<String>),
    Error(String),
}

pub const SETUP_LABELS: [&str; 3] = ["client id", "api key", "device name"];

impl Setup {
    pub fn looks_like_client_id(s: &str) -> bool {
        s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Command,
}

pub struct View {
    pub id: u64,
    pub title: String,
    pub source: Source,
    pub header: Option<Item>,
    pub items: Vec<Item>,
    pub sel: usize,
    pub scroll: usize,
    pub next: Option<usize>,
    pub loading: bool,
    pub note: Option<String>,
    pub error: Option<String>,
    /// Select this uri once it shows up (e.g. "open album of this track").
    pub focus: Option<String>,
}

impl View {
    pub fn selected(&self) -> Option<&Item> {
        self.items.get(self.sel)
    }
}

#[derive(Default)]
pub struct Input {
    pub text: String,
    /// Cursor, in chars.
    pub cur: usize,
}

impl Input {
    fn byte(&self, ch: usize) -> usize {
        self.text.char_indices().nth(ch).map_or(self.text.len(), |(b, _)| b)
    }
    fn len(&self) -> usize {
        self.text.chars().count()
    }
    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cur = self.len();
    }
    /// Returns true if the text changed.
    fn edit(&mut self, k: &KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Char('u') if ctrl => {
                let b = self.byte(self.cur);
                self.text.drain(..b);
                self.cur = 0;
                true
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if ctrl || alt => self.delete_word(),
            KeyCode::Char('a') if ctrl => {
                self.cur = 0;
                false
            }
            KeyCode::Char('e') if ctrl => {
                self.cur = self.len();
                false
            }
            KeyCode::Char(c) if !ctrl => {
                let b = self.byte(self.cur);
                self.text.insert(b, c);
                self.cur += 1;
                true
            }
            KeyCode::Backspace if self.cur > 0 => {
                let b = self.byte(self.cur - 1);
                self.text.remove(b);
                self.cur -= 1;
                true
            }
            KeyCode::Delete if self.cur < self.len() => {
                let b = self.byte(self.cur);
                self.text.remove(b);
                true
            }
            KeyCode::Left => {
                self.cur = self.cur.saturating_sub(1);
                false
            }
            KeyCode::Right => {
                self.cur = (self.cur + 1).min(self.len());
                false
            }
            KeyCode::Home => {
                self.cur = 0;
                false
            }
            KeyCode::End => {
                self.cur = self.len();
                false
            }
            _ => false,
        }
    }
    fn delete_word(&mut self) -> bool {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cur;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let (a, b) = (self.byte(i), self.byte(self.cur));
        self.text.drain(a..b);
        let changed = i != self.cur;
        self.cur = i;
        changed
    }
}

#[derive(Clone, Debug)]
pub enum Hit {
    PlaylistSort,
    SetupField(usize),
    SetupSave,
    SetupCancel,
    OpenSetup,
    Row(usize),
    Shuffle,
    Prev,
    Toggle,
    Next,
    Repeat,
    Progress,
    Volume,
    SearchBar,
    Crumb(usize),
    PlayHeader,
    List,
    /// The lyrics card in the player: opens the lyrics page.
    Lyrics,
    /// A line on the lyrics page: seeks there.
    LyricLine(usize),
    QueueMove(isize),
    QueueClear,
}

struct PlaybackContext {
    id: u64,
    source: Source,
    name: String,
    tracks: Vec<Item>,
    cursor: usize,
    upcoming: Vec<usize>,
    played: HashSet<usize>,
    next_page: Option<usize>,
    advance_on_load: bool,
}

impl PlaybackContext {
    fn reorder(&mut self, shuffle: bool) {
        let start = if shuffle { 0 } else { self.cursor + 1 };
        self.upcoming = (start..self.tracks.len())
            .filter(|i| !self.played.contains(i))
            .collect();
        if shuffle {
            self.upcoming
                .sort_by_cached_key(|_| getrandom::u64().unwrap_or_default());
        }
    }
}

#[derive(Clone, Copy)]
enum TrackOrigin {
    Queue { index: usize, revision: u64 },
    Context(usize),
}

struct TrackStart {
    track: Item,
    origin: TrackOrigin,
    from: Option<Item>,
    from_cursor: Option<usize>,
    since: Instant,
    refresh_requested: bool,
}

struct PlayedTrack {
    uri: String,
    previous: Item,
    origin: TrackOrigin,
    from_cursor: Option<usize>,
}

struct ManualAdvance {
    since: Instant,
    expected_uri: Option<String>,
    refresh_requested: bool,
}

pub enum Thumbnail {
    Bitmap(Protocol),
    Text(crate::art::Rendered),
}

pub struct App {
    pub cfg: Arc<Config>,
    pub api: Option<Api>,
    pub api_problem: Option<String>,
    http: reqwest::Client,
    tx: UnboundedSender<Msg>,
    ws: UnboundedSender<soloist::Request>,
    pub player: Player,
    /// A standalone track can reset Soloist's context options.
    shuffle_preference: Option<bool>,
    queued: Vec<Item>,
    queue_revision: u64,
    context: Option<PlaybackContext>,
    queue_start: Option<TrackStart>,
    deferred_skip: i8,
    queue_history: Vec<PlayedTrack>,
    queue_retry_blocked: bool,
    manual_advance: Option<ManualAdvance>,
    pub soloist_child: Option<tokio::process::Child>,
    pub views: Vec<View>,
    home_items: Vec<Item>,
    playlist_pins: Vec<Item>,
    pub mode: Mode,
    pub query: Input,
    pub cmd: Input,
    search_due: Option<Instant>,
    last_search: String,
    pub msg: Option<(String, bool, Instant)>,
    pub help: bool,
    pub quit: bool,
    pub setup: Option<Setup>,
    /// Download progress while installing soloist.
    pub installing: Option<u8>,
    expired_retries: u8,
    login_task: Option<tokio::task::JoinHandle<()>>,
    pub picker: Picker,
    images: HashMap<String, DynamicImage>,
    pending_images: HashSet<String>,
    /// How covers are drawn.
    pub art: crate::art::Style,
    /// Cover url currently shown per slot (0 = now playing, 1 = list header).
    /// Lags the wanted url until its image has loaded, so covers don't flash.
    pub shown: [Option<String>; 2],
    art_cache: HashMap<(String, u16, u16, crate::art::Style), crate::art::Rendered>,
    thumbnails: HashMap<(String, u16, u16, crate::art::Style), Thumbnail>,
    track_digits: [Option<Protocol>; 20],
    pub hits: Vec<(Rect, Hit)>,
    pub pointer: Option<Position>,
    pub list_height: usize,
    /// Columns of the playlist cover grid (set while drawing; 1 = plain list).
    pub lib_cols: usize,
    /// Blurred cover behind the player (and the previous one, fading out).
    pub backdrop: Option<(String, crate::art::Backdrop)>,
    pub backdrop_old: Option<crate::art::Backdrop>,
    /// 0 → 1 crossfade from `backdrop_old` to `backdrop`.
    pub backdrop_mix: f32,
    /// Backdrop rotation (radians); turns while music plays.
    pub spin: f32,
    pub bg_mode: crate::art::BgMode,
    /// UI accent colour; eases toward `accent_target` (from the playing cover).
    pub accent: (f32, f32, f32),
    accent_target: (f32, f32, f32),
    /// Lyrics per track uri (a small cache).
    pub lyrics: HashMap<String, crate::lyrics::State>,
    /// Set while you scroll the lyrics page by hand; it follows the song again
    /// a few seconds later.
    pub lyrics_manual: Option<Instant>,
    last_click: Option<(Instant, u16, u16)>,
    pending_g: bool,
    muted_from: Option<u8>,
    next_id: u64,
}

impl App {
    pub fn new(
        cfg: Arc<Config>,
        api: Option<Api>,
        api_problem: Option<String>,
        http: reqwest::Client,
        tx: UnboundedSender<Msg>,
        ws: UnboundedSender<soloist::Request>,
        picker: Picker,
    ) -> Self {
        let mut app = App {
            cfg,
            api,
            api_problem,
            http,
            tx,
            ws,
            player: Player::default(),
            shuffle_preference: None,
            queued: Vec::new(),
            queue_revision: 0,
            context: None,
            queue_start: None,
            deferred_skip: 0,
            queue_history: Vec::new(),
            queue_retry_blocked: false,
            manual_advance: None,
            soloist_child: None,
            views: Vec::new(),
            home_items: Vec::new(),
            playlist_pins: Vec::new(),
            mode: Mode::Normal,
            query: Input::default(),
            cmd: Input::default(),
            search_due: None,
            last_search: String::new(),
            msg: None,
            help: false,
            quit: false,
            setup: None,
            installing: None,
            expired_retries: 0,
            login_task: None,
            picker,
            images: HashMap::new(),
            pending_images: HashSet::new(),
            art: crate::art::Style::Pixel,
            shown: [None, None],
            art_cache: HashMap::new(),
            thumbnails: HashMap::new(),
            track_digits: std::array::from_fn(|_| None),
            hits: Vec::new(),
            pointer: None,
            list_height: 20,
            lib_cols: 1,
            backdrop: None,
            backdrop_old: None,
            backdrop_mix: 1.0,
            spin: 0.0,
            bg_mode: crate::art::BgMode::Spin,
            accent: crate::ui::DEFAULT_ACCENT,
            accent_target: crate::ui::DEFAULT_ACCENT,
            lyrics: HashMap::new(),
            lyrics_manual: None,
            last_click: None,
            pending_g: false,
            muted_from: None,
            next_id: 1,
        };
        match crate::config::load_playlist_pins() {
            Ok(pins) => app.playlist_pins = pins,
            Err(e) => app.error(format!("{e:#}")),
        }
        app.go_home();
        app
    }

    pub fn view(&self) -> &View {
        self.views.last().expect("view stack is never empty")
    }
    fn view_mut(&mut self) -> &mut View {
        self.views.last_mut().expect("view stack is never empty")
    }

    pub fn info(&mut self, s: impl Into<String>) {
        self.msg = Some((s.into(), false, Instant::now()));
    }
    pub fn error(&mut self, s: impl Into<String>) {
        self.msg = Some((s.into(), true, Instant::now()));
    }

    // ------------------------------------------------------------- views --

    fn new_view(&mut self, title: impl Into<String>, source: Source) -> View {
        self.next_id += 1;
        View {
            id: self.next_id,
            title: title.into(),
            source,
            header: None,
            items: Vec::new(),
            sel: 0,
            scroll: 0,
            next: None,
            loading: false,
            note: None,
            error: None,
            focus: None,
        }
    }

    fn load(&mut self, idx: usize, offset: usize) {
        if self.api.is_none() || matches!(self.views[idx].source, Source::Queue | Source::Lyrics) {
            return;
        }
        let v = &mut self.views[idx];
        v.loading = true;
        let (id, src) = (v.id, v.source.clone());
        self.spawn_load(id, src, offset);
    }

    fn spawn_load(&self, id: u64, src: Source, offset: usize) {
        let Some(api) = self.api.clone() else { return };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.fetch(&src, offset).await.map(Box::new).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Page { view: id, offset, result });
        });
    }

    fn view_by_id(&mut self, id: u64) -> Option<&mut View> {
        self.views.iter_mut().find(|v| v.id == id)
    }

    fn push(&mut self, v: View) {
        self.views.push(v);
        let i = self.views.len() - 1;
        self.load(i, 0);
    }

    fn replace_root(&mut self, v: View) {
        self.views.clear();
        self.push(v);
    }

    pub fn go_home(&mut self) {
        let mut v = self.new_view("Playlists", Source::Home);
        self.home_items.clear();
        v.items = self.playlist_pins.clone();
        self.replace_root(v);
    }

    pub fn playlist_pinned(&self, uri: &str) -> bool {
        self.playlist_pins.iter().any(|item| item.uri == uri)
    }

    fn refresh_home_playlists(&mut self) {
        let mut unpinned: Vec<_> = self.home_items.iter()
            .filter(|item| !self.playlist_pinned(&item.uri))
            .collect();
        match self.cfg.playlist_sort {
            PlaylistSort::Spotify => {}
            PlaylistSort::Name => unpinned.sort_by_cached_key(|item| {
                (item.kind != Kind::Liked, item.name.to_lowercase())
            }),
            PlaylistSort::Tracks => unpinned.sort_by_cached_key(|item| {
                let count = item.extra.strip_suffix(" songs").and_then(|n| n.parse::<u64>().ok());
                (item.kind != Kind::Liked, Reverse(count), item.name.to_lowercase())
            }),
        }
        let items = self
            .playlist_pins
            .iter()
            .map(|pin| {
                self.home_items
                    .iter()
                    .find(|item| item.uri == pin.uri)
                    .unwrap_or(pin)
            })
            .chain(unpinned)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(view) = self
            .views
            .iter_mut()
            .find(|view| view.source == Source::Home)
        {
            let selected = view
                .selected()
                .and_then(|item| items.iter().position(|i| i.uri == item.uri));
            view.items = items;
            view.sel = selected.unwrap_or(0);
        }
    }

    fn sort_playlists(&mut self, arg: &str) {
        let sort = match arg {
            "" => self.cfg.playlist_sort.next(),
            "spotify" => PlaylistSort::Spotify,
            "name" => PlaylistSort::Name,
            "tracks" => PlaylistSort::Tracks,
            _ => {
                self.error("Use :sort spotify, :sort name, or :sort tracks");
                return;
            }
        };
        match crate::config::save_values(&[(None, "playlist_sort", Some(sort.key()))]) {
            Ok(()) => {
                Arc::make_mut(&mut self.cfg).playlist_sort = sort;
                self.refresh_home_playlists();
                self.load_remaining_sorted_playlists();
                self.info(format!("Playlists sorted by {}", sort.label()));
            }
            Err(e) => self.error(format!("Couldn’t save playlist sort: {e:#}")),
        }
    }

    fn load_remaining_sorted_playlists(&mut self) {
        if self.cfg.playlist_sort == PlaylistSort::Spotify {
            return;
        }
        if let Some((index, view)) = self.views.iter().enumerate()
            .find(|(_, view)| view.source == Source::Home && !view.loading)
            && let Some(offset) = view.next
        {
            self.load(index, offset);
        }
    }

    fn pin_playlist(&mut self, pin: Option<bool>) {
        let Some(item) = self
            .view()
            .header
            .as_ref()
            .filter(|item| item.kind == Kind::Playlist)
            .or_else(|| {
                self.view()
                    .selected()
                    .filter(|item| item.kind == Kind::Playlist)
            })
            .cloned()
        else {
            self.info("Select or open a playlist to pin it");
            return;
        };
        let result = (|| {
            let mut pins = crate::config::load_playlist_pins()?;
            let index = pins.iter().position(|p| p.uri == item.uri);
            let pinned = pin.unwrap_or(index.is_none());
            let name = item.name.clone();
            match (pinned, index) {
                (true, None) => pins.push(item),
                (false, Some(i)) => {
                    pins.remove(i);
                }
                _ => {}
            }
            crate::config::save_playlist_pins(&pins)?;
            Ok::<_, anyhow::Error>((pins, pinned, name))
        })();
        match result {
            Ok((pins, pinned, name)) => {
                self.playlist_pins = pins;
                self.refresh_home_playlists();
                self.info(format!(
                    "{} {name}",
                    if pinned { "Pinned" } else { "Unpinned" }
                ));
            }
            Err(e) => self.error(format!("Couldn’t save playlist pins: {e:#}")),
        }
    }

    fn open_queue(&mut self) {
        if self.view().source == Source::Queue {
            self.back();
            return;
        }
        let v = self.new_view("Queue", Source::Queue);
        self.views.push(v);
        self.refresh_queue_views();
        if self.player.logged_in {
            self.send("get_queue", json!({ "limit": 0 }));
        }
    }

    fn refresh_queue_views(&mut self) {
        if !self.views.iter().any(|view| view.source == Source::Queue) {
            return;
        }
        let upcoming = self.context.as_ref().map_or_else(
            || self.player.queue.clone(),
            |context| {
                context
                    .upcoming
                    .iter()
                    .map(|&i| context.tracks[i].clone())
                    .collect()
            },
        );
        for v in self.views.iter_mut().filter(|v| v.source == Source::Queue) {
            v.items.clear();
            v.items.extend(self.queued.iter().chain(&upcoming).cloned());
            v.loading = self
                .context
                .as_ref()
                .is_some_and(|context| context.next_page.is_some());
            v.sel = v.sel.min(v.items.len().saturating_sub(1));
        }
    }

    pub fn queued_len(&self) -> usize {
        self.queued.len()
    }

    fn clear_queue(&mut self) {
        self.queue_revision += 1;
        self.queued.clear();
        self.queue_retry_blocked = false;
        self.refresh_queue_views();
        self.info("Cleared songs queued in str");
    }

    pub fn upcoming_label(&self) -> String {
        if let Some(context) = &self.context {
            format!("Next from {}", context.name)
        } else if let Some(context) = &self.player.context
            && !context.name.is_empty()
        {
            format!("Next from {}", context.name)
        } else {
            "Spotify upcoming".into()
        }
    }

    fn move_queue_item(&mut self, delta: isize) {
        let from = self.view().sel;
        if from >= self.queued.len() {
            self.info("Only songs added to the queue can be moved");
            return;
        }
        let to = from.saturating_add_signed(delta).min(self.queued.len() - 1);
        self.queued.swap(from, to);
        self.refresh_queue_views();
        self.select(to);
    }

    fn start_queued(&mut self, index: usize) {
        if let Some(item) = self.queued.get(index).cloned()
            && self.start_followup(
                item,
                TrackOrigin::Queue {
                    index,
                    revision: self.queue_revision,
                },
            )
        {
            self.queued.remove(index);
            self.refresh_queue_views();
        }
    }

    fn start_context_track(&mut self, index: usize) {
        if let Some(item) = self
            .context
            .as_ref()
            .and_then(|ctx| ctx.tracks.get(index))
            .cloned()
        {
            self.start_followup(item, TrackOrigin::Context(index));
        }
    }

    fn start_followup(&mut self, track: Item, origin: TrackOrigin) -> bool {
        if self.queue_start.is_some() {
            return false;
        }
        self.ensure_active();
        self.shuffle_preference = Some(self.shuffle_enabled());
        if !self.send("play", json!({ "uri": track.uri })) {
            return false;
        }
        self.manual_advance = None;
        self.queue_retry_blocked = false;
        self.queue_start = Some(TrackStart {
            track,
            origin,
            from: self.player.item.clone(),
            from_cursor: self.context.as_ref().map(|context| context.cursor),
            since: Instant::now(),
            refresh_requested: false,
        });
        true
    }

    fn restore_queue_start(&mut self) {
        if let Some(start) = self.queue_start.take() {
            self.queue_retry_blocked = match start.origin {
                TrackOrigin::Queue { index, revision } if revision == self.queue_revision => {
                    self.queued.insert(index.min(self.queued.len()), start.track);
                    true
                }
                TrackOrigin::Queue { .. } => false,
                TrackOrigin::Context(_) => true,
            };
            self.refresh_queue_views();
        }
    }

    fn open(&mut self, it: &Item, focus: Option<String>) {
        let src = match it.kind {
            Kind::Album => Source::Album(it.id.clone()),
            Kind::Playlist => Source::Playlist(it.id.clone()),
            Kind::Artist => Source::Artist(it.id.clone()),
            Kind::Liked => Source::Liked,
            Kind::Track | Kind::Episode => match &it.album_uri {
                Some(a) if a.starts_with("spotify:album:") => {
                    let al = Item::new(Kind::Album, a.clone(), it.extra.clone());
                    return self.open(&al, Some(it.uri.clone()));
                }
                _ => return,
            },
        };
        if self.api.is_none() {
            self.error("connect the Spotify Web API to browse (see :help)");
            return;
        }
        let mut v = self.new_view(it.name.clone(), src);
        v.header = Some(it.clone()).filter(|h| h.kind != Kind::Track);
        v.focus = focus;
        self.push(v);
    }

    fn back(&mut self) {
        if self.views.len() > 1 {
            self.views.pop();
        } else if self.view().source != Source::Home {
            // back out of a search to your playlists
            self.go_home_clear();
        }
    }

    fn go_home_clear(&mut self) {
        self.query.set("");
        self.last_search.clear();
        self.go_home();
    }

    fn open_selected(&mut self) {
        if let Some(it) = self.view().selected().cloned() {
            self.open(&it, None);
        }
    }

    fn select(&mut self, i: usize) {
        self.pointer = None;
        if self.view().source == Source::Lyrics {
            // move the lyrics cursor (and stop following the song for a bit)
            let n = self.current_lyrics().map_or(0, |l| l.lines.len());
            self.view_mut().sel = i.min(n.saturating_sub(1));
            self.lyrics_manual = Some(Instant::now());
            return;
        }
        let v = self.view_mut();
        if v.items.is_empty() {
            v.sel = 0;
            return;
        }
        v.sel = i.min(v.items.len() - 1);
        self.maybe_load_more();
    }

    fn move_sel(&mut self, d: isize) {
        let s = self.view().sel as isize + d;
        self.select(s.max(0) as usize);
    }

    fn maybe_load_more(&mut self) {
        let i = self.views.len() - 1;
        let v = &self.views[i];
        if let Some(off) = v.next
            && !v.loading
            && v.sel + 8 >= v.items.len()
        {
            self.load(i, off);
        }
    }

    // ------------------------------------------------------------ search --

    fn run_search(&mut self) {
        self.search_due = None;
        let q = self.query.text.trim().to_string();
        if q == self.last_search {
            return;
        }
        self.last_search = q.clone();
        if q.is_empty() {
            self.go_home();
            return;
        }
        let (kinds, text) = parse_query(&q);
        if text.is_empty() {
            return;
        }
        if self.api.is_none() {
            self.error("search needs the Spotify Web API — see :help");
            return;
        }
        let title = match kinds.as_slice() {
            [k] => format!("{} · {text}", k.plural()),
            _ => format!("“{text}”"),
        };
        let v = self.new_view(title, Source::Search { text, kinds });
        self.replace_root(v);
    }

    /// Tab in search: cycle all → !s → !a → !p → !r → all.
    fn cycle_filter(&mut self) {
        let (kinds, text) = parse_query(&self.query.text);
        let next = match kinds.as_slice() {
            [Kind::Track] => "!a ",
            [Kind::Album] => "!p ",
            [Kind::Playlist] => "!r ",
            [Kind::Artist] => "",
            _ => "!s ",
        };
        self.query.set(&format!("{next}{text}"));
        self.search_due = Some(Instant::now());
    }

    // ---------------------------------------------------------- playback --

    fn send(&mut self, cmd: &str, extra: Value) -> bool {
        self.request(soloist::Request::Send(soloist::command(cmd, extra)))
    }

    fn request(&mut self, request: soloist::Request) -> bool {
        if !self.player.connected {
            let r = self.player.down_reason.clone();
            self.error(if r.is_empty() { "soloist not connected".into() } else { r });
            return false;
        }
        self.ws.send(request).is_ok()
    }

    fn ensure_active(&mut self) {
        if self.player.connected && self.player.logged_in && !self.player.active {
            self.send("activate", Value::Null);
        }
    }

    fn play_uri(&mut self, uri: &str) -> bool {
        self.ensure_active();
        let shuffle = self.shuffle_enabled();
        let collection = uri.starts_with("spotify:album:") || uri.starts_with("spotify:playlist:");
        let request = if collection {
            soloist::Request::PlayCollection {
                uri: uri.to_owned(),
                shuffle,
            }
        } else {
            soloist::Request::Send(soloist::command("play", json!({ "uri": uri })))
        };
        if !self.request(request) {
            return false;
        }
        self.shuffle_preference = Some(shuffle);
        self.queue_history.clear();
        self.deferred_skip = 0;
        self.restore_queue_start();
        self.context = None;
        self.refresh_queue_views();
        self.queue_retry_blocked = false;
        self.mark_manual_advance((!collection).then(|| uri.to_owned()));
        true
    }

    pub fn toggle(&mut self) {
        self.set_playing(!self.player.playing());
    }

    fn set_playing(&mut self, playing: bool) {
        if !playing && let Some(context) = &mut self.context {
            context.advance_on_load = false;
        }
        if playing
            && !self.queued.is_empty()
            && (self.player.item.is_none() || self.player.status == "idle")
        {
            self.start_queued(0);
            return;
        }
        self.ensure_active();
        self.send(if playing { "play" } else { "pause" }, Value::Null);
    }

    fn next(&mut self) {
        if self.skip_waiting() {
            self.deferred_skip = self.deferred_skip.saturating_add(1);
            return;
        }
        if !self.advance_followup() {
            self.context = None;
            self.refresh_queue_views();
            self.mark_manual_advance(None);
            self.send("skip_next", Value::Null);
        }
    }

    fn advance_followup(&mut self) -> bool {
        if self.queue_start.is_some() {
            return true;
        }
        if !self.queued.is_empty() {
            self.start_queued(0);
            return true;
        }
        let shuffle = self.shuffle_enabled();
        let Some(context) = self.context.as_mut() else {
            return false;
        };
        if context.upcoming.is_empty() {
            if context.next_page.is_some() {
                context.advance_on_load = true;
                return true;
            }
            if self.player.repeat == Repeat::Context && !context.tracks.is_empty() {
                context.played.clear();
                context.upcoming = (0..context.tracks.len()).collect();
                if shuffle {
                    context
                        .upcoming
                        .sort_by_cached_key(|_| getrandom::u64().unwrap_or_default());
                }
            }
        }
        if let Some(&index) = context.upcoming.first() {
            self.start_context_track(index);
            return true;
        }
        false
    }

    fn prev(&mut self) {
        if self.skip_waiting() {
            self.deferred_skip = self.deferred_skip.saturating_sub(1);
            return;
        }
        self.mark_manual_advance(None);
        if let Some(current) = &self.player.item
            && self
                .queue_history
                .last()
                .is_some_and(|entry| entry.uri == current.uri)
            && self.player.connected
            && self.player.logged_in
            && self.player.active
        {
            let current = current.clone();
            let Some(entry) = self.queue_history.pop() else {
                return;
            };
            match entry.origin {
                TrackOrigin::Queue { revision, .. } if revision == self.queue_revision => {
                    self.queued.insert(0, current);
                }
                TrackOrigin::Queue { .. } => {}
                TrackOrigin::Context(index) => {
                    let shuffle = self.shuffle_enabled();
                    if let (Some(context), Some(cursor)) =
                        (self.context.as_mut(), entry.from_cursor)
                    {
                        context.cursor = cursor;
                        context.played.remove(&index);
                        context.reorder(shuffle);
                        if shuffle
                            && let Some(pos) = context.upcoming.iter().position(|&i| i == index)
                        {
                            context.upcoming.remove(pos);
                            context.upcoming.insert(0, index);
                        }
                    }
                }
            }
            self.refresh_queue_views();
            self.mark_manual_advance(Some(entry.previous.uri.clone()));
            self.send("play", json!({ "uri": entry.previous.uri }));
        } else if self.player.connected {
            let _ = self.ws.send(soloist::Request::Previous);
        } else {
            self.error("soloist not connected");
        }
    }

    fn mark_manual_advance(&mut self, expected_uri: Option<String>) {
        self.manual_advance = Some(ManualAdvance {
            since: Instant::now(),
            expected_uri,
            refresh_requested: false,
        });
    }

    fn skip_waiting(&self) -> bool {
        self.queue_start.is_some()
            || self
                .context
                .as_ref()
                .is_some_and(|context| context.advance_on_load)
            || self.manual_advance.as_ref().is_some_and(|advance| {
                advance.since.elapsed() < Duration::from_secs(3)
                    && advance.expected_uri.as_deref().is_some_and(|expected| {
                        self.player
                            .item
                            .as_ref()
                            .is_none_or(|item| item.uri != expected)
                    })
            })
    }

    pub fn shuffle_enabled(&self) -> bool {
        self.shuffle_preference.unwrap_or(self.player.shuffle)
    }

    fn shuffle(&mut self, on: Option<bool>) {
        let on = on.unwrap_or(!self.shuffle_enabled());
        self.shuffle_preference = Some(on);
        self.send("set_shuffle", json!({ "enabled": on }));
        if let Some(context) = &mut self.context {
            context.reorder(on);
            self.refresh_queue_views();
        }
    }

    fn repeat(&mut self, mode: Option<Repeat>) {
        let mode = mode.unwrap_or(match self.player.repeat {
            Repeat::Off => Repeat::Context,
            Repeat::Context => Repeat::Track,
            Repeat::Track => Repeat::Off,
        });
        self.player.repeat = mode;
        let (first, second) = match mode {
            Repeat::Off => (("set_repeat_track", false), ("set_repeat_context", false)),
            Repeat::Context => (("set_repeat_track", false), ("set_repeat_context", true)),
            Repeat::Track => (("set_repeat_context", false), ("set_repeat_track", true)),
        };
        self.send(first.0, json!({ "enabled": first.1 }));
        self.send(second.0, json!({ "enabled": second.1 }));
    }

    fn volume(&mut self, v: i32) {
        let v = v.clamp(0, 100) as u8;
        self.player.volume = v;
        self.send("set_volume", json!({ "volume": v }));
    }

    fn seek(&mut self, ms: i64) {
        let d = self.player.duration();
        let ms = ms.max(0) as u64;
        let ms = if d > 0 {
            ms.min(d.saturating_sub(500))
        } else {
            ms
        };
        self.player.pos_ms = ms;
        self.player.pos_ts = crate::model::now_ms();
        self.send("seek", json!({ "position_ms": ms }));
    }

    fn seek_rel(&mut self, delta: i64) {
        let p = self.player.position() as i64;
        self.seek(p + delta);
    }

    /// ⏎ on the selected row.
    fn activate(&mut self) {
        if self.view().source == Source::Lyrics {
            let sel = self.view().sel;
            self.seek_to_line(sel);
            return;
        }
        let v = self.view();
        let Some(it) = v.selected().cloned() else {
            return;
        };
        let sel = v.sel;
        if v.source == Source::Queue {
            if sel < self.queued.len() {
                self.start_queued(sel);
                return;
            }
            if let Some(context) = &self.context {
                if let Some(&index) = context.upcoming.get(sel - self.queued.len()) {
                    self.start_context_track(index);
                }
                return;
            }
        }
        match it.kind {
            Kind::Track | Kind::Episode => {
                if matches!(
                    v.source,
                    Source::Album(_) | Source::Playlist(_) | Source::Liked
                ) {
                    self.play_context(sel);
                } else if self.play_uri(&it.uri) {
                    self.info(format!("▶ {}", it.name));
                }
            }
            Kind::Album | Kind::Playlist => {
                if self.play_uri(&it.uri) {
                    self.info(format!("▶ {}", it.name));
                }
            }
            Kind::Artist | Kind::Liked => self.open(&it, None),
        }
    }

    fn play_context(&mut self, index: usize) {
        let view = self.view();
        let Some(track) = view.items.get(index).cloned() else {
            return;
        };
        let source = view.source.clone();
        let name = view
            .header
            .as_ref()
            .map_or_else(|| view.title.clone(), |item| item.name.clone());
        let tracks = view.items.clone();
        let next_page = view.next;
        if !self.play_uri(&track.uri) {
            return;
        }
        self.next_id += 1;
        let mut context = PlaybackContext {
            id: self.next_id,
            source,
            name,
            tracks,
            cursor: index,
            upcoming: Vec::new(),
            played: HashSet::from([index]),
            next_page,
            advance_on_load: false,
        };
        context.reorder(self.shuffle_enabled());
        self.context = Some(context);
        self.refresh_queue_views();
        self.load_context_page();
        self.info(format!("▶ {}", track.name));
    }

    fn load_context_page(&self) {
        let Some(context) = &self.context else { return };
        let (Some(api), Some(offset)) = (self.api.clone(), context.next_page) else {
            return;
        };
        let (id, source, tx) = (context.id, context.source.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = api
                .fetch(&source, offset)
                .await
                .map(Box::new)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::ContextPage {
                context: id,
                result,
            });
        });
    }

    fn play_header(&mut self) {
        let Some(h) = self.view().header.clone().or_else(|| {
            // no header (e.g. the playlist grid): play the selected playlist/album
            self.view().selected().filter(|i| matches!(i.kind, Kind::Album | Kind::Playlist)).cloned()
        }) else {
            return;
        };
        if matches!(self.view().source, Source::Album(_) | Source::Playlist(_) | Source::Liked)
            && !self.view().items.is_empty()
        {
            let len = self.view().items.len();
            let index = if self.shuffle_enabled() {
                (getrandom::u64().unwrap_or_default() % len as u64) as usize
            } else {
                0
            };
            self.play_context(index);
            return;
        }
        match h.kind {
            Kind::Album | Kind::Playlist => {
                if self.play_uri(&h.uri) {
                    self.info(format!("▶ {}", h.name));
                }
            }
            _ => {
                self.select(0);
                self.activate();
            }
        }
    }

    fn enqueue_selected(&mut self) {
        let Some(it) = self.view().selected().cloned() else { return };
        match it.kind {
            Kind::Track => {
                self.info(format!("+ queued {}", it.name));
                self.queued.push(it);
                self.refresh_queue_views();
            }
            Kind::Album | Kind::Playlist | Kind::Liked => {
                // fetch its tracks, queue them all
                let Some(api) = self.api.clone() else { return };
                let src = match it.kind {
                    Kind::Album => Source::Album(it.id.clone()),
                    Kind::Playlist => Source::Playlist(it.id.clone()),
                    _ => Source::Liked,
                };
                let (tx, name) = (self.tx.clone(), it.name.clone());
                let revision = self.queue_revision;
                self.info(format!("queueing {name}…"));
                tokio::spawn(async move {
                    let items = api.fetch(&src, 0).await.map_or_else(
                        |_| Vec::new(),
                        |p| {
                            p.items
                                .into_iter()
                                .filter(|i| i.kind == Kind::Track)
                                .collect()
                        },
                    );
                    let _ = tx.send(Msg::Queued { items, name, revision });
                });
            }
            _ => self.error("can't queue that"),
        }
    }

    fn open_now_playing(&mut self) {
        match self.player.item.clone() {
            Some(it) => self.open(&it, None),
            None => self.error("nothing playing"),
        }
    }

    // ------------------------------------------------------------- setup --

    pub fn open_setup(&mut self) {
        let mut fields: [Input; 3] = Default::default();
        fields[0].set(self.cfg.client_id.as_deref().unwrap_or(""));
        fields[1].set(self.cfg.soloist.api_key.as_deref().unwrap_or(""));
        fields[2].set(self.cfg.soloist.device_name.as_deref().unwrap_or("str"));
        let focus = if fields[0].text.is_empty() { 0 } else if fields[1].text.is_empty() { 1 } else { 3 };
        self.help = false;
        self.mode = Mode::Normal;
        self.setup = Some(Setup { fields, focus, stage: SetupStage::Editing, reveal: false });
    }

    /// Browser login without leaving the TUI.
    pub fn start_login(&mut self) {
        if self.cfg.client_id.is_none() {
            self.open_setup();
            return;
        }
        if self.setup.is_none() {
            self.open_setup();
        }
        if let Some(s) = self.setup.as_mut() {
            s.stage = SetupStage::LoggingIn(None);
            s.focus = 3;
        }
        // a previous attempt may still hold the callback port
        if let Some(t) = self.login_task.take() {
            t.abort();
        }
        let (cfg, http, tx) = (self.cfg.clone(), self.http.clone(), self.tx.clone());
        self.login_task = Some(tokio::spawn(async move {
            let url_tx = tx.clone();
            let r = crate::auth::login(&cfg, &http, move |u| {
                let _ = url_tx.send(Msg::LoginUrl(u.to_string()));
            })
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Login(r));
        }));
    }

    fn save_setup(&mut self) {
        let Some(s) = self.setup.as_ref() else { return };
        let vals: Vec<String> = s.fields.iter().map(|f| f.text.trim().to_string()).collect();
        fn opt(v: &str) -> Option<&str> {
            (!v.is_empty()).then_some(v)
        }
        let res = crate::config::save_values(&[
            (None, "client_id", opt(&vals[0])),
            (Some("soloist"), "api_key", opt(&vals[1])),
            (Some("soloist"), "device_name", opt(&vals[2])),
        ])
        .and_then(|_| Config::load());
        let new = match res {
            Ok(c) => c,
            Err(e) => {
                if let Some(s) = self.setup.as_mut() {
                    s.stage = SetupStage::Error(format!("{e:#}"));
                }
                return;
            }
        };
        let client_changed = new.client_id != self.cfg.client_id;
        self.cfg = Arc::new(new);

        self.info(format!("saved to {}", crate::config::display_path()));

        // web api: (re)login when the client id is new or we have no session
        if self.cfg.client_id.is_some() && (client_changed || self.api.is_none()) {
            if client_changed {
                let _ = crate::auth::logout();
                self.api = None;
            }
            self.start_login();
        } else {
            self.setup = None;
        }
        // playback: start (or download) soloist if we now can
        self.ensure_soloist();
    }

    // ----------------------------------------------------------- soloist --

    /// Start Soloist if we have an API key and it isn't running; download it first if needed.
    pub fn ensure_soloist(&mut self) {
        if self.soloist_child.is_some() {
            return;
        }
        match soloist::maybe_spawn(&self.cfg) {
            Ok(soloist::Spawn::NotNeeded) => {}
            Ok(soloist::Spawn::Started(child)) => {
                self.info("starting soloist…");
                self.soloist_child = Some(child);
            }
            Ok(soloist::Spawn::Missing) => self.install_soloist(true),
            Err(e) => self.error(format!("couldn't start soloist: {e:#}")),
        }
    }

    pub fn install_soloist(&mut self, then_start: bool) {
        if self.installing.is_some() {
            return;
        }
        self.installing = Some(0);
        self.info("downloading soloist from Spotify…");
        let (http, tx) = (self.http.clone(), self.tx.clone());
        tokio::spawn(async move {
            let ptx = tx.clone();
            let result = crate::install::install(&http, move |p| {
                let _ = ptx.send(Msg::InstallProgress(p));
            })
            .await
            .map(|v| v.number().to_string())
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Installed { result, then_start });
        });
    }

    fn poll_soloist(&mut self) {
        let Some(child) = &mut self.soloist_child else {
            return;
        };
        let code = match child.try_wait() {
            Ok(None) => return,
            Ok(Some(status)) => status.code(),
            Err(e) => {
                self.error(format!("couldn't check Soloist: {e}"));
                return;
            }
        };
        self.soloist_child = None;
        match code {
            Some(0) | None => {}
            Some(10) => {
                let managed = crate::install::find_bin(&self.cfg)
                    .is_some_and(|b| crate::install::is_managed(&b));
                if managed && self.expired_retries < 1 {
                    self.expired_retries += 1;
                    self.info("soloist build expired — fetching a new one…");
                    self.install_soloist(true);
                } else {
                    self.error("your soloist build expired — download a newer one (or remove [soloist] bin so str manages it)");
                }
            }
            Some(c) => {
                let why = soloist::log_tail().unwrap_or_default();
                self.error(format!("soloist exited ({c}): {why}"));
            }
        }
    }

    /// Background check: is our own soloist close to expiring?
    pub fn check_update(&self) {
        let Some(bin) = crate::install::find_bin(&self.cfg).filter(|b| crate::install::is_managed(b)) else {
            return;
        };
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(d) = crate::install::version(&bin).and_then(|v| v.days_left())
                && d <= crate::install::UPDATE_WITHIN_DAYS
            {
                let _ = tx.send(Msg::NeedsUpdate(d));
            }
        });
    }

    fn restart_soloist(&mut self) {
        if let Err(e) = soloist::stop(&self.cfg) {
            self.error(format!("{e:#}"));
            return;
        }
        self.info("restarting soloist…");
        let (cfg, tx) = (self.cfg.clone(), self.tx.clone());
        tokio::spawn(async move {
            for _ in 0..50 {
                if !soloist::daemon_running(&cfg) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let _ = tx.send(Msg::StartSoloist);
        });
    }

    fn close_setup(&mut self) {
        self.setup = None;
    }

    fn key_setup(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(s) = self.setup.as_mut() else { return };
        match k.code {
            KeyCode::Esc => self.close_setup(),
            KeyCode::Tab | KeyCode::Down => s.focus = (s.focus + 1) % 5,
            KeyCode::BackTab | KeyCode::Up => s.focus = (s.focus + 4) % 5,
            KeyCode::Left | KeyCode::Right if s.focus >= 3 => s.focus = if s.focus == 3 { 4 } else { 3 },
            KeyCode::Enter if s.focus == 4 => self.close_setup(),
            KeyCode::Enter => self.save_setup(),
            KeyCode::Char('r') if ctrl => s.reveal = !s.reveal,
            KeyCode::Char('o') if ctrl => {
                if let SetupStage::LoggingIn(Some(u)) = &s.stage {
                    crate::auth::open_browser(u);
                }
            }
            _ if s.focus < 3
                && s.fields[s.focus].edit(&k)
                && !matches!(s.stage, SetupStage::LoggingIn(_)) =>
            {
                s.stage = SetupStage::Editing;
            }
            _ => {}
        }
    }

    fn paste_setup(&mut self, text: &str) {
        let Some(s) = self.setup.as_mut() else { return };
        if s.focus >= 3 {
            s.focus = 0;
        }
        // credentials never contain whitespace; paste often brings a newline
        let clean: String = if s.focus == 2 {
            text.trim().chars().filter(|c| !c.is_control()).collect()
        } else {
            text.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect()
        };
        let f = &mut s.fields[s.focus];
        let b = f.byte(f.cur);
        f.text.insert_str(b, &clean);
        f.cur += clean.chars().count();
    }

    // ------------------------------------------------------------ events --

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            #[cfg(target_os = "macos")]
            Msg::Media(action) => self.on_media_action(action),
            Msg::Soloist(soloist::Event::Up) => {
                self.player.connected = true;
                self.player.down_reason.clear();
            }
            Msg::Soloist(soloist::Event::Down(r)) => {
                self.player.connected = false;
                self.player.logged_in = false;
                self.player.queue.clear();
                self.queue_history.clear();
                self.deferred_skip = 0;
                self.player.down_reason = r;
                self.restore_queue_start();
                self.refresh_queue_views();
            }
            Msg::Soloist(soloist::Event::ControlError(e)) => self.error(e),
            Msg::Soloist(soloist::Event::Msg(v)) => self.on_player_event(v),
            Msg::Page { view, offset, result } => {
                let is_top = self.views.last().is_some_and(|v| v.id == view);
                let Some(v) = self.view_by_id(view) else { return };
                v.loading = false;
                match result {
                    Err(e) => {
                        v.next = None;
                        v.error = Some(e.clone());
                        if offset > 0 {
                            self.error(e);
                        }
                    }
                    Ok(p) => {
                        let p = *p;
                        if v.source == Source::Home {
                            v.next = p.next;
                            if offset == 0 {
                                self.home_items = p.items;
                            } else {
                                self.home_items.extend(p.items);
                            }
                            self.refresh_home_playlists();
                            self.load_remaining_sorted_playlists();
                            if is_top {
                                self.maybe_load_more();
                            }
                            return;
                        }
                        if offset == 0 {
                            v.items = p.items;
                            v.sel = 0;
                            v.scroll = 0;
                            if let Some(h) = p.header {
                                v.header = Some(h);
                            }
                            v.note = p.note;
                        } else {
                            v.items.extend(p.items);
                        }
                        v.next = p.next;
                        if let Some(f) = v.focus.take()
                            && let Some(pos) = v.items.iter().position(|it| it.uri == f)
                        {
                            v.sel = pos;
                        }
                        if is_top {
                            self.maybe_load_more();
                        }
                    }
                }
            }
            Msg::Image { url, img } => {
                self.pending_images.remove(&url);
                if let Some(img) = img {
                    if self.images.len() > 200 {
                        self.images.clear();
                    }
                    self.images.insert(url, img);
                }
            }
            Msg::LoginUrl(u) => {
                if let Some(s) = self.setup.as_mut() {
                    s.stage = SetupStage::LoggingIn(Some(u));
                }
            }
            Msg::Login(Ok(tok)) => {
                self.login_task = None;
                self.api = Some(Api::new(self.http.clone(), self.cfg.clone(), tok));
                self.api_problem = None;
                self.setup = None;
                self.query.set("");
                self.last_search.clear();
                self.go_home();
                self.info("✓ connected to Spotify");
            }
            Msg::Login(Err(e)) => {
                self.login_task = None;
                match self.setup.as_mut() {
                    Some(s) => s.stage = SetupStage::Error(e),
                    None => self.error(e),
                }
            }
            Msg::InstallProgress(p) => {
                self.installing = Some(p);
                self.info(format!("downloading soloist… {p}%"));
            }
            Msg::Installed { result, then_start } => {
                self.installing = None;
                match result {
                    Ok(v) => {
                        self.info(format!("✓ installed soloist {v}"));
                        if then_start {
                            self.ensure_soloist();
                        } else if self.player.connected {
                            self.info(format!("✓ soloist {v} installed — :restart to use it"));
                        }
                    }
                    Err(e) => self.error(format!("couldn't install soloist: {e}")),
                }
            }
            Msg::NeedsUpdate(days) => {
                self.info(format!("soloist expires in {days} days — updating…"));
                self.install_soloist(false);
            }
            Msg::StartSoloist => {
                self.poll_soloist();
                self.ensure_soloist();
            }
            Msg::Lyrics { uri, state } => {
                self.lyrics.insert(uri, state);
            }
            Msg::Queued { items, name, revision } => {
                if revision != self.queue_revision {
                    return;
                }
                if items.is_empty() {
                    self.error(format!("couldn't queue {name}"));
                } else {
                    self.info(format!("+ queued {} songs from {name}", items.len()));
                    self.queued.extend(items);
                    self.refresh_queue_views();
                }
            }
            Msg::ContextPage { context, result } => {
                let shuffle = self.shuffle_enabled();
                let Some(active) = self.context.as_mut().filter(|active| active.id == context) else {
                    return;
                };
                let advance = std::mem::take(&mut active.advance_on_load);
                match result {
                    Ok(page) => {
                        active.next_page = page.next.filter(|next| Some(*next) > active.next_page);
                        let first = active.tracks.len();
                        active.tracks.extend(page.items);
                        if shuffle {
                            active.reorder(true);
                        } else {
                            active.upcoming.extend(first..active.tracks.len());
                        }
                        self.refresh_queue_views();
                        self.load_context_page();
                        if advance {
                            self.next();
                            if !self.skip_waiting() {
                                self.flush_deferred_skip();
                            }
                        }
                    }
                    Err(e) => {
                        active.next_page = None;
                        self.deferred_skip = 0;
                        self.refresh_queue_views();
                        self.error(format!("Couldn’t load the rest of the collection: {e}"));
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn on_media_action(&mut self, action: crate::now_playing::Action) {
        use crate::now_playing::Action;
        if !self.player.connected || !self.player.logged_in || !self.player.active {
            return;
        }
        match action {
            Action::Play => self.set_playing(true),
            Action::Pause => self.set_playing(false),
            Action::Toggle => self.toggle(),
            Action::Next => self.next(),
            Action::Previous => self.prev(),
            Action::Seek(seconds) if seconds.is_finite() => self.seek((seconds * 1000.0) as i64),
            Action::Seek(_) => {}
            Action::Shuffle(on) => self.shuffle(Some(on)),
            Action::Repeat(mode) => self.repeat(Some(match mode.as_str() {
                "track" => Repeat::Track,
                "context" => Repeat::Context,
                _ => Repeat::Off,
            })),
        }
    }

    fn on_player_event(&mut self, event: Value) {
        let kind = event["type"].as_str().unwrap_or_default();
        let track_update = matches!(kind, "track_changed" | "playback_state");
        let next_uri = track_update
            .then(|| event["item"]["uri"].as_str())
            .flatten()
            .filter(|uri| !uri.is_empty());
        let track_changed =
            next_uri.is_some() && next_uri != self.player.item.as_ref().map(|i| i.uri.as_str());
        let expected_context_track = next_uri.is_some_and(|uri| {
            self.context
                .as_ref()
                .is_some_and(|context| context.tracks[context.cursor].uri == uri)
                || self
                    .queue_history
                    .last()
                    .is_some_and(|entry| entry.uri == uri)
        });
        let duration = self.player.duration();
        let was_playing = self.player.playing();
        let finishing =
            was_playing && duration > 0 && self.player.position().saturating_add(350) >= duration;
        let finishing_track = finishing.then(|| self.player.item.clone()).flatten();
        let was_starting = self.queue_start.is_some();
        match self.player.apply(&event) {
            Change::LoggedIn => {
                self.send("get_state", Value::Null);
                self.send("get_queue", json!({ "limit": 0 }));
            }
            Change::Queue => self.refresh_queue_views(),
            Change::Error(e) => self.error(e),
            Change::Other => {}
        }
        let start_confirmed = self.queue_start.as_ref().is_some_and(|start| {
            let fresh_track = start
                .from
                .as_ref()
                .is_none_or(|from| from.uri != start.track.uri);
            self.player
                .item
                .as_ref()
                .is_some_and(|i| i.uri == start.track.uri)
                && (kind == "track_changed"
                    || (kind == "playback_state" && (fresh_track || self.player.pos_ms < 1_000))
                    || (kind == "position_sync" && self.player.pos_ms < 1_000))
        });
        if start_confirmed && let Some(start) = self.queue_start.take() {
            let shuffle = self.shuffle_enabled();
            if let TrackOrigin::Context(index) = start.origin
                && let Some(context) = self.context.as_mut()
            {
                context.cursor = index;
                context.played.insert(index);
                context
                    .upcoming
                    .retain(|&i| i != index && (shuffle || i > index));
            }
            if let Some(previous) = start.from {
                self.queue_history.push(PlayedTrack {
                    uri: start.track.uri,
                    previous,
                    origin: start.origin,
                    from_cursor: start.from_cursor,
                });
            }
            self.refresh_queue_views();
        }
        let manually_changed = self
            .manual_advance
            .as_ref()
            .is_some_and(|advance| advance.since.elapsed() < Duration::from_secs(3));
        let manual_confirmed = next_uri.is_some_and(|uri| {
            self.manual_advance
                .as_ref()
                .is_some_and(|advance| match &advance.expected_uri {
                    Some(expected) => expected == uri,
                    None => track_changed,
                })
        });
        if manual_confirmed {
            self.manual_advance = None;
        }
        if track_changed {
            self.lyrics_manual = None;
            for view in self.views.iter_mut().filter(|v| v.source == Source::Lyrics) {
                view.sel = 0;
                view.scroll = 0;
            }
        }
        let reached_end = was_playing
            && !self.player.playing()
            && duration > 0
            && self.player.position() >= duration;
        if ((finishing
            && (track_changed || matches!(self.player.status.as_str(), "idle" | "stopped")))
            || reached_end)
            && !was_starting
            && !manually_changed
            && self.can_auto_advance()
        {
            self.advance_followup();
            if let (Some(start), Some(previous)) = (self.queue_start.as_mut(), finishing_track) {
                start.from = Some(previous);
            }
        } else if kind == "track_changed"
            && track_changed
            && !was_starting
            && !manually_changed
            && !expected_context_track
            && self.context.is_some()
        {
            self.context = None;
            self.refresh_queue_views();
        }
        if (start_confirmed || manual_confirmed) && !self.skip_waiting() {
            self.flush_deferred_skip();
        }
    }

    fn flush_deferred_skip(&mut self) {
        let skips = std::mem::take(&mut self.deferred_skip);
        for remaining in (0..skips.unsigned_abs()).rev() {
            if skips > 0 {
                self.next();
            } else {
                self.prev();
            }
            if self.skip_waiting() {
                self.deferred_skip = remaining as i8 * skips.signum();
                break;
            }
        }
    }

    fn can_auto_advance(&self) -> bool {
        (!self.queued.is_empty()
            || self.context.as_ref().is_some_and(|context| {
                !context.upcoming.is_empty()
                    || context.next_page.is_some()
                    || self.player.repeat == Repeat::Context
            }))
            && !self.queue_retry_blocked
            && self.player.connected
            && self.player.logged_in
            && self.player.active
            && self.player.repeat != Repeat::Track
    }

    pub fn on_tick(&mut self) {
        self.poll_soloist();
        if let Some(advance) = &mut self.manual_advance
            && advance.expected_uri.is_some()
        {
            if advance.since.elapsed() >= Duration::from_secs(3) {
                self.manual_advance = None;
                if self.queue_start.is_none() {
                    self.deferred_skip = 0;
                }
            } else if !advance.refresh_requested
                && advance.since.elapsed() >= Duration::from_millis(400)
            {
                advance.refresh_requested = true;
                self.send("get_state", Value::Null);
            }
        }
        if self
            .queue_start
            .as_ref()
            .is_some_and(|s| s.since.elapsed() >= Duration::from_secs(5))
        {
            self.restore_queue_start();
            self.deferred_skip = 0;
            if self.queue_retry_blocked {
                self.error("Soloist didn’t start the next song. Press n to retry.");
            }
        } else if let Some(start) = &mut self.queue_start
            && !start.refresh_requested
            && start.since.elapsed() >= Duration::from_millis(400)
        {
            start.refresh_requested = true;
            self.send("get_state", Value::Null);
        }
        let duration = self.player.duration();
        if self.can_auto_advance()
            && self
                .manual_advance
                .as_ref()
                .is_none_or(|advance| advance.since.elapsed() >= Duration::from_secs(3))
            && self.player.playing()
            && duration > 0
            && self.player.position() >= duration
        {
            self.advance_followup();
        }
        if self.search_due.is_some_and(|t| Instant::now() >= t) {
            self.run_search();
        }
        // the backdrop turns slowly while music plays (one turn ≈ 40s),
        // and crossfades to a new cover over ~1s
        if self.bg_mode == crate::art::BgMode::Spin && self.player.playing() {
            self.spin = (self.spin + std::f32::consts::TAU / 40.0 * 0.2) % std::f32::consts::TAU;
        }
        let (a, t) = (self.accent, self.accent_target);
        self.accent = (
            a.0 + (t.0 - a.0) * 0.3,
            a.1 + (t.1 - a.1) * 0.3,
            a.2 + (t.2 - a.2) * 0.3,
        );
        if self.backdrop_mix < 1.0 {
            self.backdrop_mix = (self.backdrop_mix + 0.2).min(1.0);
            if self.backdrop_mix >= 1.0 {
                self.backdrop_old = None;
            }
        }
        if self
            .msg
            .as_ref()
            .is_some_and(|(_, err, t)| t.elapsed() > Duration::from_secs(if *err { 6 } else { 3 }))
        {
            self.msg = None;
        }
    }

    /// Look up the current track for the player preview and lyrics page.
    pub fn sync_lyrics(&mut self) {
        if !self.lyrics_enabled() {
            return;
        }
        let Some(it) = self.player.item.as_ref().filter(|i| {
            i.kind == Kind::Track && !i.uri.is_empty() && !i.name.is_empty() && !i.sub.is_empty()
        }) else {
            return;
        };
        if self.lyrics.contains_key(&it.uri) {
            return;
        }
        if self.lyrics.len() >= 64 {
            self.lyrics.clear();
        }
        let (uri, name, artist, album) = (
            it.uri.clone(),
            it.name.clone(),
            it.sub.clone(),
            it.extra.clone(),
        );
        let dur = it.duration_ms.unwrap_or_default();
        self.lyrics
            .insert(uri.clone(), crate::lyrics::State::Loading);
        let (http, tx) = (self.http.clone(), self.tx.clone());
        tokio::spawn(async move {
            let state = crate::lyrics::fetch(&http, &name, &artist, &album, dur)
                .await
                .unwrap_or_else(|e| crate::lyrics::State::Failed(format!("{e:#}")));
            let _ = tx.send(Msg::Lyrics { uri, state });
        });
    }

    pub fn lyrics_enabled(&self) -> bool {
        self.cfg.lyrics != Some(false)
    }

    /// Lyrics state of the playing song (None: nothing playing / turned off).
    pub fn lyrics_state(&self) -> Option<&crate::lyrics::State> {
        if !self.lyrics_enabled() {
            return None;
        }
        self.lyrics.get(&self.player.item.as_ref()?.uri)
    }

    pub fn current_lyrics(&self) -> Option<&crate::lyrics::Lyrics> {
        match self.lyrics_state()? {
            crate::lyrics::State::Ready(l) => Some(l),
            _ => None,
        }
    }

    /// Open the lyrics page (or close it if it's open).
    fn toggle_lyrics(&mut self) {
        if self.view().source == Source::Lyrics {
            self.back();
            return;
        }
        if !self.lyrics_enabled() {
            self.info("lyrics are off: set lyrics = true in the config");
            return;
        }
        // retry a lookup that failed (e.g. offline)
        if let Some(uri) = self.player.item.as_ref().map(|i| i.uri.clone())
            && matches!(self.lyrics.get(&uri), Some(crate::lyrics::State::Failed(_)))
        {
            self.lyrics.remove(&uri);
        }
        self.views.retain(|v| v.source != Source::Lyrics);
        let v = self.new_view("Lyrics", Source::Lyrics);
        self.views.push(v);
        self.lyrics_manual = None;
    }

    pub fn sync_lyrics_selection(&mut self, current: Option<usize>, synced: bool) {
        if synced
            && self
                .lyrics_manual
                .is_some_and(|t| t.elapsed() >= Duration::from_secs(5))
        {
            self.lyrics_manual = None;
        }
        if self.lyrics_manual.is_none() {
            self.view_mut().sel = current.unwrap_or_default();
        }
    }

    fn seek_to_line(&mut self, i: usize) {
        let Some(at) = self
            .current_lyrics()
            .filter(|l| l.synced)
            .and_then(|l| l.lines.get(i))
            .map(|l| l.at_ms)
        else {
            return;
        };
        self.seek(at as i64);
        self.lyrics_manual = None;
    }

    /// Make sure the covers we want to show are loaded (call before drawing).
    pub fn sync_art(&mut self) {
        let want = [
            self.player.item.as_ref().and_then(|i| i.image.clone()),
            self.view().header.as_ref().and_then(|h| h.image.clone()),
        ];
        for (i, w) in want.into_iter().enumerate() {
            match w {
                None => self.shown[i] = None,
                Some(u) if self.images.contains_key(&u) => self.shown[i] = Some(u),
                // keep showing the previous cover until this one arrives
                Some(u) => self.fetch_image(u),
            }
        }
        // player backdrop: the blurred cover, crossfading from the last one
        if self.shown[0].as_ref() != self.backdrop.as_ref().map(|b| &b.0) {
            let new = self.shown[0]
                .as_ref()
                .and_then(|u| self.images.get(u).map(|img| (u.clone(), crate::art::Backdrop::new(img))));
            self.accent_target = self.shown[0]
                .as_ref()
                .and_then(|u| self.images.get(u))
                .map_or(crate::ui::DEFAULT_ACCENT, crate::art::accent);
            let first = self.backdrop.is_none();
            self.backdrop_old = std::mem::replace(&mut self.backdrop, new).map(|b| b.1);
            self.backdrop_mix = if first { 1.0 } else { 0.0 };
        }
    }

    /// Text-art rendering of a loaded cover at a given size (cached).
    pub fn rendered_art(&mut self, url: &str, w: u16, h: u16) -> Option<&crate::art::Rendered> {
        let key = (url.to_string(), w, h, self.art);
        if !self.art_cache.contains_key(&key) {
            let img = self.images.get(url)?;
            let r = crate::art::render(img, self.art, w, h);
            if self.art_cache.len() >= 160 {
                self.art_cache.clear();
            }
            self.art_cache.insert(key.clone(), r);
        }
        self.art_cache.get(&key)
    }

    pub fn thumbnail(&mut self, url: &str, w: u16, h: u16) -> Option<&Thumbnail> {
        let key = (url.to_owned(), w, h, self.art);
        if !self.thumbnails.contains_key(&key) {
            let image = self.images.get(url)?;
            let bitmap = (self.picker.protocol_type() != ProtocolType::Halfblocks)
                .then(|| {
                    let font = self.picker.font_size();
                    let image = crate::art::bitmap(
                        image,
                        u32::from(w) * u32::from(font.width),
                        u32::from(h) * u32::from(font.height),
                        self.art,
                    );
                    self.picker.new_protocol(image, (w, h).into(), Resize::default()).ok()
                })
                .flatten();
            let thumb = bitmap.map_or_else(
                || Thumbnail::Text(crate::art::render_thumbnail(image, w, h)),
                Thumbnail::Bitmap,
            );
            if self.thumbnails.len() >= 160 {
                self.thumbnails.clear();
            }
            self.thumbnails.insert(key.clone(), thumb);
        }
        self.thumbnails.get(&key)
    }

    pub fn uses_cover_bitmaps(&self) -> bool {
        self.art != crate::art::Style::Dots
            && self.picker.protocol_type() != ProtocolType::Halfblocks
    }

    pub fn track_digit(&mut self, digit: u8, playing: bool) -> Option<&Protocol> {
        if self.picker.protocol_type() != ProtocolType::Kitty {
            return None;
        }
        let digit = usize::from(digit);
        let index = digit + if playing { 10 } else { 0 };
        let slot = &mut self.track_digits[index];
        if slot.is_none() {
            let font = self.picker.font_size();
            if font.width == 0 || font.height == 0 {
                return None;
            }
            let color = if playing { [232; 3] } else { [105; 3] };
            let bitmap = crate::art::track_digit(digit, font.width.into(), font.height.into(), color);
            *slot = self.picker.new_protocol(bitmap, (1, 2).into(), Resize::default()).ok();
        }
        slot.as_ref()
    }

    pub fn cycle_art(&mut self) {
        self.art = self.art.next();
        self.info(format!("art: {}", self.art.name()));
    }

    pub fn fetch_image(&mut self, url: String) {
        if !self.pending_images.insert(url.clone()) {
            return;
        }
        let (http, tx) = (self.http.clone(), self.tx.clone());
        tokio::spawn(async move {
            let bytes = match http.get(&url).send().await {
                Ok(r) if r.status().is_success() => r.bytes().await.ok(),
                _ => None,
            };
            let img = match bytes {
                // keep memory small: covers never need more than ~320px here
                Some(b) => tokio::task::spawn_blocking(move || image::load_from_memory(&b).ok().map(|i| i.thumbnail(320, 320)))
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let _ = tx.send(Msg::Image { url, img });
        });
    }

    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(s) if self.setup.is_some() => self.paste_setup(&s),
            Event::Paste(s) if self.mode == Mode::Search => {
                for c in s.chars().filter(|c| !c.is_control()) {
                    self.query.edit(&KeyEvent::from(KeyCode::Char(c)));
                }
                self.search_due = Some(Instant::now() + Duration::from_millis(300));
            }
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.setup.is_some() {
            self.key_setup(k);
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        match self.mode {
            Mode::Normal => self.key_normal(k),
            Mode::Search => self.key_search(k),
            Mode::Command => self.key_command(k),
        }
    }

    fn key_normal(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let g = std::mem::take(&mut self.pending_g);
        // playlists page: move around the cover grid, ⏎ opens one
        if self.view().source == Source::Home && !ctrl {
            let cols = self.lib_cols.max(1) as isize;
            let sel = self.view().sel as isize;
            let handled = match k.code {
                KeyCode::Enter => {
                    self.open_selected();
                    true
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_sel(cols);
                    true
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_sel(-cols);
                    true
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    self.move_sel(1);
                    true
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    if sel > 0 {
                        self.move_sel(-1);
                    }
                    true
                }
                _ => false,
            };
            if handled {
                return;
            }
        }
        let half = (self.list_height / 2).max(1) as isize;
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('/') | KeyCode::Char('i') => {
                self.mode = Mode::Search;
                self.query.cur = self.query.len();
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.cmd.set("");
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('S') => self.open_setup(),
            KeyCode::Char('A') => self.cycle_art(),
            KeyCode::Char('L') => self.toggle_lyrics(),
            KeyCode::Esc if self.view().source == Source::Lyrics => self.back(),
            KeyCode::Esc => self.msg = None,

            KeyCode::Char('K') | KeyCode::Up
                if self.view().source == Source::Queue
                    && (k.code == KeyCode::Char('K')
                        || k.modifiers.contains(KeyModifiers::SHIFT)) =>
            {
                self.move_queue_item(-1)
            }
            KeyCode::Char('J') | KeyCode::Down
                if self.view().source == Source::Queue
                    && (k.code == KeyCode::Char('J')
                        || k.modifiers.contains(KeyModifiers::SHIFT)) =>
            {
                self.move_queue_item(1)
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('d') if ctrl => self.move_sel(half),
            KeyCode::Char('u') if ctrl => self.move_sel(-half),
            KeyCode::Char('f') if ctrl => self.move_sel(half * 2),
            KeyCode::Char('b') if ctrl => self.move_sel(-half * 2),
            KeyCode::PageDown => self.move_sel(half * 2),
            KeyCode::PageUp => self.move_sel(-half * 2),
            KeyCode::Char('g') if g => self.select(0),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => self.select(usize::MAX),

            KeyCode::Enter => self.activate(),
            KeyCode::Char('l') | KeyCode::Right => self.open_selected(),
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => self.back(),
            KeyCode::Char('H') => self.go_home_clear(),
            KeyCode::Tab | KeyCode::Char('w') => self.open_queue(),
            KeyCode::Char('o') => self.open_now_playing(),
            KeyCode::Char('P') => self.play_header(),

            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('n') => self.next(),
            KeyCode::Char('p') => self.prev(),
            KeyCode::Char('s') => self.shuffle(None),
            KeyCode::Char('r') => self.repeat(None),
            KeyCode::Char('+') | KeyCode::Char('=') => self.volume(self.player.volume as i32 + 5),
            KeyCode::Char('-') | KeyCode::Char('_') => self.volume(self.player.volume as i32 - 5),
            KeyCode::Char('m') => match self.muted_from.take() {
                Some(v) => self.volume(v as i32),
                None => {
                    self.muted_from = Some(self.player.volume);
                    self.volume(0);
                }
            },
            KeyCode::Char(']') | KeyCode::Char('.') => self.seek_rel(10_000),
            KeyCode::Char('[') | KeyCode::Char(',') => self.seek_rel(-10_000),
            KeyCode::Char('a') => self.enqueue_selected(),
            _ => {}
        }
    }

    fn key_search(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.run_search();
            }
            KeyCode::Down => self.move_sel(1),
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('n') | KeyCode::Char('j') if ctrl => self.move_sel(1),
            KeyCode::Char('p') | KeyCode::Char('k') if ctrl => self.move_sel(-1),
            KeyCode::Tab => self.cycle_filter(),
            _ => {
                if self.query.edit(&k) {
                    let delay = if self.query.text.trim().is_empty() { 0 } else { 350 };
                    self.search_due = Some(Instant::now() + Duration::from_millis(delay));
                }
            }
        }
    }

    fn key_command(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Backspace if self.cmd.text.is_empty() => self.mode = Mode::Normal,
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                let c = std::mem::take(&mut self.cmd.text);
                self.run_command(c.trim());
            }
            _ => {
                self.cmd.edit(&k);
            }
        }
    }

    pub fn run_command(&mut self, c: &str) {
        let (name, arg) = c.split_once(' ').map_or((c, ""), |(a, b)| (a, b.trim()));
        let onoff = |s: &str| match s {
            "on" | "true" | "1" | "yes" => Some(true),
            "off" | "false" | "0" | "no" => Some(false),
            _ => None,
        };
        match name {
            "" => {}
            "q" | "quit" | "q!" | "qa" | "wq" | "x" => self.quit = true,
            "stop" => match soloist::stop(&self.cfg) {
                Ok(()) => self.info("stopped soloist"),
                Err(e) => self.error(format!("{e:#}")),
            },
            "play" | "pl" if !arg.is_empty() => {
                self.play_uri(arg);
            }
            "play" | "pl" | "pause" | "toggle" | "pp" => self.toggle(),
            "next" | "n" | "skip" => self.next(),
            "prev" | "previous" | "p" | "back" => self.prev(),
            "vol" | "volume" | "v" => {
                let cur = self.player.volume as i32;
                match (arg.strip_prefix('+'), arg.strip_prefix('-')) {
                    (Some(d), _) => self.volume(cur + d.parse::<i32>().unwrap_or(5)),
                    (_, Some(d)) => self.volume(cur - d.parse::<i32>().unwrap_or(5)),
                    _ => match arg.parse::<i32>() {
                        Ok(v) => self.volume(v),
                        Err(_) => self.info(format!("volume {cur}%")),
                    },
                }
            }
            "shuffle" | "sh" => self.shuffle(onoff(arg)),
            "repeat" | "rep" => self.repeat(match arg {
                "off" | "none" => Some(Repeat::Off),
                "context" | "all" | "on" => Some(Repeat::Context),
                "track" | "one" | "song" => Some(Repeat::Track),
                _ => None,
            }),
            "seek" => {
                let (sign, rest) = match arg.chars().next() {
                    Some('+') => (1, &arg[1..]),
                    Some('-') => (-1, &arg[1..]),
                    _ => (0, arg),
                };
                let secs: Option<i64> = match rest.split_once(':') {
                    Some((m, s)) => m.parse::<i64>().ok().zip(s.parse::<i64>().ok()).map(|(m, s)| m * 60 + s),
                    None => rest.parse().ok(),
                };
                match secs {
                    Some(s) if sign == 0 => self.seek(s * 1000),
                    Some(s) => self.seek_rel(sign * s * 1000),
                    None => self.error("usage: :seek 1:23 | +10 | -10"),
                }
            }
            "queue" | "qu" if arg == "clear" => self.clear_queue(),
            "clearqueue" => self.clear_queue(),
            "queue" | "qu" => self.open_queue(),
            "lyrics" | "ly" | "lyr" => self.toggle_lyrics(),
            "pin" => self.pin_playlist(None),
            "unpin" => self.pin_playlist(Some(false)),
            "sort" => self.sort_playlists(arg),
            "add" => self.enqueue_selected(),
            "home" | "lib" | "library" | "playlists" | "lists" => self.go_home_clear(),
            "open" => self.open_now_playing(),
            "s" | "search" | "find" => {
                self.query.set(arg);
                self.run_search();
            }
            "help" | "h" => self.help = true,
            "setup" | "config" => self.open_setup(),
            "install" | "update" => self.install_soloist(!self.player.connected),
            "restart" => self.restart_soloist(),
            "bg" | "background" => {
                self.bg_mode = match arg {
                    "" => self.bg_mode.next(),
                    a => crate::art::BgMode::parse(a),
                };
                self.info(format!("background: {}", self.bg_mode.name()));
            }
            "art" => {
                if arg.is_empty() {
                    self.cycle_art();
                } else {
                    self.art = crate::art::Style::parse(arg);
                    self.info(format!("art: {}", self.art.name()));
                }
            }
            "login" => self.start_login(),
            "activate" => {
                self.send("activate", Value::Null);
            }
            other => self.error(format!("unknown command: {other}  (try :help)")),
        }
    }

    // ------------------------------------------------------------- mouse --

    fn hit_at(&self, x: u16, y: u16) -> Option<(Rect, Hit)> {
        self.hits
            .iter()
            .rev()
            .find(|(r, _)| r.contains(Position { x, y }))
            .cloned()
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        match m.kind {
            MouseEventKind::Moved => self.pointer = Some(Position { x, y }),
            MouseEventKind::Down(btn) => {
                self.pointer = Some(Position { x, y });
                if self.help {
                    self.help = false;
                    return;
                }
                let dbl = self
                    .last_click
                    .is_some_and(|(t, cx, cy)| t.elapsed() < Duration::from_millis(400) && cx == x && cy == y);
                self.last_click = Some((Instant::now(), x, y));
                let Some((r, hit)) = self.hit_at(x, y) else { return };
                if let Some(s) = self.setup.as_mut() {
                    match hit {
                        Hit::SetupField(i) => s.focus = i,
                        Hit::SetupSave => self.save_setup(),
                        Hit::SetupCancel => self.close_setup(),
                        _ => {}
                    }
                    return;
                }
                if self.mode != Mode::Normal && !matches!(hit, Hit::SearchBar) {
                    self.mode = Mode::Normal;
                }
                match hit {
                    Hit::Row(i) => {
                        self.select(i);
                        self.pointer = Some(Position { x, y });
                        let kind = self.view().selected().map(|s| s.kind);
                        let play_song = self.view().source != Source::Queue
                            && matches!(kind, Some(Kind::Track | Kind::Episode));
                        if btn == MouseButton::Right || (btn == MouseButton::Left && play_song) {
                            self.activate();
                            self.last_click = None;
                        } else if btn == MouseButton::Left && self.view().source == Source::Home {
                            self.open_selected();
                            self.last_click = None;
                        } else if dbl {
                            match kind {
                                Some(k) if k.is_container() => self.open_selected(),
                                _ => self.activate(),
                            }
                            self.last_click = None;
                        }
                    }
                    Hit::Shuffle => self.shuffle(None),
                    Hit::Prev => self.prev(),
                    Hit::Toggle => self.toggle(),
                    Hit::Next => self.next(),
                    Hit::QueueMove(delta) => self.move_queue_item(delta),
                    Hit::QueueClear => self.clear_queue(),
                    Hit::Repeat => self.repeat(None),
                    Hit::Progress => {
                        let f = (x.saturating_sub(r.x)) as f64 / r.width.max(1) as f64;
                        self.seek((f * self.player.duration() as f64) as i64);
                    }
                    Hit::Volume => {
                        let f = (x.saturating_sub(r.x) + 1) as f64 / r.width.max(1) as f64;
                        self.volume((f * 100.0).round() as i32);
                    }
                    Hit::SearchBar => {
                        self.mode = Mode::Search;
                        self.query.cur = self.query.len();
                    }
                    Hit::Crumb(i) => self.views.truncate(i + 1),
                    Hit::PlayHeader => self.play_header(),
                    Hit::Lyrics => self.toggle_lyrics(),
                    Hit::PlaylistSort => self.sort_playlists(""),
                    Hit::LyricLine(i) => {
                        self.select(i);
                        self.seek_to_line(i);
                    }
                    Hit::OpenSetup => self.open_setup(),
                    Hit::List | Hit::SetupField(_) | Hit::SetupSave | Hit::SetupCancel => {}
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if self.setup.is_none() => {
                let up = m.kind == MouseEventKind::ScrollUp;
                match self.hit_at(x, y).map(|h| h.1) {
                    Some(Hit::Volume) => self.volume(self.player.volume as i32 + if up { 5 } else { -5 }),
                    Some(Hit::Progress) => self.seek_rel(if up { 5000 } else { -5000 }),
                    _ => self.move_sel(if up { -3 } else { 3 }),
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::UnboundedReceiver;

    fn connected_app() -> (App, UnboundedReceiver<soloist::Request>) {
        let (tx, _messages) = tokio::sync::mpsc::unbounded_channel();
        let (ws, commands) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            Arc::new(Config::default()),
            None,
            None,
            reqwest::Client::new(),
            tx,
            ws,
            Picker::halfblocks(),
        );
        app.player.connected = true;
        app.player.logged_in = true;
        app.player.active = true;
        (app, commands)
    }

    #[test]
    fn bitmap_thumbnail_fits_the_unchanged_slot() {
        let (mut app, _) = connected_app();
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize(ratatui_image::FontSize::new(8, 16));
        picker.set_protocol_type(ProtocolType::Kitty);
        app.picker = picker;
        app.images.insert("cover".into(), DynamicImage::new_rgb8(64, 64));
        let Some(Thumbnail::Bitmap(image)) = app.thumbnail("cover", 4, 2) else {
            panic!("expected terminal graphics for the thumbnail");
        };
        assert_eq!(image.size(), (4, 2).into());
        assert_eq!(app.track_digit(1, false).unwrap().size(), (1, 2).into());
    }

    #[test]
    fn clicking_the_last_visible_song_or_lyric_keeps_its_position() {
        let (mut app, _) = connected_app();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30))
            .expect("test terminal");
        let mut playlist = app.new_view("Playlist", Source::Playlist("test".into()));
        playlist.items = (0..50)
            .map(|i| {
                Item::new(
                    Kind::Track,
                    format!("spotify:track:{i}"),
                    format!("Song {i}"),
                )
            })
            .collect();
        app.player.item = playlist.items.first().cloned();
        app.push(playlist);

        for source in [Source::Playlist("test".into()), Source::Lyrics] {
            if source == Source::Lyrics {
                app.toggle_lyrics();
                app.lyrics.insert(
                    "spotify:track:0".into(),
                    crate::lyrics::State::Ready(crate::lyrics::Lyrics {
                        synced: true,
                        lines: (0..50)
                            .map(|i| crate::lyrics::LyricLine {
                                at_ms: i * 2_000,
                                text: format!("Lyric {i}"),
                            })
                            .collect(),
                    }),
                );
            }
            let row_index = |hit: &Hit| match hit {
                Hit::Row(i) if source != Source::Lyrics => Some(*i),
                Hit::LyricLine(i) if source == Source::Lyrics => Some(*i),
                _ => None,
            };
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let (area, index) = app
                .hits
                .iter()
                .rev()
                .find_map(|(area, hit)| row_index(hit).map(|i| (*area, i)))
                .unwrap();
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            });
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            assert_eq!(app.view().sel, index);
            assert!(
                app.hits
                    .iter()
                    .any(|(r, hit)| *r == area && row_index(hit) == Some(index))
            );
        }
    }

    #[test]
    fn clicking_first_song_and_playing_the_album_are_distinct() {
        let (mut app, mut commands) = connected_app();
        let album = "spotify:album:album";
        let track = "spotify:track:first";
        let mut view = app.new_view("Album", Source::Album("album".into()));
        view.header = Some(Item::new(Kind::Album, album, "Album"));
        view.items = vec![
            Item::new(Kind::Track, track, "First"),
            Item::new(Kind::Track, "spotify:track:second", "Second"),
        ];
        app.push(view);
        app.player.shuffle = true;
        app.hits.push((Rect::new(0, 0, 10, 1), Hit::Row(0)));
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            commands.try_recv().unwrap(),
            soloist::Request::Send(play) if play == soloist::command("play", json!({"uri": track}))
        ));
        assert_eq!(app.queued_len(), 0);
        assert_eq!(app.context.as_ref().unwrap().upcoming, vec![1]);
        app.enqueue_selected();
        app.on_player_event(json!({"type": "options_changed", "options": {"shuffle": false}}));
        assert!(app.shuffle_enabled());
        app.play_header();
        assert!(matches!(
            commands.try_recv().unwrap(),
            soloist::Request::Send(play) if play["command"] == "play" && play["uri"] != album
        ));
        assert_eq!(app.queued_len(), 1);
        assert_eq!(app.queued[0].uri, track);

        app.shuffle(None);
        assert!(matches!(
            commands.try_recv().unwrap(),
            soloist::Request::Send(set) if set == soloist::command("set_shuffle", json!({"enabled": false}))
        ));
        app.play_header();
        assert!(matches!(
            commands.try_recv().unwrap(),
            soloist::Request::Send(play) if play == soloist::command("play", json!({"uri": track}))
        ));
    }

    #[test]
    fn queue_advances_to_the_song_moved_to_the_front() {
        let (mut app, mut commands) = connected_app();
        app.player.status = "playing".into();
        let mut current = Item::new(Kind::Track, "spotify:track:current", "Current");
        current.duration_ms = Some(2_000);
        app.player.item = Some(current);
        app.player.pos_ms = 2_000;
        app.queued = vec![
            Item::new(Kind::Track, "spotify:track:first", "First"),
            Item::new(Kind::Track, "spotify:track:second", "Second"),
        ];
        app.open_queue();
        let _ = commands.try_recv().unwrap(); // get_queue
        app.select(1);
        app.on_key(KeyCode::Char('K').into());
        assert_eq!(app.view().sel, 0);
        assert_eq!(app.view().items[0].name, "Second");

        app.on_tick();
        let soloist::Request::Send(play) = commands.try_recv().unwrap() else {
            panic!("expected queue playback");
        };
        assert_eq!(play, soloist::command("play", json!({"uri": "spotify:track:second"})));
        app.on_player_event(json!({
            "type": "track_changed",
            "item": {"uri": "spotify:track:second", "entity_type": "track"}
        }));
        assert_eq!(app.queued[0].name, "First");
        assert!(app.queue_start.is_none());

        app.prev();
        let soloist::Request::Send(play) = commands.try_recv().unwrap() else {
            panic!("expected playback of the previous song");
        };
        assert_eq!(play, soloist::command("play", json!({"uri": "spotify:track:current"})));
        assert_eq!(app.queued[0].uri, "spotify:track:second");
    }

    #[test]
    fn playlist_resumes_after_explicit_queue_and_search_leaves_the_context() {
        let (mut app, mut commands) = connected_app();
        let mut view = app.new_view("Playlist", Source::Playlist("test".into()));
        view.items = (0..4)
            .map(|i| {
                Item::new(
                    Kind::Track,
                    format!("spotify:track:{i}"),
                    format!("Song {i}"),
                )
            })
            .collect();
        app.push(view);
        app.select(1);
        app.activate();
        let expect_play = |commands: &mut UnboundedReceiver<soloist::Request>, uri| {
            assert!(matches!(
                commands.try_recv().unwrap(),
                soloist::Request::Send(play) if play == soloist::command("play", json!({"uri": uri}))
            ));
        };
        let track_changed = |app: &mut App, uri| {
            app.on_player_event(json!({
                "type": "track_changed",
                "item": {"uri": uri, "entity_type": "track"}
            }));
        };
        expect_play(&mut commands, "spotify:track:1");
        track_changed(&mut app, "spotify:track:1");
        assert_eq!(app.queued_len(), 0);
        assert_eq!(app.context.as_ref().unwrap().upcoming, vec![2, 3]);

        let queued = Item::new(Kind::Track, "spotify:track:queued", "Queued");
        app.queued.push(queued);
        app.next();
        expect_play(&mut commands, "spotify:track:queued");
        track_changed(&mut app, "spotify:track:queued");
        app.next();
        expect_play(&mut commands, "spotify:track:2");
        track_changed(&mut app, "spotify:track:2");
        assert_eq!(app.context.as_ref().unwrap().upcoming, vec![3]);
        assert_eq!(app.queued_len(), 0);

        app.shuffle(Some(true));
        let _ = commands.try_recv().unwrap(); // set_shuffle
        let upcoming: HashSet<_> = app
            .context
            .as_ref()
            .unwrap()
            .upcoming
            .iter()
            .copied()
            .collect();
        assert_eq!(upcoming, HashSet::from([0, 3]));

        let mut search = app.new_view(
            "Search",
            Source::Search {
                kinds: vec![Kind::Track],
                text: "test".into(),
            },
        );
        search
            .items
            .push(Item::new(Kind::Track, "spotify:track:search", "Result"));
        app.replace_root(search);
        app.activate();
        expect_play(&mut commands, "spotify:track:search");
        assert!(app.context.is_none());
        track_changed(&mut app, "spotify:track:search");
        app.next();
        assert!(matches!(
            commands.try_recv().unwrap(),
            soloist::Request::Send(skip) if skip == soloist::command("skip_next", Value::Null)
        ));
    }

    #[test]
    fn buffering_snapshot_does_not_drop_the_selected_playlist() {
        let (mut app, mut commands) = connected_app();
        let mut view = app.new_view("Playlist", Source::Playlist("test".into()));
        view.items = (0..4)
            .map(|i| {
                Item::new(
                    Kind::Track,
                    format!("spotify:track:{i}"),
                    format!("Song {i}"),
                )
            })
            .collect();
        app.player.item = view.items.first().cloned();
        app.push(view);
        app.select(1);
        app.activate();
        let _ = commands.try_recv().expect("play the selected song");

        app.on_player_event(json!({"type": "playback_state", "status": "buffering", "item": null}));
        app.on_player_event(json!({
            "type": "track_changed",
            "item": {"uri": "spotify:track:1", "entity_type": "track"}
        }));
        app.next();
        assert!(matches!(
            commands.try_recv().unwrap(),
            soloist::Request::Send(play) if play == soloist::command("play", json!({"uri": "spotify:track:2"}))
        ));
    }

    #[test]
    fn clearing_the_queue_keeps_context_and_ignores_old_queue_additions() {
        let (mut app, mut commands) = connected_app();
        let mut view = app.new_view("Album", Source::Album("test".into()));
        view.items = vec![
            Item::new(Kind::Track, "spotify:track:first", "First"),
            Item::new(Kind::Track, "spotify:track:second", "Second"),
        ];
        app.push(view);
        app.activate();
        let _ = commands.try_recv().expect("play first track");
        let queued = Item::new(Kind::Track, "spotify:track:queued", "Queued");
        app.queued.extend([queued.clone(), queued.clone()]);
        app.start_queued(0);
        let _ = commands.try_recv().expect("start queued track");
        let revision = app.queue_revision;
        app.run_command("clearqueue");
        app.restore_queue_start();
        app.on_msg(Msg::Queued {
            items: vec![queued],
            name: "Old request".into(),
            revision,
        });
        assert!(app.queued.is_empty());
        assert!(!app.queue_retry_blocked);

        app.next();
        assert!(matches!(
            commands.try_recv().unwrap(),
            soloist::Request::Send(play) if play == soloist::command("play", json!({"uri": "spotify:track:second"}))
        ));
    }

    #[test]
    fn rapid_next_presses_are_kept_until_the_pending_track_confirms() {
        let (mut app, mut commands) = connected_app();
        let mut view = app.new_view("Album", Source::Album("test".into()));
        view.items = (0..4)
            .map(|i| {
                Item::new(
                    Kind::Track,
                    format!("spotify:track:{i}"),
                    format!("Song {i}"),
                )
            })
            .collect();
        app.player.item = view.items.first().cloned();
        app.push(view);
        app.activate();
        let _ = commands.try_recv().unwrap();

        app.next();
        app.next();
        assert!(
            matches!(commands.try_recv().unwrap(), soloist::Request::Send(play) if play["uri"] == "spotify:track:1")
        );
        app.queue_start.as_mut().unwrap().since -= Duration::from_millis(500);
        app.on_tick();
        assert!(
            matches!(commands.try_recv().unwrap(), soloist::Request::Send(query) if query["command"] == "get_state")
        );
        app.on_tick();
        assert!(commands.try_recv().is_err());
        app.on_player_event(json!({
            "type": "playback_state", "status": "playing",
            "item": {"uri": "spotify:track:1", "entity_type": "track"},
            "position": {"position_ms": 1_200, "speed": 1}
        }));
        assert!(
            matches!(commands.try_recv().unwrap(), soloist::Request::Send(play) if play["uri"] == "spotify:track:2")
        );
    }

    #[test]
    fn previous_bypasses_a_track_only_started_between_rapid_skips() {
        let (mut app, mut commands) = connected_app();
        let mut view = app.new_view("Album", Source::Album("test".into()));
        view.items = (0..3)
            .map(|i| Item::new(Kind::Track, format!("spotify:track:{i}"), format!("Song {i}")))
            .collect();
        app.player.item = view.items.first().cloned();
        app.push(view);
        app.activate();
        let _ = commands.try_recv().unwrap();

        app.next();
        app.next();
        let _ = commands.try_recv().unwrap();
        for track in ["spotify:track:1", "spotify:track:2"] {
            app.on_player_event(json!({
                "type": "track_changed",
                "item": {"uri": track, "entity_type": "track"}
            }));
        }
        let _ = commands.try_recv().unwrap();
        app.prev();
        assert!(matches!(
            commands.try_recv().unwrap(),
            soloist::Request::Send(play) if play["uri"] == "spotify:track:0"
        ));
    }

    #[test]
    fn previous_from_a_selected_album_track_stays_in_the_album() {
        let (mut app, mut commands) = connected_app();
        let mut view = app.new_view("Album", Source::Album("test".into()));
        view.items = vec![
            Item::new(Kind::Track, "spotify:track:first", "First"),
            Item::new(Kind::Track, "spotify:track:second", "Second"),
        ];
        app.player.item = view.items.last().cloned();
        app.push(view);
        app.select(1);
        app.activate();
        let _ = commands.try_recv().unwrap();
        app.prev();
        assert!(matches!(
            commands.try_recv().unwrap(),
            soloist::Request::Send(play) if play["uri"] == "spotify:track:first"
        ));
    }
}
