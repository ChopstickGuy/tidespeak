//! Character grid for the ASCII interface: cells with a character, a colour and an
//! optional background, plus drawing helpers (frames, lines, hexagons) and the
//! small 3D renderers (tide wave, wireframe prism).

/// Colours of the palette (the renderer maps them to RGB)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
#[repr(u8)]
pub enum C {
    /// Normal text
    #[default]
    T,
    /// Bright, bold
    B,
    /// Dimmed
    D,
    /// Faint
    F,
    /// Ghost (barely visible)
    G,
    Teal,
    TealB,
    TDim,
    Purple,
    PDim,
    Lav,
    Red,
    RedB,
    RDim,
    Amber,
    AmberB,
    Green,
    Blue,
    LCur,
    LOth,
    LUser,
    LFar,
    LMute,
    U0,
    U1,
    U2,
    U3,
    U4,
    U5,
}

/// Cell background
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
#[repr(u8)]
pub enum Bg {
    #[default]
    None,
    Hover,
    Mention,
    Selected,
}

#[derive(Clone)]
pub struct Grid {
    pub cols: i32,
    pub rows: i32,
    pub ch: Vec<char>,
    pub co: Vec<C>,
    pub bg: Vec<Bg>,
}

impl Grid {
    pub fn new(rows: i32, cols: i32) -> Grid {
        let n = (rows.max(0) * cols.max(0)) as usize;
        Grid { cols, rows, ch: vec![' '; n], co: vec![C::T; n], bg: vec![Bg::None; n] }
    }

    #[inline]
    fn idx(&self, r: i32, c: i32) -> Option<usize> {
        (r >= 0 && r < self.rows && c >= 0 && c < self.cols).then(|| (r * self.cols + c) as usize)
    }

    pub fn get(&self, r: i32, c: i32) -> char {
        self.idx(r, c).map(|k| self.ch[k]).unwrap_or(' ')
    }

    pub fn set(&mut self, r: i32, c: i32, ch: char, co: C) {
        if let Some(k) = self.idx(r, c) {
            self.ch[k] = ch;
            self.co[k] = co;
        }
    }

    pub fn put(&mut self, r: i32, c: i32, s: &str, co: C) -> i32 {
        let mut x = c;
        for ch in s.chars() {
            self.set(r, x, ch, co);
            x += 1;
        }
        x
    }

    pub fn putf(&mut self, r: f32, c: f32, s: &str, co: C) {
        self.put(r.round() as i32, c.round() as i32, s, co);
    }

    pub fn segs(&mut self, r: i32, c: i32, list: &[(&str, C)]) -> i32 {
        let mut x = c;
        for (s, co) in list {
            x = self.put(r, x, s, *co);
        }
        x
    }

    pub fn center(&mut self, r: i32, c: i32, s: &str, co: C) {
        self.put(r, c - s.chars().count() as i32 / 2, s, co);
    }

    pub fn right(&mut self, r: i32, end: i32, s: &str, co: C) {
        self.put(r, end - s.chars().count() as i32 + 1, s, co);
    }

    pub fn clear(&mut self, r: i32, c: i32, h: i32, w: i32) {
        for rr in r..r + h {
            for cc in c..c + w {
                if let Some(k) = self.idx(rr, cc) {
                    self.ch[k] = ' ';
                    self.bg[k] = Bg::None;
                }
            }
        }
    }

    pub fn fill_bg(&mut self, r: i32, c: i32, h: i32, w: i32, bg: Bg) {
        for rr in r..r + h {
            for cc in c..c + w {
                if let Some(k) = self.idx(rr, cc) {
                    self.bg[k] = bg;
                }
            }
        }
    }

    pub fn recolor(&mut self, co: C, r0: i32, r1: i32, c0: i32, c1: i32) {
        for r in r0..=r1 {
            for c in c0..=c1 {
                if let Some(k) = self.idx(r, c) {
                    if self.ch[k] != ' ' {
                        self.co[k] = co;
                    }
                }
            }
        }
    }

    pub fn copy_from(&mut self, src: &Grid, r0: i32, r1: i32, c0: i32, c1: i32) {
        for r in r0.max(0)..=r1.min(self.rows - 1).min(src.rows - 1) {
            for c in c0.max(0)..=c1.min(self.cols - 1).min(src.cols - 1) {
                let k = (r * self.cols + c) as usize;
                let ks = (r * src.cols + c) as usize;
                self.ch[k] = src.ch[ks];
                self.co[k] = src.co[ks];
                self.bg[k] = src.bg[ks];
            }
        }
    }

    /// Like `copy_from`, but skips empty cells and shifts by `dc` columns
    pub fn overlay(&mut self, src: &Grid, r0: i32, r1: i32, c0: i32, c1: i32, dc: i32) {
        for r in r0.max(0)..=r1.min(src.rows - 1) {
            for c in c0.max(0)..=c1.min(src.cols - 1) {
                let ks = (r * src.cols + c) as usize;
                if src.ch[ks] == ' ' && src.bg[ks] == Bg::None {
                    continue;
                }
                if let Some(k) = self.idx(r, c + dc) {
                    self.ch[k] = src.ch[ks];
                    self.co[k] = src.co[ks];
                    self.bg[k] = src.bg[ks];
                }
            }
        }
    }

    pub fn frame(&mut self, r: i32, c: i32, h: i32, w: i32, co: C, double: bool) {
        let k = if double { ['╔', '╗', '╚', '╝', '═', '║'] } else { ['┌', '┐', '└', '┘', '─', '│'] };
        for x in c + 1..c + w - 1 {
            self.set(r, x, k[4], co);
            self.set(r + h - 1, x, k[4], co);
        }
        for y in r + 1..r + h - 1 {
            self.set(y, c, k[5], co);
            self.set(y, c + w - 1, k[5], co);
        }
        self.set(r, c, k[0], co);
        self.set(r, c + w - 1, k[1], co);
        self.set(r + h - 1, c, k[2], co);
        self.set(r + h - 1, c + w - 1, k[3], co);
    }

    /// Panel: cleared, framed, with a title in brackets
    pub fn panel(&mut self, r: i32, c: i32, h: i32, w: i32, co: C, double: bool, title: &str, tco: C) {
        self.clear(r, c, h, w);
        self.frame(r, c, h, w, co, double);
        if !title.is_empty() {
            self.put(r, c + 2, &format!("[ {title} ]"), tco);
        }
    }

    pub fn line(&mut self, r1: f32, c1: f32, r2: f32, c2: f32, co: C, ch: Option<char>, frac: f32) {
        let (dr, dc) = (r2 - r1, c2 - c1);
        let n = (dr.round().abs()).max(dc.round().abs()).max(1.0) as i32;
        let first = ch.unwrap_or_else(|| slope_char(dr, dc));
        let m = (n as f32 * frac).round() as i32;
        let mut prev: Option<(i32, i32)> = None;
        for i in 0..=m {
            let t = i as f32 / n as f32;
            let (r, c) = ((r1 + dr * t).round() as i32, (c1 + dc * t).round() as i32);
            // Treppenlinie: waagerecht/senkrecht, an jedem Versatz ein Schrägstrich
            let sym = match (ch, prev) {
                (Some(x), _) => x,
                (None, None) => first,
                (None, Some((pr, pc))) if pr != r && pc != c => {
                    if (r > pr) == (c > pc) {
                        '╲'
                    } else {
                        '╱'
                    }
                }
                (None, Some((pr, _))) if pr != r => '│',
                (None, Some(_)) => '─',
            };
            self.set(r, c, sym, co);
            prev = Some((r, c));
        }
    }

    /// Hexagon, 5 rows high; (r, c) = centre, w = width; three text lines inside
    pub fn hex(&mut self, r: i32, c: i32, w: i32, co: C, lines: &[(&str, C)]) {
        let (top, left) = (r - 2, c - w / 2);
        self.clear(top, left, 5, w);
        self.put(top, left + 2, &"_".repeat((w - 4) as usize), co);
        self.set(top + 1, left + 1, '/', co);
        self.set(top + 1, left + w - 2, '\\', co);
        self.set(top + 2, left, '/', co);
        self.set(top + 2, left + w - 1, '\\', co);
        self.set(top + 3, left, '\\', co);
        self.set(top + 3, left + w - 1, '/', co);
        self.put(top + 4, left + 1, &format!("\\{}/", "_".repeat((w - 4) as usize)), co);
        for (i, (s, lco)) in lines.iter().enumerate() {
            self.center(top + 1 + i as i32, left + w / 2, s, *lco);
        }
    }
}

/// Line character by slope (cells are about twice as high as wide)
pub fn slope_char(dr: f32, dc: f32) -> char {
    let (vx, vy) = (dc / 2.05, dr);
    let mut a = vy.atan2(vx).abs().to_degrees();
    if a > 90.0 {
        a = 180.0 - a;
    }
    if a < 25.0 {
        '─'
    } else if a > 65.0 {
        '│'
    } else if (vy > 0.0) == (vx > 0.0) {
        '╲'
    } else {
        '╱'
    }
}

pub fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

pub fn ease(x: f32) -> f32 {
    let x = clamp01(x);
    x * x * (3.0 - 2.0 * x)
}

/// Small, deterministic random numbers (same seed = same sequence)
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Rng {
        Rng(seed)
    }
    pub fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0 as f32 / 4_294_967_296.0
    }
}

/// Text "decrypts" from left to right: the rest are random characters (brackets and spaces stay)
pub fn decode(s: &str, p: f32, seed: u32) -> String {
    let n = s.chars().count().max(1) as f32;
    let mut rng = Rng::new((p * 40.0) as u32 + seed);
    let noise: Vec<char> = "#%&@$*+=/\\".chars().collect();
    s.chars()
        .enumerate()
        .map(|(i, ch)| {
            if (i as f32) < p * n || ch == ' ' || ch == '[' || ch == ']' {
                ch
            } else {
                noise[(rng.next() * noise.len() as f32) as usize % noise.len()]
            }
        })
        .collect()
}

/// Word wrap to `w` characters (long words are cut)
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in s.split('\n') {
        let mut cur = String::new();
        for word in para.split(' ') {
            let mut word = word.to_string();
            while word.chars().count() > w {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                let head: String = word.chars().take(w).collect();
                word = word.chars().skip(w).collect();
                out.push(head);
            }
            let len = cur.chars().count() + word.chars().count() + usize::from(!cur.is_empty());
            if len > w && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(&word);
        }
        out.push(cur);
    }
    out
}

/// Cuts a string to `n` characters (with … when shortened)
pub fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

pub struct TideOpts {
    pub amp: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub density: f32,
    pub seed: u32,
    pub ghost: bool,
}

impl Default for TideOpts {
    fn default() -> Self {
        TideOpts { amp: 1.0, yaw: 0.45, pitch: -0.62, density: 1.0, seed: 3, ghost: false }
    }
}

/// Resting phase of the server wave
pub const TIDE_T: f32 = 6.0;

/// The server core: a water surface of characters. Height picks the character,
/// depth the colour (teal in front, purple at the back), crests are white.
pub fn tide(g: &mut Grid, r0: f32, c0: f32, sc: f32, t: f32, o: &TideOpts) {
    let (cyw, syw) = (o.yaw.cos(), o.yaw.sin());
    let (cp, sp) = (o.pitch.cos(), o.pitch.sin());
    let ramp: Vec<char> = ".,-~:;=!*#$@".chars().collect();
    let mut zb = vec![f32::MAX; (g.rows * g.cols).max(0) as usize];
    let mut rng = Rng::new(o.seed);
    let sx = (0.4 / (2.05 * sc)).min(0.035);
    let sz = (0.9 / (2.05 * sc)).min(0.07);
    let mut x = -1.8f32;
    while x <= 1.8 {
        let mut z = -1.8f32;
        while z <= 1.8 {
            let skip = o.density < 1.0 && rng.next() > o.density;
            if !skip {
                let y = o.amp * (0.38 * (2.2 * x + t).sin() + 0.24 * (1.7 * z - 0.8 * t + 0.6 * x).sin());
                let x1 = x * cyw + z * syw;
                let z1 = -x * syw + z * cyw;
                let y2 = y * cp - z1 * sp;
                let d = y * sp + z1 * cp;
                let f = 10.0 / (d + 10.0);
                let rr = (r0 - sc * y2 * f).round() as i32;
                let cc = (c0 + 2.05 * sc * x1 * f).round() as i32;
                if let Some(k) = g.idx(rr, cc) {
                    if d < zb[k] {
                        zb[k] = d;
                        let hgt = clamp01((y / o.amp.max(0.05) + 0.62) / 1.24);
                        g.ch[k] = if o.amp < 0.04 { '-' } else { ramp[((hgt * 12.0) as usize).min(11)] };
                        g.co[k] = if o.ghost {
                            C::G
                        } else if hgt > 0.78 && o.amp > 0.2 {
                            C::B
                        } else if d < -0.6 {
                            C::Teal
                        } else if d < 0.6 {
                            C::Lav
                        } else {
                            C::Purple
                        };
                    }
                }
            }
            z += sz;
        }
        x += sx;
    }
}

fn rot(p: [f32; 3], ax: f32, ay: f32) -> [f32; 3] {
    let [x, y, z] = p;
    let y1 = y * ax.cos() - z * ax.sin();
    let z1 = y * ax.sin() + z * ax.cos();
    [x * ay.cos() + z1 * ay.sin(), y1, -x * ay.sin() + z1 * ay.cos()]
}

fn proj(p: [f32; 3], r0: f32, c0: f32, sc: f32) -> (f32, f32) {
    let f = 4.0 / (p[2] + 4.0);
    (r0 - sc * p[1] * f, c0 + 2.05 * sc * p[0] * f)
}

pub fn wire(g: &mut Grid, v: &[[f32; 3]], e: &[(usize, usize)], r0: f32, c0: f32, sc: f32, ax: f32, ay: f32, co: C, vco: C) {
    let p: Vec<(f32, f32)> = v.iter().map(|&q| proj(rot(q, ax, ay), r0, c0, sc)).collect();
    for &(a, b) in e {
        g.line(p[a].0, p[a].1, p[b].0, p[b].1, co, None, 1.0);
    }
    for q in &p {
        g.putf(q.0, q.1, "+", vco);
    }
}

/// Hexagonal prism as wireframe (channel switch)
pub fn prism(g: &mut Grid, r0: f32, c0: f32, sc: f32, ax: f32, ay: f32, co: C, vco: C) {
    let mut v = Vec::new();
    for z in [-0.35f32, 0.35] {
        for k in 0..6 {
            let a = k as f32 * std::f32::consts::PI / 3.0;
            v.push([a.cos(), a.sin(), z]);
        }
    }
    let mut e = Vec::new();
    for k in 0..6 {
        e.push((k, (k + 1) % 6));
        e.push((6 + k, 6 + (k + 1) % 6));
        e.push((k, 6 + k));
    }
    wire(g, &v, &e, r0, c0, sc, ax, ay, co, vco);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_clip() {
        let mut g = Grid::new(3, 5);
        g.put(1, 3, "abc", C::T);
        assert_eq!(g.get(1, 3), 'a');
        assert_eq!(g.get(1, 4), 'b');
        g.put(-1, 0, "x", C::T);
        g.put(5, 0, "x", C::T);
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("eins zwei drei", 9), vec!["eins zwei", "drei"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(cut("Frametide Crew", 8), "Frameti…");
    }

    #[test]
    fn decode_keeps_length_and_ends_clean() {
        let s = "AF03 6B71";
        assert_eq!(decode(s, 0.3, 1).chars().count(), s.chars().count());
        assert_eq!(decode(s, 1.0, 1), s);
    }

    #[test]
    fn tide_draws_inside() {
        let mut g = Grid::new(20, 60);
        tide(&mut g, 10.0, 30.0, 3.0, TIDE_T, &TideOpts::default());
        let n = g.ch.iter().filter(|c| **c != ' ').count();
        assert!(n > 100, "{n} Zeichen");
    }
}
