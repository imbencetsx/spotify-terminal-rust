//! Album art drawn with full-color half blocks or a dot matrix.

use image::DynamicImage;
use image::imageops::FilterType;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use std::borrow::Cow;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Style {
    /// Size-aware pixel art, with half blocks when bitmaps aren't available.
    Pixel,
    /// A dot matrix: dot size follows brightness, dot colour follows the art.
    Dots,
    /// The real image via kitty / iTerm2 / sixel (falls back to half blocks).
    Image,
    /// A bitmap with small, hard-edged pixels.
    SoftPixel,
}

impl Style {
    pub fn parse(s: &str) -> Style {
        match s {
            "dots" | "dot" | "matrix" => Style::Dots,
            "image" | "real" | "photo" => Style::Image,
            "softpixel" => Style::SoftPixel,
            _ => Style::Pixel,
        }
    }
    pub fn next(self) -> Style {
        match self {
            Style::Pixel => Style::Dots,
            Style::Dots => Style::Image,
            Style::Image => Style::SoftPixel,
            Style::SoftPixel => Style::Pixel,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Style::Pixel => "pixel",
            Style::Dots => "dots",
            Style::Image => "image",
            Style::SoftPixel => "softpixel",
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
                let c = self.cells[y as usize * self.w as usize + x as usize];
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

fn rgb(p: &image::Rgb<u8>) -> Color {
    Color::Rgb(p[0], p[1], p[2])
}

/// Crop evenly from the sides or ends to match the display aspect ratio.
fn center_crop(img: &DynamicImage, width: u32, height: u32) -> Cow<'_, DynamicImage> {
    let (source_w, source_h) = (img.width(), img.height());
    let (w, h) = (u64::from(width), u64::from(height));
    let (source_width, source_height) = (u64::from(source_w), u64::from(source_h));
    if w == 0 || h == 0 || source_w == 0 || source_h == 0 {
        return Cow::Borrowed(img);
    }
    let crop_w = (source_height * w / h).clamp(1, source_width) as u32;
    let crop_h = (source_width * h / w).clamp(1, source_height) as u32;
    if (crop_w, crop_h) == (source_w, source_h) {
        Cow::Borrowed(img)
    } else {
        Cow::Owned(img.crop_imm(
            (source_w - crop_w) / 2,
            (source_h - crop_h) / 2,
            crop_w,
            crop_h,
        ))
    }
}

/// Render `img` into `w`×`h` cells.
pub fn render(img: &DynamicImage, style: Style, w: u16, h: u16) -> Rendered {
    let (w32, h32) = (w as u32, h as u32);
    let mut cells = Vec::with_capacity(w as usize * h as usize);
    if w == 0 || h == 0 {
        return Rendered { w, h, cells };
    }
    let img = center_crop(img, w32, h32 * 2);
    match style {
        Style::Pixel | Style::Image | Style::SoftPixel => {
            let small = img.resize_exact(w32, h32 * 2, FilterType::Lanczos3).to_rgb8();
            for y in 0..h32 {
                for x in 0..w32 {
                    cells.push(Cell {
                        ch: '▀',
                        fg: rgb(small.get_pixel(x, y * 2)),
                        bg: Some(rgb(small.get_pixel(x, y * 2 + 1))),
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

/// Downsample to a fine pixel grid and enlarge it without smoothing the edges.
pub fn bitmap(img: &DynamicImage, width: u32, height: u32, style: Style) -> DynamicImage {
    let grid = |pixels: u32| {
        // The extra samples give little covers enough detail to read without
        // losing the chunky look. Larger covers get a finer grid overall.
        match style {
            Style::Pixel => pixels.min(5 + pixels / 6),
            Style::SoftPixel => pixels.min(7 + pixels / 5),
            Style::Image | Style::Dots => pixels,
        }
    };
    center_crop(img, width, height)
        .resize_exact(grid(width), grid(height), FilterType::Lanczos3)
        .resize_exact(width, height, FilterType::Nearest)
}

/// Use four color samples per cell when the terminal can't draw bitmap images.
pub fn render_thumbnail(img: &DynamicImage, w: u16, h: u16) -> Rendered {
    let mut cells = Vec::with_capacity(w as usize * h as usize);
    if w > 0 && h > 0 {
        let small = center_crop(img, u32::from(w), u32::from(h) * 2)
            .resize_exact(u32::from(w) * 2, u32::from(h) * 2, FilterType::Lanczos3)
            .to_rgb8();
        for y in (0..small.height()).step_by(2) {
            for x in (0..small.width()).step_by(2) {
                let samples = [
                    small.get_pixel(x, y).0,
                    small.get_pixel(x + 1, y).0,
                    small.get_pixel(x, y + 1).0,
                    small.get_pixel(x + 1, y + 1).0,
                ];
                cells.push(quadrants(samples));
            }
        }
    }
    Rendered { w, h, cells }
}

fn quadrants(samples: [[u8; 3]; 4]) -> Cell {
    // Each mask and its inverse represent the same split of foreground and background.
    // Keep the split with the least RGB error.
    const GLYPHS: [char; 8] = [' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛'];
    let mut best = (u32::MAX, ' ', [[0; 3]; 2]);
    for (mask, ch) in GLYPHS.into_iter().enumerate() {
        let mut sums = [[0u16; 3]; 2];
        let mut counts = [0u16; 2];
        for (i, sample) in samples.iter().enumerate() {
            let group = (mask >> i) & 1;
            counts[group] += 1;
            for (sum, value) in sums[group].iter_mut().zip(sample) {
                *sum += u16::from(*value);
            }
        }
        let mut colors = [[0u8; 3]; 2];
        for group in 0..2 {
            colors[group] = sums[group].map(|sum| (sum / counts[group].max(1)) as u8);
        }
        let mut error = 0;
        for (i, sample) in samples.iter().enumerate() {
            for (&value, mean) in sample.iter().zip(colors[(mask >> i) & 1]) {
                let difference = u32::from(value.abs_diff(mean));
                error += difference * difference;
            }
        }
        if error < best.0 {
            best = (error, ch, colors);
        }
    }
    let (_, ch, [[r, g, b], [fr, fg, fb]]) = best;
    Cell { ch, fg: Color::Rgb(fr, fg, fb), bg: Some(Color::Rgb(r, g, b)) }
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
    fn tiny_cover_keeps_horizontal_detail_in_one_cell() {
        let red = image::Rgb([255, 0, 0]);
        let blue = image::Rgb([0, 0, 255]);
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(4, 4, |x, _| {
            if x % 2 == 0 { red } else { blue }
        }));
        let thumbnail = render_thumbnail(&img, 2, 1);
        assert_eq!(
            thumbnail.cells,
            [Cell { ch: '▌', fg: rgb(&red), bg: Some(rgb(&blue)) }; 2],
        );
    }

    #[test]
    fn softpixel_bitmap_keeps_its_size_and_has_hard_pixel_edges() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_fn(12, 6, |x, y| {
            image::Rgb([(x * 20) as u8, (y * 40) as u8, 0])
        }));
        let smooth = bitmap(&image, 36, 18, Style::Image).to_rgb8();
        let soft = bitmap(&image, 36, 18, Style::SoftPixel).to_rgb8();
        assert_eq!(soft.dimensions(), smooth.dimensions());
        assert_eq!(soft.get_pixel(0, 0), soft.get_pixel(1, 0));
        assert_ne!(soft.get_pixel(1, 0), soft.get_pixel(3, 0));
        assert_ne!(smooth, soft);
    }

    #[test]
    fn default_bitmap_uses_a_finer_grid_for_the_large_cover() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_fn(256, 256, |x, _| {
            image::Rgb([x as u8, 0, 0])
        }));
        let samples = |size| {
            let image = bitmap(&image, size, size, Style::Pixel).to_rgb8();
            image.rows().next().expect("nonempty cover").map(|p| p.0[0])
                .collect::<std::collections::HashSet<_>>().len()
        };
        assert_eq!(samples(32), 10);
        assert_eq!(samples(360), 65);
    }

    #[test]
    fn wide_and_tall_covers_crop_to_the_center() {
        let center = image::Rgb([30, 200, 60]);
        let edge = image::Rgb([220, 40, 80]);
        for (w, h) in [(32, 16), (16, 32)] {
            let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
                let long_axis = if w > h { x } else { y };
                if (8..24).contains(&long_axis) { center } else { edge }
            }));
            for style in [Style::Pixel, Style::SoftPixel, Style::Image] {
                let output = bitmap(&img, 32, 32, style).to_rgb8();
                assert_eq!(output.dimensions(), (32, 32));
                assert!(output.pixels().all(|p| *p == center));
            }
            for output in [render(&img, Style::Pixel, 4, 2), render_thumbnail(&img, 4, 2)] {
                assert_eq!(output.cells.len(), 8);
                assert!(output.cells.iter().all(|cell| cell.bg == Some(rgb(&center))));
            }
        }
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
        assert_eq!(Style::Pixel.next().next().next().next(), Style::Pixel);
        assert_eq!(Style::parse("dots"), Style::Dots);
        assert_eq!(Style::parse("whatever"), Style::Pixel);
    }
}
