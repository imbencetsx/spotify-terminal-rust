//! Rendering. Everything is drawn from `App` each frame; clickable regions are
//! recorded into `app.hits` as we go.

use crate::app::{App, Hit, Mode, SETUP_LABELS, Setup, SetupStage, Thumbnail, View};
use crate::art::{BgMode as ArtBg, Style as ArtStyle};
use crate::lyrics;
use crate::model::{Item, Kind, Source, fmt_ms, parse_query};
use crate::soloist::Repeat;
use image::imageops::FilterType;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui_image::{Resize, StatefulImage};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Default accent (Spotify green), used when nothing is playing.
pub const DEFAULT_ACCENT: (f32, f32, f32) = (30.0, 215.0, 96.0);

/// The accent follows the playing song; set once per frame from the app.
static ACCENT_RGB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x1ED760);

fn accent() -> Color {
    let v = ACCENT_RGB.load(std::sync::atomic::Ordering::Relaxed);
    Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// Text colour on top of the accent: true black, or white if the accent is dark.
/// (Never the theme's "black", which many themes render as grey.)
fn on_accent() -> Color {
    let Color::Rgb(r, g, b) = accent() else { return Color::Rgb(0, 0, 0) };
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let l = 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
    // pick whichever contrasts more
    if (l + 0.05) / 0.05 >= 1.05 / (l + 0.05) { Color::Rgb(0, 0, 0) } else { Color::Rgb(255, 255, 255) }
}

fn set_accent((r, g, b): (f32, f32, f32)) {
    let v = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
    ACCENT_RGB.store(v, std::sync::atomic::Ordering::Relaxed);
}
/// The app's own background, so it looks the same on any terminal theme.
const BG: Color = Color::Rgb(18, 18, 18);
/// Pop-ups (help, setup) sit a step above it.
const CARD_BG: Color = Color::Rgb(26, 26, 26);
const FG: Color = Color::Rgb(232, 232, 232);
const MUTED: Color = Color::Rgb(165, 165, 165);
const DIM: Color = Color::Rgb(105, 105, 105);
const FAINT: Color = Color::Rgb(58, 58, 58);
const SEL_BG: Color = Color::Rgb(40, 40, 40);
const RED: Color = Color::Rgb(240, 95, 95);
const YELLOW: Color = Color::Rgb(255, 196, 92);
const BLUE: Color = Color::Rgb(110, 170, 255);

fn tag_color(k: Kind) -> Color {
    match k {
        Kind::Track => BLUE,
        Kind::Episode => RED,
        Kind::Album => Color::Rgb(196, 140, 255),
        Kind::Artist => YELLOW,
        Kind::Playlist => accent(),
        Kind::Liked => Color::Rgb(255, 110, 160),
    }
}

fn sw(s: &str) -> u16 {
    s.width() as u16
}

/// Truncate to `max` columns with an ellipsis.
fn trunc(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

fn pad(s: &str, width: usize) -> String {
    let t = trunc(s, width);
    let n = width.saturating_sub(t.width());
    t + &" ".repeat(n)
}

fn span_at(f: &mut Frame, x: u16, y: u16, s: Span) {
    let w = sw(&s.content);
    f.render_widget(s, Rect::new(x, y, w, 1));
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hits.clear();
    set_accent(app.accent);
    let area = f.area();
    f.render_widget(Block::new().style(Style::new().bg(BG).fg(FG)), area);
    let [main, _, composer_r, footer_r] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);

    if area.width >= 80 && main.height >= 14 {
        let lw = (area.width * 36 / 100).clamp(32, 60);
        let [left, sep, right] = Layout::horizontal([
            Constraint::Length(lw),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(main);
        player(f, app, left, true);
        f.render_widget(
            Block::new().borders(Borders::LEFT).border_style(Style::new().fg(FAINT)),
            sep.inner(Margin::new(0, 1)),
        );
        list(f, app, right);
    } else {
        let ph = 4.min(main.height / 2);
        let [top, bottom] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(ph)]).areas(main);
        list(f, app, top);
        player(f, app, bottom, false);
    }
    composer(f, app, composer_r);
    footer(f, app, footer_r);
    if app.help {
        help(f, area);
    }
    if app.setup.is_some() {
        setup(f, app, area);
    }
}

// ------------------------------------------------------------- player ----

/// Colours for the player card. Over the blurred-cover backdrop, text is
/// white blended against each row's average colour; without one, the usual
/// greys on the terminal background.
struct Pal {
    /// Average backdrop colour of each row (empty = no backdrop).
    rows: Vec<(f32, f32, f32)>,
    y0: u16,
}

impl Pal {
    fn plain() -> Pal {
        Pal { rows: Vec::new(), y0: 0 }
    }
    fn bg_rgb(&self, y: u16) -> Option<(f32, f32, f32)> {
        y.checked_sub(self.y0).and_then(|i| self.rows.get(i as usize)).copied()
    }
    /// White at opacity `a` over this row's background (or `plain` with no backdrop).
    fn fg(&self, y: u16, a: f32, plain: Color) -> Color {
        match self.bg_rgb(y) {
            None => plain,
            Some((r, g, b)) => {
                let mix = |c: f32| (c + (255.0 - c) * a) as u8;
                Color::Rgb(mix(r), mix(g), mix(b))
            }
        }
    }
    fn text(&self, y: u16) -> Color {
        self.fg(y, 0.96, FG)
    }
    fn muted(&self, y: u16) -> Color {
        self.fg(y, 0.82, MUTED)
    }
    fn dim(&self, y: u16) -> Color {
        self.fg(y, 0.64, DIM)
    }
    fn faint(&self, y: u16) -> Color {
        self.fg(y, 0.3, FAINT)
    }
    /// A soft drop shadow right/below `r`, so the cover lifts off the gradient.
    fn shadow(&self, f: &mut Frame, r: Rect, clip: Rect) {
        if self.rows.is_empty() {
            return;
        }
        let buf = f.buffer_mut();
        let mut set = |x: u16, y: u16| {
            if clip.contains(Position { x, y })
                && let Some(c) = buf.cell_mut((x, y))
                && let Color::Rgb(rr, gg, bb) = c.bg
            {
                let d = |v: u8| (v as f32 * 0.4) as u8;
                c.set_bg(Color::Rgb(d(rr), d(gg), d(bb)));
            }
        };
        for y in r.top() + 1..=r.bottom() {
            set(r.right(), y);
        }
        for x in r.left() + 1..r.right() {
            set(x, r.bottom());
        }
    }
}

fn cover_placeholder(f: &mut Frame, r: Rect) {
    f.render_widget(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(FAINT)),
        r,
    );
    if r.height >= 3 {
        let mid = Rect::new(r.x, r.y + r.height / 2, r.width, 1);
        f.render_widget(Line::from("♪".fg(DIM)).alignment(Alignment::Center), mid);
    }
}

/// Draw the cover for `slot` (0 = now playing, 1 = list header) into `r`.
fn cover(f: &mut Frame, app: &mut App, r: Rect, slot: usize) {
    let Some(url) = app.shown[slot].clone() else {
        cover_placeholder(f, r);
        return;
    };
    if app.art == ArtStyle::Image
        && let Some(a) = app.protos[slot].as_mut()
    {
        f.render_stateful_widget(
            StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle))),
            r,
            &mut a.proto,
        );
        return;
    }
    match app.rendered_art(&url, r.width, r.height) {
        Some(art) => art.draw(r, f.buffer_mut()),
        None => cover_placeholder(f, r),
    }
}

/// Size (cols, rows) of a square image that fits in `max_w` × `max_h` cells.
fn square(app: &App, max_w: u16, max_h: u16) -> (u16, u16) {
    let fs = app.picker.font_size();
    let (fw, fh) = (fs.width.max(1) as u32, fs.height.max(1) as u32);
    let mut cw = max_w;
    let mut ch = (cw as u32 * fw / fh) as u16;
    if ch > max_h {
        ch = max_h;
        cw = ((ch as u32 * fh) / fw).min(max_w as u32) as u16;
    }
    (cw, ch)
}

/// Paint the blurred, turning cover behind the player. Returns the palette
/// text should use on top of it.
fn backdrop(f: &mut Frame, app: &App, card: Rect) -> Pal {
    let Some((_, new)) = app.backdrop.as_ref().filter(|_| app.bg_mode != ArtBg::Off) else {
        return Pal::plain();
    };
    let old = app.backdrop_old.as_ref();
    let fs = app.picker.font_size();
    let (fw, fh) = (fs.width.max(1) as f32, fs.height.max(1) as f32);
    // map cells to a square texture space centred on the card, in pixels
    let (pw, ph) = (card.width as f32 * fw, card.height as f32 * fh);
    let side = pw.max(ph).max(1.0);
    let (angle, mix) = (app.spin, app.backdrop_mix);
    let buf = f.buffer_mut();
    let mut rows = Vec::with_capacity(card.height as usize);
    for dy in 0..card.height {
        // darker toward the bottom so controls and text always read
        let t = dy as f32 / card.height.max(1) as f32;
        let dark = 0.74 - 0.46 * t;
        let v = ((dy as f32 + 0.5) * fh - ph / 2.0) / side;
        let mut acc = (0.0, 0.0, 0.0);
        for dx in 0..card.width {
            let u = ((dx as f32 + 0.5) * fw - pw / 2.0) / side;
            let mut c = new.sample(u, v, angle);
            if let Some(o) = old {
                let a = o.sample(u, v, angle);
                c = (a.0 + (c.0 - a.0) * mix, a.1 + (c.1 - a.1) * mix, a.2 + (c.2 - a.2) * mix);
            }
            let mut c = (c.0 * dark, c.1 * dark, c.2 * dark);
            // never brighter than `cap` (keeps the hue): light covers turn into a
            // deep tone instead of grey, so the cover and white text stand out
            let cap = 118.0 - 70.0 * t;
            let m = c.0.max(c.1).max(c.2);
            if m > cap {
                let k = cap / m;
                c = (c.0 * k, c.1 * k, c.2 * k);
            }
            if let Some(cell) = buf.cell_mut((card.x + dx, card.y + dy)) {
                cell.set_bg(Color::Rgb(c.0 as u8, c.1 as u8, c.2 as u8));
            }
            acc = (acc.0 + c.0, acc.1 + c.1, acc.2 + c.2);
        }
        let n = card.width.max(1) as f32;
        rows.push((acc.0 / n, acc.1 / n, acc.2 / n));
    }
    Pal { rows, y0: card.y }
}

fn player(f: &mut Frame, app: &mut App, area: Rect, full: bool) {
    // the player sits on a card tinted with the cover's colour
    let card = if full {
        Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(1))
    } else {
        area
    };
    let pal = backdrop(f, app, card);
    let inner = if full { card.inner(Margin::new(2, 1)) } else { card.inner(Margin::new(2, 0)) };
    if inner.width < 12 || inner.height < 2 {
        return;
    }
    let mut y = inner.y;

    if full {
        // Leave room for the shadow, controls and the wrapped lyrics preview.
        let reserved = if app.lyrics_enabled() {
            let preview_rows = lyrics_preview_lines(app).iter().fold(0u16, |rows, text| {
                rows.saturating_add(lyric_preview_rows(*text, inner.width))
            });
            13u16.saturating_add(preview_rows.max(4))
        } else {
            14
        };
        let (cw, ch) = square(app, inner.width.saturating_sub(1), inner.height.saturating_sub(reserved));
        if ch >= 4 {
            let r = Rect::new(inner.x + (inner.width - cw - 1) / 2, y, cw, ch);
            cover(f, app, r, 0);
            pal.shadow(f, r, card);
            y += ch + 2; // cover, shadow row, blank line
        }
    }

    let row = |y: u16| Rect::new(inner.x, y, inner.width, 1);
    let center = |l: Line<'static>| l.alignment(Alignment::Center);
    let width = inner.width as usize;
    let p = &app.player;

    if full {
        let (title, artist, album) = match &p.item {
            Some(it) => (it.name.clone(), it.sub.clone(), it.extra.clone()),
            None => ("nothing playing".into(), "press / to search".into(), String::new()),
        };
        let title_style = if p.item.is_some() {
            Style::new().fg(pal.text(y)).bold()
        } else {
            Style::new().fg(pal.dim(y))
        };
        f.render_widget(center(Line::styled(trunc(&title, width), title_style)), row(y));
        f.render_widget(
            center(Line::styled(trunc(&artist, width), Style::new().fg(pal.muted(y + 1)))),
            row(y + 1),
        );
        f.render_widget(
            center(Line::styled(trunc(&album, width), Style::new().fg(pal.dim(y + 2)).italic())),
            row(y + 2),
        );
        y += 4;
        progress(f, app, row(y), &pal);
        y += 2;
        controls(f, app, row(y), &pal);
        y += 2;
        volume(f, app, row(y), &pal);
        if app.lyrics_enabled() && y + 2 < inner.bottom() {
            lyrics_button(f, app, row(y + 2), &pal);
            let preview_y = y + 3;
            let preview = Rect::new(inner.x, preview_y, inner.width, inner.bottom().saturating_sub(preview_y));
            lyrics_preview(f, app, preview, &pal);
        }
    } else {
        // compact: title · artist / progress / controls + volume
        let line = match &p.item {
            Some(it) => Line::from(vec![
                Span::styled(it.name.clone(), Style::new().fg(pal.text(y)).bold()),
                Span::styled(format!("  {}", it.sub), Style::new().fg(pal.muted(y))),
            ]),
            None => Line::styled("nothing playing", Style::new().fg(pal.dim(y))),
        };
        let title_width = if app.lyrics_enabled() && inner.width >= 28 {
            inner.width - 12
        } else {
            inner.width
        };
        f.render_widget(line, Rect::new(inner.x, y, title_width, 1));
        if title_width < inner.width {
            lyrics_button(f, app, Rect::new(inner.x + title_width, y, 12, 1), &pal);
        }
        if inner.height >= 3 {
            progress(f, app, row(y + 1), &pal);
            controls(f, app, row(y + 2), &pal);
        } else {
            controls(f, app, row(y + 1), &pal);
        }
    }
}

fn lyrics_button(f: &mut Frame, app: &mut App, r: Rect, pal: &Pal) {
    let label = " Lyrics L ";
    let width = sw(label);
    let x = r.x + r.width.saturating_sub(width) / 2;
    let active = app.view().source == Source::Lyrics;
    let style = if active {
        Style::new().fg(on_accent()).bg(accent()).bold()
    } else {
        Style::new().fg(pal.muted(r.y))
    };
    span_at(f, x, r.y, Span::styled(label, style));
    app.hits.push((Rect::new(x, r.y, width, 1), Hit::Lyrics));
}

fn lyrics_preview_lines(app: &App) -> [Option<&str>; 4] {
    let mut preview = [None; 4];
    match app.lyrics_state() {
        Some(lyrics::State::Ready(lyrics)) if lyrics.synced => {
            let current = lyrics.current(app.player.position());
            let next = current.map_or(0, |i| i + 1);
            if let Some(i) = current {
                preview[0] = lyrics.lines[..i]
                    .iter()
                    .rev()
                    .find(|line| !line.text.is_empty())
                    .map(|line| line.text.as_str());
                preview[1] = Some(lyrics.lines[i].text.as_str());
            }
            for (slot, line) in preview[2..].iter_mut().zip(
                lyrics.lines[next..]
                    .iter()
                    .filter(|line| !line.text.is_empty()),
            ) {
                *slot = Some(line.text.as_str());
            }
        }
        Some(lyrics::State::Loading) => preview[0] = Some("Loading lyrics…"),
        Some(lyrics::State::Ready(_)) => preview[0] = Some("Open Lyrics to read"),
        Some(lyrics::State::Failed(_)) => preview[0] = Some("Open Lyrics to retry"),
        _ => {}
    }
    preview
}

fn lyric_preview_paragraph(text: &str) -> Paragraph<'_> {
    Paragraph::new(text).alignment(Alignment::Center).wrap(Wrap { trim: true })
}

fn lyric_preview_rows(text: Option<&str>, width: u16) -> u16 {
    text.map_or(1, |text| {
        lyric_preview_paragraph(text).line_count(width).clamp(1, u16::MAX as usize) as u16
    })
}

fn lyrics_preview(f: &mut Frame, app: &mut App, area: Rect, pal: &Pal) {
    let mut y = area.y;
    let mut has_text = false;
    for (i, text) in lyrics_preview_lines(app).into_iter().enumerate() {
        if y >= area.bottom() {
            break;
        }
        let height = lyric_preview_rows(text, area.width).min(area.bottom() - y);
        let row = Rect::new(area.x, y, area.width, height);
        y += height;
        let Some(text) = text else { continue };
        let y = row.y;
        let style = match i {
            1 => Style::new().fg(pal.text(y)).bold(),
            2 => Style::new().fg(pal.fg(y, 0.58, MUTED)),
            3 => Style::new().fg(pal.fg(y, 0.44, DIM)),
            _ => Style::new().fg(pal.fg(y, 0.35, Color::Rgb(92, 92, 92))),
        };
        f.render_widget(lyric_preview_paragraph(text).style(style), row);
        has_text = true;
    }
    if has_text {
        app.hits.push((Rect::new(area.x, area.y, area.width, y - area.y), Hit::Lyrics));
    }
}

fn progress(f: &mut Frame, app: &mut App, r: Rect, pal: &Pal) {
    let p = &app.player;
    let (pos, dur) = (p.position(), p.duration());
    let l = fmt_ms(pos);
    let rt = if dur > 0 { fmt_ms(dur) } else { "-:--".into() };
    let bw = r.width.saturating_sub(sw(&l) + sw(&rt) + 2);
    if bw < 4 {
        return;
    }
    let filled = if dur > 0 { ((bw as f64) * pos as f64 / dur as f64).round() as u16 } else { 0 }.min(bw);
    let y = r.y;
    let fill_color = if p.playing() { accent() } else { pal.muted(y) };
    let mut spans = vec![Span::styled(format!("{l} "), Style::new().fg(pal.muted(y)))];
    if dur > 0 {
        spans.push(Span::styled("━".repeat(filled.saturating_sub(1) as usize), Style::new().fg(fill_color)));
        spans.push(Span::styled("●", Style::new().fg(pal.text(y))));
        spans.push(Span::styled("─".repeat((bw - filled.max(1)) as usize), Style::new().fg(pal.faint(y))));
    } else {
        spans.push(Span::styled("─".repeat(bw as usize), Style::new().fg(pal.faint(y))));
    }
    spans.push(Span::styled(format!(" {rt}"), Style::new().fg(pal.muted(y))));
    f.render_widget(Line::from(spans), r);
    app.hits.push((Rect::new(r.x + sw(&l) + 1, r.y, bw, 1), Hit::Progress));
}

fn controls(f: &mut Frame, app: &mut App, r: Rect, pal: &Pal) {
    let p = &app.player;
    let y = r.y;
    let on = |b: bool| Style::new().fg(if b { accent() } else { pal.dim(y) });
    let toggle = if p.playing() { " ⏸ " } else { " ▶ " };
    let btns: [(Hit, &str, Style); 5] = [
        (Hit::Shuffle, "⇄", on(app.shuffle_enabled())),
        (Hit::Prev, "⏮", Style::new().fg(pal.text(y))),
        (Hit::Toggle, toggle, Style::new().fg(on_accent()).bg(accent()).bold()),
        (Hit::Next, "⏭", Style::new().fg(pal.text(y))),
        (
            Hit::Repeat,
            if p.repeat == Repeat::Track { "↻¹" } else { "↻" },
            on(p.repeat != Repeat::Off),
        ),
    ];
    let gap: u16 = if r.width >= 34 { 5 } else { 3 };
    let total: u16 = btns.iter().map(|b| sw(b.1)).sum::<u16>() + gap * (btns.len() as u16 - 1);
    if total > r.width {
        return;
    }
    let mut x = r.x + (r.width - total) / 2;
    for (hit, label, style) in btns {
        let w = sw(label);
        span_at(f, x, r.y, Span::styled(label, style));
        app.hits.push((Rect::new(x.saturating_sub(1), r.y, w + 2, 1), hit));
        x += w + gap;
    }
}

fn volume(f: &mut Frame, app: &mut App, r: Rect, pal: &Pal) {
    let v = app.player.volume;
    let bw = r.width.saturating_sub(10).min(22);
    if bw < 5 {
        return;
    }
    let label = if v == 0 { "mute" } else { "vol " };
    let total = 4 + 1 + bw + 5;
    let x = r.x + r.width.saturating_sub(total) / 2;
    let filled = (bw as u32 * v as u32 / 100) as u16;
    let line = Line::from(vec![
        Span::styled(format!("{label} "), Style::new().fg(pal.dim(r.y))),
        Span::styled("━".repeat(filled as usize), Style::new().fg(pal.muted(r.y))),
        Span::styled("─".repeat((bw - filled) as usize), Style::new().fg(pal.faint(r.y))),
        Span::styled(format!(" {v:>3}%"), Style::new().fg(pal.dim(r.y))),
    ]);
    f.render_widget(line, Rect::new(x, r.y, total.min(r.width), 1));
    app.hits.push((Rect::new(x + 5, r.y, bw, 1), Hit::Volume));
}

// --------------------------------------------------------------- list ----

fn top(app: &mut App) -> &mut View {
    app.views.last_mut().expect("view stack is never empty")
}

fn list(f: &mut Frame, app: &mut App, area: Rect) {
    let area = area.inner(Margin::new(2, 1));
    if area.width < 10 || area.height < 3 {
        return;
    }
    let y = crumbs(f, app, area);
    let body = Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y));
    if body.height == 0 {
        return;
    }
    app.hits.push((body, Hit::List));
    app.lib_cols = 1;

    // your playlists: a full page of covers; everything else: header + rows
    let v = app.view();
    if v.source == Source::Lyrics {
        lyrics_page(f, app, body);
    } else if v.source == Source::Home && !v.items.is_empty() {
        playlists_grid(f, app, body);
    } else {
        pane(f, app, body);
    }
}

fn lyrics_page(f: &mut Frame, app: &mut App, area: Rect) {
    let (message, style) = match app.lyrics_state() {
        Some(lyrics::State::Ready(_)) => ("", Style::new()),
        Some(lyrics::State::Loading) => ("Loading lyrics…", Style::new().fg(MUTED)),
        Some(lyrics::State::Instrumental) => ("Instrumental track", Style::new().fg(MUTED)),
        Some(lyrics::State::Missing) => ("No lyrics found for this track", Style::new().fg(MUTED)),
        Some(lyrics::State::Failed(error)) => {
            let content = format!("Couldn’t load lyrics. Reopen Lyrics to retry.\n{error}");
            f.render_widget(
                Paragraph::new(content)
                    .style(Style::new().fg(RED))
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        None => {
            let message = match app.player.item.as_ref().map(|i| i.kind) {
                None => "Play a song to see its lyrics",
                Some(Kind::Track) => "Waiting for track details…",
                _ => "Lyrics are available for songs",
            };
            (message, Style::new().fg(MUTED))
        }
    };
    let Some(lyrics) = app.current_lyrics() else {
        f.render_widget(Line::styled(message, style), area);
        return;
    };
    let synced = lyrics.synced;
    let current = lyrics.current(app.player.position());
    app.sync_lyrics_selection(current, synced);
    let selected = app.view().sel;
    let manual = app.lyrics_manual.is_some();
    let hint = match (synced, manual) {
        (true, true) => "Following paused  ·  Enter or click to seek",
        (true, false) => "Synced  ·  Enter or click to seek",
        (false, _) => "Unsynced  ·  j/k or scroll to read",
    };
    f.render_widget(
        Line::styled(hint, Style::new().fg(DIM)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let area = Rect::new(
        area.x,
        area.y + 1,
        area.width.min(86),
        area.height.saturating_sub(1),
    );
    if area.height == 0 {
        return;
    }
    app.list_height = area.height as usize;
    let Some(lyrics) = app.current_lyrics() else {
        return;
    };
    let gutter = if synced { 8 } else { 2 };
    let text_width = area.width.saturating_sub(gutter);
    let paragraph = |i: usize| {
        let style = if current == Some(i) {
            Style::new().fg(accent()).bold()
        } else {
            Style::new().fg(if manual && i == selected { FG } else { MUTED })
        };
        Paragraph::new(lyrics.lines[i].text.as_str())
            .style(style)
            .wrap(Wrap { trim: true })
    };
    // Keep the active line near the middle and account for wrapped lines.
    let mut first = selected;
    let mut above = 0;
    while first > 0 {
        let height = paragraph(first - 1).line_count(text_width).max(1);
        if above + height > area.height as usize / 2 {
            break;
        }
        first -= 1;
        above += height;
    }
    let mut y = area.y;
    let mut hits = Vec::new();
    for (i, line) in lyrics.lines.iter().enumerate().skip(first) {
        let text = paragraph(i);
        let height = text
            .line_count(text_width)
            .max(1)
            .min((area.bottom() - y) as usize) as u16;
        let row = Rect::new(area.x, y, area.width, height);
        let cursor = current == Some(i) || (manual && i == selected);
        if cursor {
            f.render_widget(Block::new().style(Style::new().bg(SEL_BG)), row);
            span_at(f, row.x, y, Span::styled("▎", Style::new().fg(accent())));
        }
        if synced {
            f.render_widget(
                Line::styled(fmt_ms(line.at_ms), Style::new().fg(DIM)),
                Rect::new(row.x + 2, y, gutter - 3, 1),
            );
        }
        f.render_widget(text, Rect::new(row.x + gutter, y, text_width, height));
        hits.push((row, Hit::LyricLine(i)));
        y += height;
        if y == area.bottom() {
            break;
        }
    }
    app.hits.extend(hits);
}

/// Breadcrumbs + item count. Returns the y where content starts.
fn crumbs(f: &mut Frame, app: &mut App, area: Rect) -> u16 {
    let y = area.y;
    let mut x = area.x;
    let n = app.views.len();
    for i in 0..n {
        let last = i + 1 == n;
        let t = trunc(&app.views[i].title, 28);
        let st = if last { Style::new().fg(FG).bold() } else { Style::new().fg(DIM) };
        let w = sw(&t);
        if x + w > area.right() {
            break;
        }
        span_at(f, x, y, Span::styled(t, st));
        app.hits.push((Rect::new(x, y, w, 1), Hit::Crumb(i)));
        x += w;
        if !last {
            span_at(f, x, y, Span::styled(" › ", Style::new().fg(FAINT)));
            x += 3;
        }
    }
    let v = app.view();
    let right = if v.loading {
        "loading…".to_string()
    } else if v.items.is_empty() {
        String::new()
    } else {
        format!("{}{}", v.items.len(), if v.next.is_some() { "+" } else { "" })
    };
    if !right.is_empty() && x + sw(&right) + 2 < area.right() {
        f.render_widget(
            Line::styled(right, Style::new().fg(DIM)).alignment(Alignment::Right),
            Rect::new(area.x, y, area.width, 1),
        );
    }
    y + 2
}

/// A view's header (cover + info) and its rows.
fn pane(f: &mut Frame, app: &mut App, area: Rect) {
    let mut y = area.y;
    if app.view().source == Source::Queue && !app.view().items.is_empty() && area.height >= 3 {
        let local = app.queued_len();
        let sel = app.view().sel;
        let mut x = area.x;
        for (label, delta, enabled) in [
            (" ↑ K ", -1, sel > 0 && sel < local),
            (" ↓ J ", 1, sel + 1 < local),
        ] {
            let style = if enabled {
                Style::new().fg(FG).bg(SEL_BG)
            } else {
                Style::new().fg(DIM)
            };
            let r = Rect::new(x, y, sw(label), 1).intersection(area);
            f.render_widget(Line::styled(label, style), r);
            if enabled {
                app.hits.push((r, Hit::QueueMove(delta)));
            }
            x += sw(label) + 1;
        }
        let label = if sel < local {
            "Move queued song"
        } else {
            "Spotify upcoming • read-only"
        };
        let r = Rect::new(x, y, area.right().saturating_sub(x), 1);
        f.render_widget(Line::styled(label, Style::new().fg(DIM)), r);
        y += 2;
    }
    if let Some(h) = app.view().header.clone()
        && area.height > 12
    {
        y += header_block(f, app, Rect::new(area.x, y, area.width, 7), &h);
    }
    let body = Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y));
    if body.height == 0 {
        return;
    }
    app.list_height = body.height as usize;
    rows(f, app, body);
}

/// Cover + name + play chip for an album / playlist / artist. Returns rows used.
fn header_block(f: &mut Frame, app: &mut App, r: Rect, h: &Item) -> u16 {
    let (cw, ch) = square(app, 30, r.height);
    let cr = Rect::new(r.x, r.y, cw, ch);
    if h.kind == Kind::Liked {
        liked_cover(f, cr);
    } else {
        cover(f, app, cr, 1);
    }
    let tx = r.x + cw + 3;
    let tw = r.right().saturating_sub(tx) as usize;
    let label = match h.kind {
        Kind::Album => "ALBUM",
        Kind::Playlist => "PLAYLIST",
        Kind::Artist => "ARTIST",
        Kind::Liked => "LIBRARY",
        _ => "",
    };
    let at = |dy: u16| Rect::new(tx, r.y + dy, tw as u16, 1);
    f.render_widget(Line::styled(label, Style::new().fg(tag_color(h.kind)).bold()), at(0));
    f.render_widget(Line::styled(trunc(&h.name, tw), Style::new().fg(FG).bold()), at(1));
    f.render_widget(Line::styled(trunc(&h.sub, tw), Style::new().fg(MUTED)), at(2));
    f.render_widget(Line::styled(trunc(&h.extra, tw), Style::new().fg(DIM)), at(3));
    if h.kind != Kind::Artist && r.height >= 6 {
        let chip = " ▶ play ";
        span_at(f, tx, r.y + 5, Span::styled(chip, Style::new().fg(on_accent()).bg(accent()).bold()));
        app.hits.push((Rect::new(tx, r.y + 5, sw(chip), 1), Hit::PlayHeader));
        if tw > 22 {
            span_at(f, tx + sw(chip) + 2, r.y + 5, Span::styled("P", Style::new().fg(MUTED)));
        }
    }
    ch.max(6) + 1
}

/// Keep the selection on screen with a little vim-style scrolloff.
fn scroll_into_view(v: &mut View, h: usize) {
    let so = 3.min(h / 3);
    if v.sel < v.scroll + so {
        v.scroll = v.sel.saturating_sub(so);
    }
    if v.sel + so >= v.scroll + h {
        v.scroll = v.sel + so + 1 - h;
    }
    v.scroll = v.scroll.min(v.items.len().saturating_sub(h));
}

/// Song / search-result rows of a view.
fn rows(f: &mut Frame, app: &mut App, body: Rect) {
    // search results and song lists get two-line rows with a thumbnail;
    // album pages (one cover for every song) and narrow panes stay compact
    let v = app.view();
    let rich = !v.items.is_empty()
        && !matches!(
            v.source,
            Source::Album(_) | Source::Artist(_) | Source::Queue
        )
        && body.width >= 50
        && body.height >= 6;
    if rich {
        rich_rows(f, app, body);
    } else {
        plain_rows(f, app, body);
    }
}

/// What a result is, for the second line of search rows.
fn kind_label(it: &Item) -> &'static str {
    match it.kind {
        Kind::Track => "Song",
        Kind::Episode => "Podcast",
        Kind::Album if it.extra.starts_with("single") => "Single",
        Kind::Album => "Album",
        Kind::Artist => "Artist",
        Kind::Playlist => "Playlist",
        Kind::Liked => "Library",
    }
}

/// A small cover thumbnail (fetched on demand).
fn thumb(f: &mut Frame, app: &mut App, r: Rect, it: &Item) {
    if it.kind == Kind::Liked {
        liked_cover(f, r);
        return;
    }
    let Some(url) = it.thumb.clone().or_else(|| it.image.clone()) else {
        f.render_widget(Block::new().style(Style::new().bg(Color::Rgb(38, 38, 38))), r);
        return;
    };
    match app.thumbnail(&url, r.width, r.height) {
        Some(Thumbnail::Bitmap(image)) => f.render_widget(ratatui_image::Image::new(image), r),
        Some(Thumbnail::Text(art)) => art.draw(r, f.buffer_mut()),
        None => {
            app.fetch_image(url);
            f.render_widget(Block::new().style(Style::new().bg(Color::Rgb(38, 38, 38))), r);
        }
    }
}

/// Two-line rows: thumbnail, title over artists (or kind · artists), extras.
fn rich_rows(f: &mut Frame, app: &mut App, body: Rect) {
    const RH: u16 = 2; // text rows per item
    const STEP: u16 = RH + 1; // + one blank line between items
    const TW: u16 = 4; // thumbnail width in terminal cells
    let playing_uri = app.player.item.as_ref().map(|i| i.uri.clone());
    let per = ((body.height + 1) / STEP).max(1) as usize;
    app.list_height = per;
    let v = top(app);
    let numbered = matches!(v.source, Source::Playlist(_) | Source::Liked | Source::Queue);
    let is_search = matches!(v.source, Source::Search { .. });
    scroll_into_view(v, per);
    let (scroll, sel) = (v.scroll, v.sel);
    let items: Vec<Item> = v.items.iter().skip(scroll).take(per).cloned().collect();
    let loading_more = v.loading && v.next.is_some();

    let lead: u16 = if numbered { 5 } else { 0 };
    let dur_w: u16 = 6;
    let x_thumb = body.x + 2 + lead;
    let x_text = x_thumb + TW + 2;
    let text_w = body.right().saturating_sub(x_text + dur_w + 1);
    // playlists: an album column on wide screens, like a music app's table
    let album_w = if numbered && text_w >= 70 { text_w * 34 / 100 } else { 0 };
    let title_w = text_w.saturating_sub(if album_w > 0 { album_w + 2 } else { 0 });

    for (row, it) in items.iter().enumerate() {
        let idx = scroll + row;
        let y = body.y + row as u16 * STEP;
        let r = Rect::new(body.x, y, body.width, RH);
        let is_sel = idx == sel;
        let playing = playing_uri.as_deref() == Some(it.uri.as_str()) && !it.uri.is_empty();
        if is_sel {
            f.render_widget(Block::new().style(Style::new().bg(SEL_BG)), r);
        }
        // marker + number
        let marker = if playing {
            Span::styled("▶", Style::new().fg(accent()))
        } else if is_sel {
            Span::styled("▎", Style::new().fg(accent()))
        } else {
            Span::raw(" ")
        };
        f.render_widget(Line::from(marker), Rect::new(body.x, y, 1, 1));
        if is_sel && !playing {
            f.render_widget(Line::styled("▎", Style::new().fg(accent())), Rect::new(body.x, y + 1, 1, 1));
        }
        if numbered {
            f.render_widget(
                Line::styled(format!("{:>3}", idx + 1), Style::new().fg(if playing { accent() } else { DIM })),
                Rect::new(body.x + 1, y, 3, 1),
            );
        }
        thumb(f, app, Rect::new(x_thumb, y, TW, RH), it);

        // line 1: title (+ album column) + duration
        let mut name_st = Style::new().fg(if playing { accent() } else { FG });
        if is_sel {
            name_st = name_st.add_modifier(Modifier::BOLD);
        }
        f.render_widget(Line::styled(trunc(&it.name, title_w as usize), name_st), Rect::new(x_text, y, title_w, 1));
        if album_w > 0 && it.kind.api_type() == "track" {
            f.render_widget(
                Line::styled(trunc(&it.extra, album_w as usize), Style::new().fg(DIM)),
                Rect::new(x_text + title_w + 2, y, album_w, 1),
            );
        }
        if let Some(d) = it.duration_ms {
            f.render_widget(
                Line::styled(fmt_ms(d), Style::new().fg(DIM)).alignment(Alignment::Right),
                Rect::new(body.right().saturating_sub(dur_w), y, dur_w, 1),
            );
        }

        // line 2: what it is · who
        let sub_st = Style::new().fg(if is_sel { MUTED } else { DIM });
        let mut spans: Vec<Span> = Vec::new();
        if is_search {
            spans.push(Span::styled(kind_label(it), Style::new().fg(tag_color(it.kind))));
            let rest = match it.kind {
                Kind::Album | Kind::Playlist => [it.sub.as_str(), it.extra.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · "),
                _ => it.sub.clone(),
            };
            if !rest.is_empty() {
                spans.push(Span::styled(" · ", Style::new().fg(FAINT)));
                let room = (text_w as usize).saturating_sub(kind_label(it).len() + 3);
                spans.push(Span::styled(trunc(&rest, room), sub_st));
            }
        } else {
            spans.push(Span::styled(trunc(&it.sub, text_w as usize), sub_st));
        }
        f.render_widget(Line::from(spans), Rect::new(x_text, y + 1, text_w + dur_w, 1));
        // the gap below counts as part of the row for clicks
        let hit_h = STEP.min(body.bottom().saturating_sub(y));
        app.hits.push((Rect::new(body.x, y, body.width, hit_h), Hit::Row(idx)));
    }
    if loading_more && (items.len() as u16) * STEP < body.height {
        let ry = body.y + items.len() as u16 * STEP;
        f.render_widget(Line::styled("    loading…", Style::new().fg(DIM)), Rect::new(body.x, ry, body.width, 1));
    }
}

/// One-line rows (album pages, narrow panes).
fn plain_rows(f: &mut Frame, app: &mut App, body: Rect) {
    let playing_uri = app.player.item.as_ref().map(|i| i.uri.clone());
    let h = body.height as usize;
    let local = app.queued_len();
    let v = top(app);
    if v.items.is_empty() {
        let info = EmptyInfo { source: v.source.clone(), error: v.error.clone(), loading: v.loading, note: v.note.clone() };
        if let Some(h) = empty_state(f, app, body, &info) {
            app.hits.push(h);
        }
        return;
    }
    let numbered = matches!(v.source, Source::Album(_) | Source::Playlist(_) | Source::Liked | Source::Queue);
    let queue = v.source == Source::Queue;
    scroll_into_view(v, h);
    let (scroll, sel) = (v.scroll, v.sel);
    let items: Vec<Item> = v.items.iter().skip(scroll).take(h).cloned().collect();
    let loading_more = v.loading && v.next.is_some();

    let w = body.width as usize;
    let lead = if numbered { 5 } else { 8 };
    let dur_w = 7;
    let rest = w.saturating_sub(2 + lead + dur_w);
    let name_w = rest * 11 / 20;
    let sub_w = rest.saturating_sub(name_w + 2);

    for (row, it) in items.iter().enumerate() {
        let idx = scroll + row;
        let r = Rect::new(body.x, body.y + row as u16, body.width, 1);
        let is_sel = idx == sel;
        let playing = playing_uri.as_deref() == Some(it.uri.as_str()) && !it.uri.is_empty();
        let bg = if is_sel { SEL_BG } else { BG };
        let marker = if queue && idx < local {
            Span::styled("↕ ", Style::new().fg(if is_sel { accent() } else { MUTED }))
        } else if playing {
            Span::styled("▶ ", Style::new().fg(accent()))
        } else if is_sel {
            Span::styled("▎ ", Style::new().fg(accent()))
        } else {
            Span::raw("  ")
        };
        let lead_span = if numbered {
            Span::styled(format!("{:>3}  ", idx + 1), Style::new().fg(DIM))
        } else {
            Span::styled(pad(it.kind.tag(), lead), Style::new().fg(tag_color(it.kind)))
        };
        let mut name_st = Style::new().fg(if playing { accent() } else { FG });
        if is_sel {
            name_st = name_st.add_modifier(Modifier::BOLD);
        }
        let sub = match it.kind {
            Kind::Album | Kind::Playlist => [it.sub.as_str(), it.extra.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            _ => it.sub.clone(),
        };
        let dur = it.duration_ms.map(fmt_ms).unwrap_or_default();
        // rows without a duration let the secondary column use that space
        let this_sub_w = if dur.is_empty() { sub_w + dur_w } else { sub_w };
        let line = Line::from(vec![
            marker,
            lead_span,
            Span::styled(pad(&it.name, name_w), name_st),
            Span::raw("  "),
            Span::styled(pad(&sub, this_sub_w), Style::new().fg(if is_sel { MUTED } else { DIM })),
            Span::styled(if dur.is_empty() { String::new() } else { format!("{dur:>dur_w$}") }, Style::new().fg(DIM)),
        ])
        .style(Style::new().bg(bg));
        f.render_widget(line, r);
        app.hits.push((r, Hit::Row(idx)));
    }
    if loading_more && items.len() < h {
        let ry = body.y + items.len() as u16;
        f.render_widget(Line::styled("    loading…", Style::new().fg(DIM)), Rect::new(body.x, ry, body.width, 1));
    }
}

/// The playlists page: your playlists as a wall of cover cards.
fn playlists_grid(f: &mut Frame, app: &mut App, area: Rect) {
    const GAP: u16 = 3;
    const CARD_W: u16 = 18;
    const SELECTED_BG: Color = Color::Rgb(48, 48, 48);
    const HOVER_BG: Color = Color::Rgb(58, 58, 58);
    // 1-col inset so the selection frame has room on the left
    let area = Rect::new(area.x + 1, area.y, area.width.saturating_sub(1), area.height);
    let cols = ((area.width + GAP) / (CARD_W + GAP)).max(1);
    let cw = ((area.width + GAP) / cols).saturating_sub(GAP).max(8);
    // small enough that at least two rows of cards fit
    let max_cover_h = (area.height / 2).saturating_sub(4).max(4);
    let (cw, cover_h) = square(app, cw, max_cover_h);
    let card_h = cover_h + 4; // top padding, cover, name, count, gap
    let rows_vis = ((area.height + 1) / card_h) as usize;
    if rows_vis == 0 {
        return;
    }
    let cols_us = cols as usize;
    app.lib_cols = cols_us;
    app.list_height = rows_vis * cols_us;
    let pointer = app.pointer;

    let ctx = app.player.context.as_ref().map(|c| c.uri.clone());
    let v = top(app);
    let sel_row = v.sel / cols_us;
    if sel_row < v.scroll {
        v.scroll = sel_row;
    }
    if sel_row >= v.scroll + rows_vis {
        v.scroll = sel_row + 1 - rows_vis;
    }
    let (first, sel) = (v.scroll * cols_us, v.sel);
    let cards: Vec<(usize, Item)> =
        v.items.iter().enumerate().skip(first).take(rows_vis * cols_us).map(|(i, it)| (i, it.clone())).collect();

    for (n, (idx, it)) in cards.into_iter().enumerate() {
        let (row, col) = ((n / cols_us) as u16, (n % cols_us) as u16);
        let x = area.x + col * (cw + GAP);
        let card = Rect::new(x, area.y + row * card_h, cw, cover_h + 3);
        let y = card.y + 1;
        let is_sel = idx == sel;
        let hovered = pointer.is_some_and(|p| card.contains(p));
        let playing = ctx.as_deref() == Some(it.uri.as_str()) && !it.uri.is_empty();

        if is_sel || hovered {
            let bg = if hovered { HOVER_BG } else { SELECTED_BG };
            let frame = Rect::new(x - 1, card.y, cw + 2, card.height).intersection(f.area());
            f.render_widget(Block::new().style(Style::new().bg(bg)), frame);
        }
        let cover_r = Rect::new(x, y, cw, cover_h);
        match (&it.image, it.kind) {
            (_, Kind::Liked) => liked_cover(f, cover_r),
            (Some(url), _) => match app.rendered_art(url, cw, cover_h) {
                Some(art) => art.draw(cover_r, f.buffer_mut()),
                None => {
                    app.fetch_image(url.clone());
                    cover_placeholder(f, cover_r);
                }
            },
            (None, _) => cover_placeholder(f, cover_r),
        }

        let (mark, mark_st) = if playing { ("▶ ", Style::new().fg(accent())) } else { ("", Style::new()) };
        let mut name_st = Style::new().fg(if playing { accent() } else { FG });
        if is_sel {
            name_st = name_st.add_modifier(Modifier::BOLD);
        }
        let name_w = (cw as usize).saturating_sub(mark.width());
        f.render_widget(
            Line::from(vec![Span::styled(mark, mark_st), Span::styled(trunc(&it.name, name_w), name_st)]),
            Rect::new(x, y + cover_h, cw, 1),
        );
        let sub = if it.extra.is_empty() { "Playlist" } else { &it.extra };
        let pin = if app.playlist_pinned(&it.uri) { "★ " } else { "" };
        f.render_widget(
            Line::from(vec![
                Span::styled(pin, Style::new().fg(accent())),
                Span::styled(
                    trunc(sub, (cw as usize).saturating_sub(pin.width())),
                    Style::new().fg(if is_sel { MUTED } else { DIM }),
                ),
            ]),
            Rect::new(x, y + cover_h + 1, cw, 1),
        );
        app.hits.push((card, Hit::Row(idx)));
    }
}

struct EmptyInfo {
    source: Source,
    error: Option<String>,
    loading: bool,
    note: Option<String>,
}

fn empty_state(f: &mut Frame, app: &App, r: Rect, v: &EmptyInfo) -> Option<(Rect, Hit)> {
    let dim = Style::new().fg(DIM);
    let mut lines: Vec<Line> = Vec::new();
    if app.api.is_none() && v.source != Source::Queue {
        lines.push(Line::styled("search & library need the Spotify Web API", Style::new().fg(FG).bold()));
        lines.push(Line::raw(""));
        if let Some(p) = &app.api_problem {
            lines.push(Line::styled(p.clone(), Style::new().fg(YELLOW)));
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(vec![
            Span::styled(" S ", Style::new().fg(on_accent()).bg(accent()).bold()),
            Span::styled("  open setup and paste your client id", Style::new().fg(MUTED)),
        ]));
        lines.push(Line::raw(""));
        lines.push(Line::styled("playback controls work without it, through Soloist.", dim));
    } else if let Some(e) = &v.error {
        lines.push(Line::styled(e.clone(), Style::new().fg(RED)));
    } else if v.loading {
        lines.push(Line::styled(
            if matches!(v.source, Source::Search { .. }) { "searching…" } else { "loading…" },
            dim,
        ));
    } else if let Some(n) = &v.note {
        lines.push(Line::styled(n.clone(), Style::new().fg(MUTED)));
    } else {
        lines.push(Line::styled(
            match &v.source {
                Source::Search { .. } => "no results",
                Source::Queue => "queue is empty — press a on a song to add it",
                _ => "nothing here",
            },
            dim,
        ));
    }
    let inner = r.inner(Margin::new(2, 0));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    let at = 2 + app.api_problem.as_ref().map_or(0, |_| 2);
    (app.api.is_none() && v.source != Source::Queue && inner.height > at)
        .then(|| (Rect::new(inner.x, inner.y + at, 42.min(inner.width), 1), Hit::OpenSetup))
}

/// Liked Songs has no artwork: a purple→blue gradient with a heart.
fn liked_cover(f: &mut Frame, r: Rect) {
    let buf = f.buffer_mut();
    for dy in 0..r.height {
        for dx in 0..r.width {
            let t = (dx as f32 / r.width.max(1) as f32 + dy as f32 / r.height.max(1) as f32) / 2.0;
            let mix = |a: f32, b: f32| (a + (b - a) * t) as u8;
            let c = Color::Rgb(mix(80.0, 40.0), mix(40.0, 90.0), mix(160.0, 150.0));
            if let Some(cell) = buf.cell_mut((r.x + dx, r.y + dy)) {
                cell.set_char(' ').set_bg(c);
            }
        }
    }
    if let Some(cell) = buf.cell_mut((r.x + r.width / 2, r.y + r.height / 2)) {
        cell.set_char('♥').set_fg(Color::White);
    }
}

// ------------------------------------------------- composer + footer ---

const COMPOSER_BG: Color = Color::Rgb(34, 34, 34);
const PLACEHOLDER: &str = "Search songs, albums, artists, playlists";

/// The input box, Codex-style: a slightly lighter block with a `›` prompt.
fn composer(f: &mut Frame, app: &mut App, r: Rect) {
    let r = r.inner(Margin::new(1, 0));
    f.render_widget(Block::new().style(Style::new().bg(COMPOSER_BG)), r);
    app.hits.push((r, Hit::SearchBar));
    let row = Rect::new(r.x + 2, r.y + 1, r.width.saturating_sub(4), 1);
    let bg = Style::new().bg(COMPOSER_BG);
    let width = row.width.saturating_sub(2) as usize;

    if app.mode == Mode::Command {
        let line = if app.cmd.text.is_empty() {
            Line::from(vec![
                Span::styled(": ", bg.fg(YELLOW).bold()),
                Span::styled(trunc("command   vol 50 · seek 1:23 · shuffle on · q", width), bg.fg(DIM)),
            ])
        } else {
            Line::from(vec![Span::styled(": ", bg.fg(YELLOW).bold()), Span::styled(app.cmd.text.clone(), bg.fg(FG))])
        };
        f.render_widget(line, row);
        let before: String = app.cmd.text.chars().take(app.cmd.cur).collect();
        f.set_cursor_position(Position::new(row.x + 2 + sw(&before), row.y));
        return;
    }

    let focused = app.mode == Mode::Search;
    let q = &app.query.text;
    let (kinds, _) = parse_query(q);
    let prompt = Span::styled("› ", bg.fg(if focused { FG } else { MUTED }).bold());
    let mut spans = vec![prompt];
    if q.is_empty() {
        spans.push(Span::styled(trunc(PLACEHOLDER, width), bg.fg(DIM)));
    } else {
        let style = bg.fg(if focused { FG } else { MUTED });
        match (kinds.len() == 1).then(|| q.split_once(' ').unwrap_or((q.as_str(), ""))) {
            Some((tok, rest)) => {
                spans.push(Span::styled(tok.to_string(), bg.fg(tag_color(kinds[0])).bold()));
                if q.len() > tok.len() {
                    spans.push(Span::styled(format!(" {rest}"), style));
                }
            }
            None => spans.push(Span::styled(q.clone(), style)),
        }
    }
    f.render_widget(Line::from(spans), row);

    // right side: what the search is filtered to, or how to start one
    let right = if !q.trim().is_empty() {
        if kinds.len() == 1 { kinds[0].plural().to_string() } else { "everything".into() }
    } else if !focused {
        "/ to search".into()
    } else {
        String::new()
    };
    if !right.is_empty() && row.width as usize > PLACEHOLDER.len() + right.len() + 6 {
        f.render_widget(Line::styled(right, bg.fg(DIM)).alignment(Alignment::Right), row);
    }
    if focused {
        let before: String = app.query.text.chars().take(app.query.cur).collect();
        f.set_cursor_position(Position::new(row.x + 2 + sw(&before), row.y));
    }
}

/// One quiet line under the composer: status / messages on the left, hints on the right.
fn footer(f: &mut Frame, app: &App, r: Rect) {
    let r = Rect::new(r.x + 3, r.y, r.width.saturating_sub(6), 1);
    let dim = Style::new().fg(DIM);
    let sep = || Span::styled(" · ", Style::new().fg(FAINT));
    let room = (r.width as usize).saturating_sub(4);

    let left: Vec<Span> = match (&app.msg, app.mode) {
        (Some((m, true, _)), _) => vec![Span::styled(trunc(m, room), Style::new().fg(RED))],
        (Some((m, false, _)), _) => vec![Span::styled(trunc(m, room), Style::new().fg(MUTED))],
        (None, Mode::Search) => vec![
            Span::styled("search", Style::new().fg(BLUE)),
            sep(),
            Span::styled("⏎ done", dim),
            sep(),
            Span::styled("esc back", dim),
            sep(),
            Span::styled("tab filter", dim),
        ],
        (None, Mode::Command) => vec![
            Span::styled("command", Style::new().fg(YELLOW)),
            sep(),
            Span::styled("⏎ run", dim),
            sep(),
            Span::styled("esc cancel", dim),
        ],
        (None, Mode::Normal) => {
            let p = &app.player;
            if !p.connected {
                let reason = if p.down_reason.is_empty() {
                    "Connecting to Soloist…"
                } else {
                    &p.down_reason
                };
                vec![Span::styled(trunc(reason, room), Style::new().fg(RED))]
            } else if !p.logged_in {
                let msg = format!("Select “{}” in Spotify to connect", p.device);
                vec![Span::styled(trunc(&msg, room), dim)]
            } else {
                Vec::new()
            }
        }
    };
    let left_w: usize = left.iter().map(|s| s.content.width()).sum();
    f.render_widget(Line::from(left), r);

    let full = "!s songs · !a albums · !p playlists · !r artists · ? for shortcuts";
    let short = "? for shortcuts";
    let free = (r.width as usize).saturating_sub(left_w + 4);
    let hint = if app.mode == Mode::Normal && free >= full.width() {
        full
    } else if free >= short.width() {
        short
    } else {
        ""
    };
    f.render_widget(Line::styled(hint, dim).alignment(Alignment::Right), r);
}

// ---------------------------------------------------------------- help ---

fn help(f: &mut Frame, area: Rect) {
    let rows: &[(&str, &str, &str, &str)] = &[
        ("NAVIGATE", "", "PLAYBACK", ""),
        ("j k  ↑ ↓", "move", "space", "play / pause"),
        ("gg  G", "top / bottom", "n  p", "next / previous"),
        ("^d  ^u", "half page", "s", "shuffle"),
        ("⏎", "play", "r", "repeat  off → all → one"),
        ("l  →", "open", "+  -   m", "volume   mute"),
        ("h  ←  ⌫", "back", "]  [", "seek ±10s"),
        ("o", "open playing album", "a", "add to queue"),
        ("H   Tab", "playlists   queue", "P", "play this album / list"),
        ("K   J", "move queued song up/down", "L", "show / hide lyrics"),
        ("", "", "", ""),
        ("SEARCH", "", "COMMANDS", ""),
        ("/  i", "search", ":vol 50", ":seek 1:23  :seek +30"),
        ("!s !a", "songs  albums", ":shuffle on", ":repeat track"),
        ("!p !r", "playlists  artists", ":play <uri>", ":queue  :home"),
        ("Tab", "cycle filter", ":stop", "stop soloist"),
        ("", "", ":pin", "pin / unpin playlist"),
        ("esc  ⏎", "leave search", ":q", "quit"),
        ("S", "setup / credentials", ":login", "reconnect Spotify"),
        ("A", "art: pixel → dots → image", ":bg", "background: spin/still/off"),
    ];
    let w = 76.min(area.width.saturating_sub(4));
    let h = (rows.len() as u16 + 8).min(area.height.saturating_sub(2));
    let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height.saturating_sub(h)) / 2, w, h);
    f.render_widget(Clear, r);
    f.render_widget(Block::new().style(Style::new().bg(CARD_BG)), r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(FAINT))
        .title(Line::from(vec![Span::styled(" str ", Style::new().fg(accent()).bold()), Span::styled("keys ", Style::new().fg(DIM))]));
    let inner = block.inner(r).inner(Margin::new(2, 1));
    f.render_widget(block, r);
    let col = (inner.width / 2) as usize;
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(k1, d1, k2, d2)| {
            let head = d1.is_empty() && !k1.is_empty();
            if head {
                return Line::from(vec![
                    Span::styled(pad(k1, col), Style::new().fg(accent()).bold()),
                    Span::styled(*k2, Style::new().fg(accent()).bold()),
                ]);
            }
            Line::from(vec![
                Span::styled(pad(k1, 10), Style::new().fg(FG).bold()),
                Span::styled(pad(d1, col.saturating_sub(10)), Style::new().fg(MUTED)),
                Span::styled(pad(k2, 12), Style::new().fg(FG).bold()),
                Span::styled(*d2, Style::new().fg(MUTED)),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "mouse: click selects · double-click plays/opens · right-click plays · click the bar to seek · scroll volume",
        Style::new().fg(DIM),
    ));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// --------------------------------------------------------------- setup ---

fn setup(f: &mut Frame, app: &mut App, area: Rect) {
    let port = app.cfg.redirect_port();
    let spotify_ok = app.api.is_some();
    let soloist_ok = app.player.connected;
    let installing = app.installing;
    let Some(st) = app.setup.as_ref() else { return };

    let w = 76.min(area.width.saturating_sub(2));
    let h = 27.min(area.height);
    let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    f.render_widget(Clear, r);
    f.render_widget(Block::new().style(Style::new().bg(CARD_BG)), r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(FAINT))
        .title(Line::from(vec![
            Span::styled(" str ", Style::new().fg(accent()).bold()),
            Span::styled("setup ", Style::new().fg(DIM)),
        ]));
    let inner = block.inner(r).inner(Margin::new(3, 1));
    f.render_widget(block, r);
    if inner.width < 30 || inner.height < 8 {
        return;
    }
    let mut hits: Vec<(Rect, Hit)> = Vec::new();
    let row = |y: u16| Rect::new(inner.x, inner.y + y, inner.width, 1);
    let tw = inner.width as usize;

    let state = |ok: bool| {
        if ok {
            Span::styled("● connected", Style::new().fg(accent()))
        } else {
            Span::styled("○ not connected", Style::new().fg(DIM))
        }
    };
    let section = |f: &mut Frame, y: u16, title: &str, right: &str, state: Span<'static>| {
        f.render_widget(Line::styled(title.to_string(), Style::new().fg(FG).bold()), row(y));
        let right = Line::from(vec![Span::styled(format!("{right}  "), Style::new().fg(DIM)), state]);
        f.render_widget(right.alignment(Alignment::Right), row(y));
    };
    let step = |f: &mut Frame, y: u16, n: &str, parts: Vec<Span<'static>>| {
        let mut v = vec![Span::styled(format!("{n}  "), Style::new().fg(accent()))];
        v.extend(parts);
        f.render_widget(Line::from(v), row(y));
    };

    // ---- Spotify Web API
    section(f, 0, "Spotify Web API", "search & library", state(spotify_ok));
    step(f, 2, "1", vec![
        Span::styled("create an app at ", Style::new().fg(MUTED)),
        Span::styled("developer.spotify.com/dashboard", Style::new().fg(FG)),
    ]);
    step(f, 3, "2", vec![
        Span::styled("add the redirect URI ", Style::new().fg(MUTED)),
        Span::styled(format!("http://127.0.0.1:{port}/callback"), Style::new().fg(FG)),
    ]);
    step(f, 4, "3", vec![Span::styled("paste its client id here", Style::new().fg(MUTED))]);
    field(f, st, 0, row(6), &mut hits);
    let cid = st.fields[0].text.trim();
    if !cid.is_empty() && !Setup::looks_like_client_id(cid) {
        f.render_widget(
            Line::styled(
                format!("{:15}that doesn't look like a client id (32 hex characters)", ""),
                Style::new().fg(YELLOW),
            ),
            row(7),
        );
    }

    // ---- Soloist
    let sol_state = match installing {
        Some(p) => Span::styled(format!("↓ downloading {p}%"), Style::new().fg(YELLOW)),
        None => state(soloist_ok),
    };
    section(f, 9, "Spotify Soloist", "playback", sol_state);
    f.render_widget(
        Line::from(vec![
            Span::styled("optional  ", Style::new().fg(DIM)),
            Span::styled(
                trunc("paste your Soloist API key — str downloads and runs Soloist for you", tw.saturating_sub(10)),
                Style::new().fg(MUTED),
            ),
        ]),
        row(11),
    );
    field(f, st, 1, row(13), &mut hits);
    field(f, st, 2, row(14), &mut hits);

    // ---- buttons
    let by = 16;
    let (save_l, cancel_l) = (" ▶ save & connect ", " cancel ");
    let save_st = if st.focus == 3 {
        Style::new().fg(on_accent()).bg(accent()).bold()
    } else {
        Style::new().fg(accent()).bg(Color::Rgb(34, 34, 34)).bold()
    };
    let cancel_st = if st.focus == 4 {
        Style::new().fg(Color::Rgb(0, 0, 0)).bg(MUTED)
    } else {
        Style::new().fg(MUTED).bg(Color::Rgb(34, 34, 34))
    };
    let sx = inner.x + 15;
    span_at(f, sx, inner.y + by, Span::styled(save_l, save_st));
    hits.push((Rect::new(sx, inner.y + by, sw(save_l), 1), Hit::SetupSave));
    let cx = sx + sw(save_l) + 2;
    span_at(f, cx, inner.y + by, Span::styled(cancel_l, cancel_st));
    hits.push((Rect::new(cx, inner.y + by, sw(cancel_l), 1), Hit::SetupCancel));

    // ---- status
    let status: Vec<Line> = match &st.stage {
        SetupStage::Editing => vec![Line::styled(
            format!("saves to {}", crate::config::display_path()),
            Style::new().fg(DIM),
        )],
        SetupStage::LoggingIn(None) => vec![Line::styled("◐ starting login…", Style::new().fg(YELLOW))],
        SetupStage::LoggingIn(Some(_)) => vec![
            Line::styled("◐ approve str in the browser tab that just opened…", Style::new().fg(YELLOW)),
            Line::styled("  didn't open? press ^o", Style::new().fg(DIM)),
        ],
        SetupStage::Error(e) => vec![Line::styled(format!("✗ {e}"), Style::new().fg(RED))],
    };
    let sy = by + 2;
    if inner.height > sy {
        let sh = (inner.height - sy).saturating_sub(2).min(3);
        f.render_widget(
            Paragraph::new(status).wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.y + sy, inner.width, sh),
        );
    }
    let hint = "tab next · ⏎ save · esc close · ⌘V paste · ^r show key";
    f.render_widget(
        Line::styled(trunc(hint, tw), Style::new().fg(DIM)).alignment(Alignment::Center),
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
    );

    // cursor in the focused field
    if st.focus < 3 {
        let y = [6u16, 13, 14][st.focus];
        let (_, cur) = field_view(st, st.focus, field_width(inner.width));
        f.set_cursor_position(Position::new(inner.x + 16 + cur, inner.y + y));
    }
    app.hits.extend(hits);
}

fn field_width(total: u16) -> usize {
    total.saturating_sub(15 + 2) as usize
}

/// What a field shows (masked, scrolled) and where the cursor sits in it.
fn field_view(st: &Setup, i: usize, w: usize) -> (String, u16) {
    let f = &st.fields[i];
    let chars: Vec<char> = if i == 1 && !st.reveal {
        let n = f.text.chars().count();
        f.text
            .chars()
            .enumerate()
            .map(|(j, c)| if j + 4 >= n { c } else { '•' })
            .collect()
    } else {
        f.text.chars().collect()
    };
    let inner = w.saturating_sub(1).max(1);
    let start = f.cur.saturating_sub(inner);
    let shown: String = chars.iter().skip(start).take(inner).collect();
    let cur: String = chars[start..f.cur.min(chars.len())].iter().collect();
    (shown, sw(&cur))
}

fn field(f: &mut Frame, st: &Setup, i: usize, r: Rect, hits: &mut Vec<(Rect, Hit)>) {
    let focused = st.focus == i;
    let label = SETUP_LABELS[i];
    let (marker, lst) = if focused {
        ("› ", Style::new().fg(accent()).bold())
    } else {
        ("  ", Style::new().fg(MUTED))
    };
    span_at(f, r.x, r.y, Span::styled(format!("{marker}{label}"), lst));
    let w = field_width(r.width);
    let bx = Rect::new(r.x + 15, r.y, (w + 2) as u16, 1);
    let bg = if focused { Color::Rgb(46, 46, 46) } else { Color::Rgb(32, 32, 32) };
    f.render_widget(Block::new().style(Style::new().bg(bg)), bx);
    let (shown, _) = field_view(st, i, w);
    let content = if shown.is_empty() && !focused {
        let ph = ["paste here", "only if str should start Soloist", "str"][i];
        Span::styled(ph, Style::new().fg(DIM).bg(bg))
    } else {
        Span::styled(shown, Style::new().fg(FG).bg(bg))
    };
    f.render_widget(Line::from(content), Rect::new(bx.x + 1, r.y, w as u16, 1));
    hits.push((Rect::new(r.x, r.y, r.width, 1), Hit::SetupField(i)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation() {
        assert_eq!(trunc("hello", 10), "hello");
        assert_eq!(trunc("hello world", 6), "hello…");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(trunc("日本語テキスト", 5).width(), 5);
    }
}
