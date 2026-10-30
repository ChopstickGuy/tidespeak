//! Draws the character grid into an RGBA image. Glyphs come from JetBrains Mono
//! (rasterised with fontdue); frame, line and block characters are drawn as
//! shapes so they join seamlessly. Only cells that changed are redrawn.
//! The image is RGB without alpha, so the window can copy it without blending.

use std::collections::HashMap;

use fontdue::{Font, FontSettings};

use crate::grid::{Bg, Grid, C};

/// Cell size in logical pixels
pub const CELL_W: f32 = 8.0;
pub const CELL_H: f32 = 16.0;
const BACKGROUND: [u8; 3] = [20, 20, 22];

struct Style {
    rgb: [u8; 3],
    bold: bool,
    glow: bool,
}

fn style(c: C) -> Style {
    let s = |hex: u32, bold: bool, glow: bool| Style { rgb: [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8], bold, glow };
    match c {
        C::T => s(0xE6E6E6, false, false),
        C::B => s(0xFFFFFF, true, false),
        C::D => s(0xA1A1A2, false, false),
        C::F => s(0x646465, false, false),
        C::G => s(0x333334, false, false),
        C::Teal => s(0x22D3EE, false, false),
        C::TealB => s(0x22D3EE, true, true),
        C::TDim => s(0x1B7482, false, false),
        C::Purple => s(0xB77CF9, false, false),
        C::PDim => s(0x653892, false, false),
        C::Lav => s(0x8FA8F8, false, false),
        C::Red => s(0xFF7B63, false, false),
        C::RedB => s(0xFF7B63, true, true),
        C::RDim => s(0x7E4239, false, false),
        C::Amber => s(0xE5A50A, false, false),
        C::AmberB => s(0xE5A50A, true, true),
        C::Green => s(0x2EC27E, false, false),
        C::Blue => s(0x7DB4F5, false, false),
        C::LCur => s(0x20C0D8, false, false),
        C::LOth => s(0x9E6CD7, false, false),
        C::LUser => s(0xA6A6A7, false, false),
        C::LFar => s(0x777778, false, false),
        C::LMute => s(0xC46150, false, false),
        C::U0 => s(0x7DB4F5, true, false),
        C::U1 => s(0x57D9A3, true, false),
        C::U2 => s(0xF5C04A, true, false),
        C::U3 => s(0xF59A62, true, false),
        C::U4 => s(0xD88BE0, true, false),
        C::U5 => s(0x4FD1E0, true, false),
    }
}

fn bg_rgb(b: Bg) -> [u8; 3] {
    match b {
        Bg::None => BACKGROUND,
        Bg::Hover => [22, 48, 53],
        Bg::Mention => [25, 38, 58],
        Bg::Selected => [24, 69, 77],
    }
}

struct Mask {
    cov: Vec<u8>,
    glow: Vec<u8>,
}

pub struct Renderer {
    scale: f32,
    cw: usize,
    chh: usize,
    regular: Font,
    bold: Font,
    px: f32,
    baseline: f32,
    cache: HashMap<(char, bool), Mask>,
    prev: Vec<(char, C, Bg)>,
    cols: i32,
    rows: i32,
    pub buf: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Changed tiles since the last `take_tiles`
    dirty: Vec<bool>,
}

/// Tile size in cells: only changed tiles are handed to the window
pub const TILE_COLS: i32 = 24;
pub const TILE_ROWS: i32 = 8;

pub struct Tile {
    pub index: usize,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<u8>,
}

impl Renderer {
    pub fn new(scale: f32) -> Renderer {
        let regular = Font::from_bytes(&include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")[..], FontSettings::default()).expect("Schrift");
        let bold = Font::from_bytes(&include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf")[..], FontSettings::default()).expect("Schrift");
        let mut r = Renderer {
            scale: 0.0,
            cw: 0,
            chh: 0,
            regular,
            bold,
            px: 0.0,
            baseline: 0.0,
            cache: HashMap::new(),
            prev: Vec::new(),
            cols: 0,
            rows: 0,
            buf: Vec::new(),
            width: 0,
            height: 0,
            dirty: Vec::new(),
        };
        r.set_scale(scale);
        r
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() < 0.001 {
            return;
        }
        self.scale = scale;
        self.cw = (CELL_W * scale).round().max(4.0) as usize;
        self.chh = (CELL_H * scale).round().max(8.0) as usize;
        // JetBrains Mono: Vorschub 0,6 em – so füllt ein Zeichen genau eine Zelle
        self.px = self.cw as f32 / 0.6;
        let lm = self.regular.horizontal_line_metrics(self.px).expect("Schriftmaße");
        let text_h = lm.ascent - lm.descent;
        self.baseline = ((self.chh as f32 - text_h) / 2.0 + lm.ascent).round();
        self.cache.clear();
        self.prev.clear();
    }

    /// Draws the grid; returns whether anything changed
    pub fn render(&mut self, g: &Grid) -> bool {
        let (w, h) = (g.cols.max(1) as usize * self.cw, g.rows.max(1) as usize * self.chh);
        if g.cols != self.cols || g.rows != self.rows || self.buf.len() != w * h * 3 || self.prev.len() != g.ch.len() {
            self.cols = g.cols;
            self.rows = g.rows;
            self.width = w as u32;
            self.height = h as u32;
            self.buf = vec![0; w * h * 3];
            self.prev = vec![('\u{0}', C::T, Bg::None); g.ch.len()];
            self.dirty = vec![true; self.tiles_x() * self.tiles_y()];
        }
        let mut changed = false;
        for r in 0..g.rows {
            for c in 0..g.cols {
                let k = (r * g.cols + c) as usize;
                let cell = (g.ch[k], g.co[k], g.bg[k]);
                if self.prev[k] == cell {
                    continue;
                }
                self.prev[k] = cell;
                changed = true;
                let t = (r / TILE_ROWS) as usize * self.tiles_x() + (c / TILE_COLS) as usize;
                self.dirty[t] = true;
                self.draw_cell(r as usize, c as usize, cell.0, cell.1, cell.2);
            }
        }
        changed
    }

    pub fn tiles_x(&self) -> usize {
        ((self.cols + TILE_COLS - 1) / TILE_COLS).max(1) as usize
    }

    pub fn tiles_y(&self) -> usize {
        ((self.rows + TILE_ROWS - 1) / TILE_ROWS).max(1) as usize
    }

    /// Pixels of all tiles that changed (and marks them clean)
    pub fn take_tiles(&mut self) -> Vec<Tile> {
        let (tx, ty) = (self.tiles_x(), self.tiles_y());
        let mut out = Vec::new();
        for t in 0..tx * ty {
            if !self.dirty.get(t).copied().unwrap_or(false) {
                continue;
            }
            self.dirty[t] = false;
            let (cx, cy) = ((t % tx) as i32 * TILE_COLS, (t / tx) as i32 * TILE_ROWS);
            let x = cx as usize * self.cw;
            let y = cy as usize * self.chh;
            let w = (TILE_COLS.min(self.cols - cx)) as usize * self.cw;
            let h = (TILE_ROWS.min(self.rows - cy)) as usize * self.chh;
            let stride = self.width as usize * 3;
            let mut rgb = Vec::with_capacity(w * h * 3);
            for row in y..y + h {
                rgb.extend_from_slice(&self.buf[row * stride + x * 3..row * stride + (x + w) * 3]);
            }
            out.push(Tile { index: t, x: x as u32, y: y as u32, w: w as u32, h: h as u32, rgb });
        }
        out
    }

    fn draw_cell(&mut self, r: usize, c: usize, ch: char, co: C, bg: Bg) {
        let (cw, chh, stride) = (self.cw, self.chh, self.width as usize * 3);
        let b = bg_rgb(bg);
        let x0 = c * cw;
        let y0 = r * chh;
        for y in 0..chh {
            let row = (y0 + y) * stride + x0 * 3;
            self.buf[row..row + cw * 3].chunks_exact_mut(3).for_each(|p| p.copy_from_slice(&b));
        }
        if ch == ' ' {
            return;
        }
        let st = style(co);
        let key = (ch, st.bold);
        if !self.cache.contains_key(&key) {
            let m = self.make_mask(ch, st.bold);
            self.cache.insert(key, m);
        }
        let m = &self.cache[&key];
        let f = st.rgb;
        for y in 0..chh {
            let row = (y0 + y) * stride + x0 * 3;
            for x in 0..cw {
                let i = y * cw + x;
                let mut a = m.cov[i] as u32;
                if st.glow {
                    a = a.max(m.glow[i] as u32);
                }
                if a == 0 {
                    continue;
                }
                let p = row + x * 3;
                for ch in 0..3 {
                    let base = self.buf[p + ch] as u32;
                    self.buf[p + ch] = ((base * (255 - a) + f[ch] as u32 * a) / 255) as u8;
                }
            }
        }
    }

    fn make_mask(&self, ch: char, bold: bool) -> Mask {
        let (w, h) = (self.cw, self.chh);
        let mut cov = vec![0u8; w * h];
        if !shape(ch, w, h, self.scale, &mut cov) {
            let font = if bold { &self.bold } else { &self.regular };
            let glyph = if font.lookup_glyph_index(ch) == 0 { '?' } else { ch };
            let (m, bmp) = font.rasterize(glyph, self.px);
            let top = self.baseline as i32 - m.height as i32 - m.ymin;
            for gy in 0..m.height {
                for gx in 0..m.width {
                    let (x, y) = (m.xmin + gx as i32, top + gy as i32);
                    if x >= 0 && (x as usize) < w && y >= 0 && (y as usize) < h {
                        let v = bmp[gy * m.width + gx];
                        let i = y as usize * w + x as usize;
                        cov[i] = cov[i].max(v);
                    }
                }
            }
        }
        // weiches Leuchten: Abdeckung der Nachbarpixel, abgeschwächt
        let mut glow = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let mut m = 0u8;
                for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1), (-1, -1), (1, 1), (-1, 1), (1, -1)] {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx >= 0 && (nx as usize) < w && ny >= 0 && (ny as usize) < h {
                        m = m.max(cov[ny as usize * w + nx as usize]);
                    }
                }
                glow[y * w + x] = (m as u32 * 90 / 255) as u8;
            }
        }
        Mask { cov, glow }
    }
}

/// Frame, line, block and a few symbol characters as exact shapes. Returns false for normal glyphs.
fn shape(ch: char, w: usize, h: usize, scale: f32, cov: &mut [u8]) -> bool {
    let t = scale.round().max(1.0) as usize;
    let (cx, cy) = (w / 2, h / 2);
    let mut rect = |x0: usize, y0: usize, x1: usize, y1: usize, a: u8| {
        for y in y0.min(h)..y1.min(h) {
            for x in x0.min(w)..x1.min(w) {
                cov[y * w + x] = cov[y * w + x].max(a);
            }
        }
    };
    let hl = |rect: &mut dyn FnMut(usize, usize, usize, usize, u8), x0: usize, x1: usize, y: usize, th: usize| rect(x0, y.saturating_sub(th / 2), x1, y.saturating_sub(th / 2) + th, 255);
    let vl = |rect: &mut dyn FnMut(usize, usize, usize, usize, u8), y0: usize, y1: usize, x: usize, th: usize| rect(x.saturating_sub(th / 2), y0, x.saturating_sub(th / 2) + th, y1, 255);
    let g = (1.5 * scale).round().max(1.0) as usize + t / 2;
    match ch {
        '─' => hl(&mut rect, 0, w, cy, t),
        '━' => hl(&mut rect, 0, w, cy, t * 2),
        '│' => vl(&mut rect, 0, h, cx, t),
        '┌' => {
            hl(&mut rect, cx, w, cy, t);
            vl(&mut rect, cy, h, cx, t);
        }
        '┐' => {
            hl(&mut rect, 0, cx + t, cy, t);
            vl(&mut rect, cy, h, cx, t);
        }
        '└' => {
            hl(&mut rect, cx, w, cy, t);
            vl(&mut rect, 0, cy + t, cx, t);
        }
        '┘' => {
            hl(&mut rect, 0, cx + t, cy, t);
            vl(&mut rect, 0, cy + t, cx, t);
        }
        '├' => {
            vl(&mut rect, 0, h, cx, t);
            hl(&mut rect, cx, w, cy, t);
        }
        '┤' => {
            vl(&mut rect, 0, h, cx, t);
            hl(&mut rect, 0, cx, cy, t);
        }
        '═' => {
            hl(&mut rect, 0, w, cy - g, t);
            hl(&mut rect, 0, w, cy + g, t);
        }
        '║' => {
            vl(&mut rect, 0, h, cx - g, t);
            vl(&mut rect, 0, h, cx + g, t);
        }
        '╔' => {
            hl(&mut rect, cx - g, w, cy - g, t);
            hl(&mut rect, cx + g, w, cy + g, t);
            vl(&mut rect, cy - g, h, cx - g, t);
            vl(&mut rect, cy + g, h, cx + g, t);
        }
        '╗' => {
            hl(&mut rect, 0, cx + g + t, cy - g, t);
            hl(&mut rect, 0, cx - g + t, cy + g, t);
            vl(&mut rect, cy - g, h, cx + g, t);
            vl(&mut rect, cy + g, h, cx - g, t);
        }
        '╚' => {
            hl(&mut rect, cx - g, w, cy + g, t);
            hl(&mut rect, cx + g, w, cy - g, t);
            vl(&mut rect, 0, cy + g + t, cx - g, t);
            vl(&mut rect, 0, cy - g + t, cx + g, t);
        }
        '╝' => {
            hl(&mut rect, 0, cx + g + t, cy + g, t);
            hl(&mut rect, 0, cx - g + t, cy - g, t);
            vl(&mut rect, 0, cy + g + t, cx + g, t);
            vl(&mut rect, 0, cy - g + t, cx - g, t);
        }
        '╱' | '╲' => {
            let steps = h * 3;
            for i in 0..steps {
                let fy = i as f32 / steps as f32 * h as f32;
                let fx = if ch == '╱' { (h as f32 - fy) * w as f32 / h as f32 } else { fy * w as f32 / h as f32 };
                let (x, y) = (fx as usize, fy as usize);
                rect(x.saturating_sub(t / 2), y, x.saturating_sub(t / 2) + t, y + 1, 255);
            }
        }
        '█' => rect(0, 0, w, h, 255),
        '▓' => rect(0, 0, w, h, 190),
        '▒' => rect(0, 0, w, h, 125),
        '░' => rect(0, 0, w, h, 60),
        '▌' => rect(0, 0, cx, h, 255),
        '▐' => rect(cx, 0, w, h, 255),
        '▀' => rect(0, 0, w, cy, 255),
        '▄' => rect(0, cy, w, h, 255),
        '▁' | '▂' | '▃' | '▅' | '▆' | '▇' => {
            let eighths = match ch {
                '▁' => 1,
                '▂' => 2,
                '▃' => 3,
                '▅' => 5,
                '▆' => 6,
                _ => 7,
            };
            rect(0, h - h * eighths / 8, w, h, 255);
        }
        '◆' | '●' => {
            let r = w as f32 * 0.42;
            let (fx, fy) = (w as f32 / 2.0, h as f32 / 2.0);
            for y in 0..h {
                for x in 0..w {
                    let (dx, dy) = ((x as f32 + 0.5 - fx).abs(), (y as f32 + 0.5 - fy).abs());
                    let d = if ch == '◆' { (dx + dy) / r } else { (dx * dx + dy * dy).sqrt() / r };
                    if d <= 1.0 {
                        rect(x, y, x + 1, y + 1, 255);
                    }
                }
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_only_changes() {
        let mut r = Renderer::new(1.0);
        let mut g = Grid::new(4, 10);
        g.put(1, 1, "Tidespeak", C::TealB);
        g.frame(0, 0, 4, 10, C::D, true);
        assert!(r.render(&g));
        assert_eq!(r.width, 80);
        assert_eq!(r.height, 64);
        assert!(!r.render(&g), "unverändertes Raster zeichnet nichts neu");
        g.put(2, 2, "x", C::T);
        assert!(r.render(&g));
        // irgendein Pixel des Schriftzugs ist hell
        assert!(r.buf.chunks(3).any(|p| p[1] > 150));
    }
}
