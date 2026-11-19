//! Album art drawn with text: chunky pixel art (half blocks) or a dot matrix.
//! Fits the terminal look better than a photo, and works in every terminal.

use image::DynamicImage;
use image::imageops::FilterType;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Style {
    /// Square pixels (half blocks) at full cell resolution, lightly posterized.
    Pixel,
    /// A dot matrix: dot size follows brightness, dot colour follows the art.
    Dots,
    /// The real image via kitty / iTerm2 / sixel (falls back to half blocks).
    Image,
}

impl Style {
    pub fn parse(s: &str) -> Style {
        match s {
            "dots" | "dot" | "matrix" => Style::Dots,
            "image" | "real" | "photo" => Style::Image,
            _ => Style::Pixel,
        }
    }
    pub fn next(self) -> Style {
        match self {
            Style::Pixel => Style::Dots,
            Style::Dots => Style::Image,
            Style::Image => Style::Pixel,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Style::Pixel => "pixel",
            Style::Dots => "dots",
            Style::Image => "image",
        }
    }
}

/// One terminal cell of rendered art.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Option<Color>,
}

/// Pre-rendered art for a fixed area size.
pub struct Rendered {
    pub w: u16,
    pub h: u16,
    pub cells: Vec<Cell>,
}

impl Rendered {
    pub fn draw(&self, area: Rect, buf: &mut Buffer) {
        for y in 0..self.h.min(area.height) {
            for x in 0..self.w.min(area.width) {
                let c = self.cells[(y * self.w + x) as usize];
                if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                    cell.set_char(c.ch).set_fg(c.fg);
                    if let Some(bg) = c.bg {
                        cell.set_bg(bg);
                    }
                }
            }
        }
    }
}

/// Snap a channel to a few levels: gives the retro, reduced-palette look.
fn posterize(v: u8) -> u8 {
    const LEVELS: f32 = 14.0;
    ((v as f32 / 255.0 * LEVELS).round() / LEVELS * 255.0) as u8
}

fn rgb(p: image::Rgb<u8>, poster: bool) -> Color {
    let f = |v| if poster { posterize(v) } else { v };
    Color::Rgb(f(p[0]), f(p[1]), f(p[2]))
}

/// Render `img` into `w`×`h` cells.
pub fn render(img: &DynamicImage, style: Style, w: u16, h: u16) -> Rendered {
    let (w32, h32) = (w as u32, h as u32);
    let mut cells = Vec::with_capacity((w * h) as usize);
    match style {
        Style::Pixel | Style::Image => {
            // Each art pixel is k columns × k half-rows (= square on a 1:2 font).
            // k grows only for very large covers, so art stays crisp but pixel-y.
            let k = w32.div_ceil(72).max(1);
            let (pw, ph) = ((w32 / k).max(1), ((h32 * 2) / k).max(1));
            let small = img.resize_exact(pw, ph, FilterType::Triangle).to_rgb8();
            // centre the grid when the area isn't a multiple of k
            let (ox, oy) = ((w32 - (pw * k).min(w32)) / 2, (h32 * 2 - (ph * k).min(h32 * 2)) / 2);
            let px = |x: u32, hy: u32| -> Option<Color> {
                let (x, hy) = (x.checked_sub(ox)? / k, hy.checked_sub(oy)? / k);
                (x < pw && hy < ph).then(|| rgb(*small.get_pixel(x, hy), true))
            };
            for y in 0..h32 {
                for x in 0..w32 {
                    let (top, bot) = (px(x, y * 2), px(x, y * 2 + 1));
                    cells.push(match (top, bot) {
                        (Some(t), Some(b)) => Cell { ch: '▀', fg: t, bg: Some(b) },
                        (Some(t), None) => Cell { ch: '▀', fg: t, bg: None },
                        (None, Some(b)) => Cell { ch: '▄', fg: b, bg: None },
                        (None, None) => Cell { ch: ' ', fg: Color::Reset, bg: None },
                    });
                }
            }
        }
        Style::Dots => {
            let small = img.resize_exact(w32, h32, FilterType::Triangle).to_rgb8();
            for y in 0..h32 {
                for x in 0..w32 {
                    let p = *small.get_pixel(x, y);
                    let lum = (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0;
                    let ch = match lum {
                        l if l < 0.06 => '·',
                        l if l < 0.30 => '•',
                        _ => '●',
                    };
                    // keep dark areas faintly visible as a grid, like a dot-matrix display
                    let fg = if lum < 0.06 {
                        Color::Rgb(38, 38, 38)
                    } else {
                        // lift colours a bit so dots read on a dark background
                        let lift = |v: u8| ((v as f32 * 0.85) + 30.0).min(255.0) as u8;
                        Color::Rgb(lift(p[0]), lift(p[1]), lift(p[2]))
                    };
                    cells.push(Cell { ch, fg, bg: None });
                }
            }
        }
    }
    Rendered { w, h, cells }
}

fn hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, if max == 0.0 { 0.0 } else { d / max }, max)
}

fn rgb_of(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r + m, g + m, b + m)
}

/// Relative luminance (0–1) of an sRGB colour given as 0–1 floats.
fn luminance(r: f32, g: f32, b: f32) -> f32 {
    let lin = |c: f32| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// The UI accent for a cover: its most vivid colour, bright enough to read on
/// a dark background and to carry black text (the play button). Mostly grey
/// covers get a soft off-white. Returns 0–255 floats.
pub fn accent(img: &DynamicImage) -> (f32, f32, f32) {
    let small = img.resize_exact(24, 24, FilterType::Triangle).to_rgb8();
    let (mut acc, mut wsum) = ((0.0f32, 0.0f32, 0.0f32), 0.0f32);
    for p in small.pixels() {
        let (r, g, b) = (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
        let (_, s, v) = hsv(r, g, b);
        // vivid pixels decide the colour; dull/dark ones barely count
        let w = (s * v).powf(1.6) + 0.004;
        acc = (acc.0 + r * w, acc.1 + g * w, acc.2 + b * w);
        wsum += w;
    }
    let (h, s, _) = hsv(acc.0 / wsum, acc.1 / wsum, acc.2 / wsum);
    if s < 0.12 {
        return (225.0, 225.0, 220.0);
    }
    let mut s = (s * 1.15).clamp(0.45, 0.8);
    let mut c = rgb_of(h, s, 0.92);
    // lighten (less saturation) until black text on it is comfortable
    while luminance(c.0, c.1, c.2) < 0.32 && s > 0.15 {
        s -= 0.05;
        c = rgb_of(h, s, 0.95);
    }
    (c.0 * 255.0, c.1 * 255.0, c.2 * 255.0)
}

/// How the player background looks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BgMode {
    /// Blurred cover, slowly turning while music plays.
    Spin,
    /// Blurred cover, standing still.
    Still,
    /// Plain terminal background.
    Off,
}

impl BgMode {
    pub fn parse(s: &str) -> BgMode {
        match s {
            "still" | "blur" | "static" => BgMode::Still,
            "off" | "plain" | "none" => BgMode::Off,
            _ => BgMode::Spin,
        }
    }
    pub fn next(self) -> BgMode {
        match self {
            BgMode::Spin => BgMode::Still,
            BgMode::Still => BgMode::Off,
            BgMode::Off => BgMode::Spin,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            BgMode::Spin => "spin",
            BgMode::Still => "still",
            BgMode::Off => "off",
        }
    }
}

/// A small, heavily blurred, slightly more saturated copy of a cover that
/// can be sampled at any rotation — the player's backdrop.
pub struct Backdrop {
    n: usize,
    px: Vec<(f32, f32, f32)>,
}

impl Backdrop {
    const N: u32 = 40;

    pub fn new(img: &DynamicImage) -> Backdrop {
        let small = img
            .resize_exact(Self::N, Self::N, FilterType::Triangle)
            .blur(3.0)
            .to_rgb8();
        let px = small
            .pixels()
            .map(|p| {
                let (h, s, v) = hsv(p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
                let (r, g, b) = rgb_of(h, (s * 1.3).min(1.0), v);
                (r * 255.0, g * 255.0, b * 255.0)
            })
            .collect();
        Backdrop { n: Self::N as usize, px }
    }

    fn at(&self, x: usize, y: usize) -> (f32, f32, f32) {
        self.px[y.min(self.n - 1) * self.n + x.min(self.n - 1)]
    }

    /// Colour at (u, v) ∈ [-0.5, 0.5]² of the area, with the texture turned by
    /// `angle` and zoomed in so its edges never show while it turns.
    pub fn sample(&self, u: f32, v: f32, angle: f32) -> (f32, f32, f32) {
        const ZOOM: f32 = 0.62; // the area's corners stay inside the texture at any angle
        let (sn, cs) = angle.sin_cos();
        let (ru, rv) = ((u * cs - v * sn) * ZOOM, (u * sn + v * cs) * ZOOM);
        let n = self.n as f32;
        let (fx, fy) = (((ru + 0.5) * n - 0.5).clamp(0.0, n - 1.0), ((rv + 0.5) * n - 0.5).clamp(0.0, n - 1.0));
        let (x0, y0) = (fx as usize, fy as usize);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let lerp = |a: (f32, f32, f32), b: (f32, f32, f32), t: f32| {
            (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
        };
        let top = lerp(self.at(x0, y0), self.at(x0 + 1, y0), tx);
        let bot = lerp(self.at(x0, y0 + 1), self.at(x0 + 1, y0 + 1), tx);
        lerp(top, bot, ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(64, 64, |x, _| {
            if x < 32 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) }
        }))
    }

    #[test]
    fn pixel_fills_area_with_left_red_right_blue() {
        let r = render(&img(), Style::Pixel, 40, 20);
        assert_eq!(r.cells.len(), 800);
        let mid = 10 * 40;
        assert!(matches!(r.cells[mid + 2].fg, Color::Rgb(255, 0, 0)));
        assert!(matches!(r.cells[mid + 37].fg, Color::Rgb(0, 0, 255)));
    }

    #[test]
    fn dots_ramp() {
        let black = DynamicImage::ImageRgb8(image::RgbImage::new(8, 8));
        assert!(render(&black, Style::Dots, 4, 2).cells.iter().all(|c| c.ch == '·'));
        let white = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([255; 3])));
        assert!(render(&white, Style::Dots, 4, 2).cells.iter().all(|c| c.ch == '●'));
    }

    #[test]
    fn backdrop_samples_inside_and_turns() {
        // left half red, right half blue
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(64, 64, |x, _| {
            if x < 32 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) }
        }));
        let bd = Backdrop::new(&img);
        let left = bd.sample(-0.45, 0.0, 0.0);
        let right = bd.sample(0.45, 0.0, 0.0);
        assert!(left.0 > left.2 && right.2 > right.0);
        // half a turn swaps the sides
        let turned = bd.sample(-0.45, 0.0, std::f32::consts::PI);
        assert!(turned.2 > turned.0);
        // corners never sample outside (no panic, finite)
        let c = bd.sample(0.5, 0.5, 0.7);
        assert!(c.0.is_finite());
    }

    #[test]
    fn accent_is_vivid_and_readable() {
        // mostly black with a deep blue block: accent is blue-ish but light enough for black text
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(48, 48, |x, y| {
            if x < 16 && y < 16 { image::Rgb([20, 40, 200]) } else { image::Rgb([8, 8, 8]) }
        }));
        let (r, g, b) = accent(&img);
        assert!(b > r && b > g, "{r} {g} {b}");
        assert!(luminance(r / 255.0, g / 255.0, b / 255.0) >= 0.3);
        // grey covers get a neutral accent
        let grey = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([90, 90, 90])));
        let (r, g, _) = accent(&grey);
        assert!((r - g).abs() < 10.0);
    }

    #[test]
    fn styles_cycle() {
        assert_eq!(Style::Pixel.next().next().next(), Style::Pixel);
        assert_eq!(Style::parse("dots"), Style::Dots);
        assert_eq!(Style::parse("whatever"), Style::Pixel);
    }
}
