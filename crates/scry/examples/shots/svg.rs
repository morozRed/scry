//! A ratatui buffer as SVG: every cell's colours and bold, dim, italic, reverse, underline
//! and strikethrough. Named colours go through a terminal palette; truecolor passes through.

use std::fmt::Write;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

pub type Rgb = (u8, u8, u8);

/// A terminal's colours: default background and foreground, then ANSI 0 to 15.
pub struct Palette {
    pub name: &'static str,
    pub bg: Rgb,
    pub fg: Rgb,
    pub ansi: [Rgb; 16],
}

const fn hex(v: u32) -> Rgb {
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// macOS Terminal's default profile, "Basic".
pub const TERMINAL_BASIC: Palette = Palette {
    name: "light",
    bg: hex(0xffffff),
    fg: hex(0x000000),
    ansi: [
        hex(0x000000),
        hex(0x990000),
        hex(0x00a600),
        hex(0x999900),
        hex(0x0000b2),
        hex(0xb200b2),
        hex(0x00a6b2),
        hex(0xbfbfbf),
        hex(0x666666),
        hex(0xe50000),
        hex(0x00d900),
        hex(0xe5e500),
        hex(0x0000ff),
        hex(0xe500e5),
        hex(0x00e5e5),
        hex(0xe5e5e5),
    ],
};

/// Tomorrow Night, a common dark palette.
pub const TOMORROW_NIGHT: Palette = Palette {
    name: "dark",
    bg: hex(0x1d1f21),
    fg: hex(0xc5c8c6),
    ansi: [
        hex(0x1d1f21),
        hex(0xcc6666),
        hex(0xb5bd68),
        hex(0xf0c674),
        hex(0x81a2be),
        hex(0xb294bb),
        hex(0x8abeb7),
        hex(0xc5c8c6),
        hex(0x666666),
        hex(0xd54e53),
        hex(0xb9ca4a),
        hex(0xe7c547),
        hex(0x7aa6da),
        hex(0xc397d8),
        hex(0x70c0b1),
        hex(0xeaeaea),
    ],
};

/// Solarized light, a low-contrast light palette.
pub const SOLARIZED_LIGHT: Palette = Palette {
    name: "solarized-light",
    bg: hex(0xfdf6e3),
    fg: hex(0x657b83),
    ansi: [
        hex(0x073642),
        hex(0xdc322f),
        hex(0x859900),
        hex(0xb58900),
        hex(0x268bd2),
        hex(0xd33682),
        hex(0x2aa198),
        hex(0xeee8d5),
        hex(0x002b36),
        hex(0xcb4b16),
        hex(0x586e75),
        hex(0x657b83),
        hex(0x839496),
        hex(0x6c71c4),
        hex(0x93a1a1),
        hex(0xfdf6e3),
    ],
};

const CELL_W: f32 = 8.4;
const CELL_H: f32 = 18.0;
const BASELINE: f32 = 13.5;
const PAD: f32 = 14.0;
const FONT: &str = r#"ui-monospace,SFMono-Regular,"SF Mono",Menlo,Consolas,"Liberation Mono","DejaVu Sans Mono",monospace"#;

impl Palette {
    fn resolve(&self, c: Color, default: Rgb) -> Rgb {
        match c {
            Color::Reset => default,
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Indexed(i) => self.indexed(i),
            named => {
                self.ansi[match named {
                    Color::Black => 0,
                    Color::Red => 1,
                    Color::Green => 2,
                    Color::Yellow => 3,
                    Color::Blue => 4,
                    Color::Magenta => 5,
                    Color::Cyan => 6,
                    Color::Gray => 7,
                    Color::DarkGray => 8,
                    Color::LightRed => 9,
                    Color::LightGreen => 10,
                    Color::LightYellow => 11,
                    Color::LightBlue => 12,
                    Color::LightMagenta => 13,
                    Color::LightCyan => 14,
                    _ => 15,
                }]
            }
        }
    }

    fn indexed(&self, i: u8) -> Rgb {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                (level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            _ => {
                let v = 8 + (i - 232) * 10;
                (v, v, v)
            }
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
struct Ink {
    fg: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

pub fn render(buf: &Buffer, pal: &Palette) -> String {
    let area = buf.area;
    let w = area.width as f32 * CELL_W + 2.0 * PAD;
    let h = area.height as f32 * CELL_H + 2.0 * PAD;
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">"#
    );
    let _ = write!(
        out,
        r#"<style>text{{font-family:{FONT};font-size:14px;white-space:pre}}</style><rect width="{w}" height="{h}" rx="8" fill="{}"/><g transform="translate({PAD} {PAD})">"#,
        css(pal.bg)
    );

    for y in 0..area.height {
        let mut cells = Vec::with_capacity(area.width as usize);
        for x in 0..area.width {
            let c = &buf[(area.x + x, area.y + y)];
            let m = c.modifier;
            let (mut fg, mut bg) = (pal.resolve(c.fg, pal.fg), pal.resolve(c.bg, pal.bg));
            if m.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if m.contains(Modifier::DIM) {
                fg = mix(fg, bg, 0.5);
            }
            let ink = Ink {
                fg,
                bold: m.contains(Modifier::BOLD),
                italic: m.contains(Modifier::ITALIC),
                underline: m.contains(Modifier::UNDERLINED),
                strike: m.contains(Modifier::CROSSED_OUT),
            };
            let text = if m.contains(Modifier::HIDDEN) {
                " "
            } else {
                c.symbol()
            };
            cells.push((bg, ink, text.to_string()));
        }

        let top = y as f32 * CELL_H;
        let mut x = 0;
        while x < cells.len() {
            let bg = cells[x].0;
            let start = x;
            while x < cells.len() && cells[x].0 == bg {
                x += 1;
            }
            if bg != pal.bg {
                let _ = write!(
                    out,
                    r#"<rect x="{:.1}" y="{top}" width="{:.1}" height="{CELL_H}" fill="{}"/>"#,
                    start as f32 * CELL_W,
                    (x - start) as f32 * CELL_W + 0.4,
                    css(bg)
                );
            }
        }

        // A run is cells with the same ink, cut at gaps of two or more blanks: renderers
        // disagree on collapsing spaces, so each piece is placed at its own column.
        let mut x = 0;
        while x < cells.len() {
            let ink = cells[x].1;
            let start = x;
            while x < cells.len()
                && cells[x].1 == ink
                && !(blank(&cells[x].2) && cells.get(x + 1).is_none_or(|c| blank(&c.2)))
            {
                x += 1;
            }
            let end = x.max(start + 1);
            x = end;
            let text: String = cells[start..end].iter().map(|c| c.2.as_str()).collect();
            let lead = text.len() - text.trim_start().len();
            let shown = text.trim();
            if shown.is_empty() {
                continue;
            }
            let cols = shown.chars().count() as f32;
            let _ = write!(
                out,
                r#"<text x="{:.1}" y="{:.1}" fill="{}" textLength="{:.1}" lengthAdjust="spacingAndGlyphs" xml:space="preserve"{}{}{}>{}</text>"#,
                (start + lead) as f32 * CELL_W,
                top + BASELINE,
                css(ink.fg),
                cols * CELL_W,
                if ink.bold {
                    r#" font-weight="700""#
                } else {
                    ""
                },
                if ink.italic {
                    r#" font-style="italic""#
                } else {
                    ""
                },
                match (ink.underline, ink.strike) {
                    (true, true) => r#" text-decoration="underline line-through""#,
                    (true, false) => r#" text-decoration="underline""#,
                    (false, true) => r#" text-decoration="line-through""#,
                    _ => "",
                },
                escape(shown)
            );
        }
    }
    out.push_str("</g></svg>\n");
    out
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

fn css(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn blank(s: &str) -> bool {
    s.trim().is_empty()
}
