//! Semantic tokens. Components ask for `muted` or `warn`, never for a colour, so a look is
//! one constructor here and the degraded variants come for free.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use scry_core::view::Tone;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    fn luma(self) -> f32 {
        0.2126 * self.0 as f32 + 0.7152 * self.1 as f32 + 0.0722 * self.2 as f32
    }
}

impl From<Rgb> for Color {
    fn from(c: Rgb) -> Color {
        Color::Rgb(c.0, c.1, c.2)
    }
}

/// What the terminal can show, decided once at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Colors {
    /// `NO_COLOR`: weight and reverse video only.
    Mono,
    /// The terminal's own 16 colours, so the user's theme picks every hue.
    Ansi,
    /// 24-bit colour, tuned to the background the terminal reported.
    True { bg: Rgb, fg: Rgb },
}

const QUERY_TIMEOUT: Duration = Duration::from_millis(150);

impl Colors {
    pub fn detect() -> Colors {
        let var = |k| std::env::var(k).ok().filter(|v| !v.is_empty());
        Colors::from_env(
            var("NO_COLOR").is_some(),
            var("COLORTERM").as_deref(),
            query_terminal,
        )
    }

    /// Truecolor needs both `COLORTERM` and an answer to the background query; anything
    /// less falls back to the ANSI palette, which is right in every theme.
    pub fn from_env(
        no_color: bool,
        colorterm: Option<&str>,
        query: impl FnOnce() -> Option<(Rgb, Rgb)>,
    ) -> Colors {
        if no_color {
            return Colors::Mono;
        }
        if !matches!(colorterm, Some("truecolor" | "24bit")) {
            return Colors::Ansi;
        }
        match query() {
            Some((bg, fg)) => Colors::True { bg, fg },
            None => Colors::Ansi,
        }
    }
}

fn query_terminal() -> Option<(Rgb, Rgb)> {
    let mut options = terminal_colorsaurus::QueryOptions::default();
    options.timeout = QUERY_TIMEOUT;
    let p = terminal_colorsaurus::color_palette(options).ok()?;
    let rgb = |c: terminal_colorsaurus::Color| {
        let (r, g, b) = c.scale_to_8bit();
        Rgb(r, g, b)
    };
    Some((rgb(p.background), rgb(p.foreground)))
}

/// How regions are set apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chrome {
    /// Thin rules between header, list, detail and footer.
    Rules,
    /// Tinted bands behind the header, detail and footer, no lines.
    Bands,
    /// Rounded boxes around the list and the detail.
    Panels,
}

/// The looks compared in docs/design.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Native,
    Tinted,
    Panels,
}

pub const SHIPPED: Look = Look::Tinted;

#[derive(Clone, Debug)]
pub struct Theme {
    pub chrome: Chrome,
    pub text: Style,
    pub muted: Style,
    pub faint: Style,
    pub strong: Style,
    pub accent: Style,
    pub heading: Style,
    pub ok: Style,
    pub info: Style,
    pub warn: Style,
    pub danger: Style,
    pub selection: Style,
    /// Background of bands, sheets and overlays.
    pub surface: Style,
    pub border: Style,
    pub key: Style,
    pub matched: Style,
    pub tab_active: Style,
    /// States drawn as filled pills instead of glyph and word.
    pub pills: bool,
    /// Set only for truecolor, where fades can be computed.
    pub motion: Option<MotionColors>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MotionColors {
    pub bg: Rgb,
    pub fg: Rgb,
    pub flash: Rgb,
}

impl Theme {
    pub fn detect() -> Theme {
        Theme::new(SHIPPED, Colors::detect())
    }

    pub fn new(look: Look, colors: Colors) -> Theme {
        match (look, colors) {
            (Look::Tinted, Colors::True { bg, fg }) => tinted(bg, fg),
            (Look::Tinted, other) => Theme::new(Look::Native, other),
            (Look::Native, c) => native(c),
            (Look::Panels, c) => Theme {
                chrome: Chrome::Panels,
                pills: c != Colors::Mono,
                selection: Style::new().add_modifier(Modifier::BOLD),
                ..native(c)
            },
        }
    }

    pub fn tone(&self, tone: Tone) -> Style {
        match tone {
            Tone::Ok => self.ok,
            Tone::Info => self.info,
            Tone::Warn => self.warn,
            Tone::Danger => self.danger,
            Tone::Quiet => self.muted,
        }
    }

    /// A pill: the tone as background, for looks that use them.
    pub fn pill(&self, tone: Tone) -> Style {
        match (tone, self.tone(tone).fg) {
            (Tone::Quiet, _) | (_, None) => self.tone(tone).add_modifier(Modifier::REVERSED),
            (_, Some(c)) => Style::new().fg(Color::Black).bg(c),
        }
    }
}

pub fn glyph(tone: Tone) -> &'static str {
    match tone {
        Tone::Ok => "●",
        Tone::Info => "◆",
        Tone::Warn => "▲",
        Tone::Danger => "✕",
        Tone::Quiet => "○",
    }
}

fn native(colors: Colors) -> Theme {
    let s = Style::new();
    let dim = s.add_modifier(Modifier::DIM);
    let bold = s.add_modifier(Modifier::BOLD);
    let paint = |c: Color| if colors == Colors::Mono { s } else { s.fg(c) };
    Theme {
        chrome: Chrome::Rules,
        text: s,
        muted: dim,
        faint: dim,
        strong: bold,
        accent: paint(Color::Cyan),
        heading: dim,
        ok: paint(Color::Green),
        info: paint(Color::Cyan),
        warn: paint(Color::Yellow),
        danger: paint(Color::Red).add_modifier(Modifier::BOLD),
        selection: s.add_modifier(Modifier::REVERSED),
        surface: s,
        border: dim,
        key: bold,
        matched: paint(Color::Yellow).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        tab_active: paint(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        pills: false,
        motion: None,
    }
}

/// Hues chosen for contrast of at least 4.5:1 on common light and dark backgrounds; the
/// neutrals are mixed from the terminal's own background and foreground.
fn tinted(bg: Rgb, fg: Rgb) -> Theme {
    let dark = bg.luma() < fg.luma();
    let [accent, ok, info, warn, danger] = if dark {
        [
            Rgb(0x7d, 0xae, 0xff),
            Rgb(0x6c, 0xcb, 0x8b),
            Rgb(0x5f, 0xc4, 0xd0),
            Rgb(0xe5, 0xb5, 0x67),
            Rgb(0xf2, 0x77, 0x7a),
        ]
    } else {
        [
            Rgb(0x2f, 0x5f, 0xd0),
            Rgb(0x1e, 0x7f, 0x45),
            Rgb(0x10, 0x80, 0x8c),
            Rgb(0x9a, 0x5b, 0x00),
            Rgb(0xc6, 0x30, 0x3e),
        ]
    };
    let s = Style::new();
    let text = fg.mix(bg, 0.06);
    let surface = bg.mix(fg, if dark { 0.07 } else { 0.05 });
    let select = bg.mix(accent, if dark { 0.24 } else { 0.14 });
    Theme {
        chrome: Chrome::Bands,
        text: s.fg(text.into()),
        muted: s.fg(fg.mix(bg, 0.42).into()),
        faint: s.fg(fg.mix(bg, 0.66).into()),
        strong: s.fg(text.into()).add_modifier(Modifier::BOLD),
        accent: s.fg(accent.into()),
        heading: s.fg(fg.mix(bg, 0.42).into()).add_modifier(Modifier::BOLD),
        ok: s.fg(ok.into()),
        info: s.fg(info.into()),
        warn: s.fg(warn.into()),
        danger: s.fg(danger.into()).add_modifier(Modifier::BOLD),
        selection: s.bg(select.into()),
        surface: s.fg(text.into()).bg(surface.into()),
        border: s.fg(fg.mix(bg, 0.78).into()),
        key: s.fg(accent.into()).add_modifier(Modifier::BOLD),
        matched: s.fg(warn.into()).add_modifier(Modifier::BOLD),
        tab_active: s
            .fg(accent.into())
            .bg(select.into())
            .add_modifier(Modifier::BOLD),
        pills: false,
        motion: Some(MotionColors {
            bg,
            fg: text,
            flash: bg.mix(warn, if dark { 0.30 } else { 0.22 }),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK: (Rgb, Rgb) = (Rgb(0x1d, 0x1f, 0x21), Rgb(0xc5, 0xc8, 0xc6));

    #[test]
    fn truecolor_needs_colorterm_and_an_answer() {
        assert_eq!(
            Colors::from_env(false, Some("truecolor"), || Some(DARK)),
            Colors::True {
                bg: DARK.0,
                fg: DARK.1
            }
        );
        assert_eq!(
            Colors::from_env(false, Some("24bit"), || None),
            Colors::Ansi
        );
        assert_eq!(
            Colors::from_env(false, None, || panic!("must not query")),
            Colors::Ansi
        );
        assert_eq!(
            Colors::from_env(false, Some("yes"), || panic!("must not query")),
            Colors::Ansi
        );
        assert_eq!(
            Colors::from_env(true, Some("truecolor"), || panic!("must not query")),
            Colors::Mono
        );
    }

    #[test]
    fn tinted_degrades_to_the_ansi_palette_and_to_mono() {
        let ansi = Theme::new(Look::Tinted, Colors::Ansi);
        assert_eq!(ansi.chrome, Chrome::Rules);
        assert_eq!(ansi.warn.fg, Some(Color::Yellow));
        assert!(ansi.motion.is_none());
        let mono = Theme::new(Look::Tinted, Colors::Mono);
        for style in [
            mono.text,
            mono.accent,
            mono.ok,
            mono.warn,
            mono.danger,
            mono.selection,
        ] {
            assert_eq!((style.fg, style.bg), (None, None));
        }
    }

    #[test]
    fn tinted_picks_hues_for_the_reported_background() {
        let dark = Theme::new(
            Look::Tinted,
            Colors::True {
                bg: DARK.0,
                fg: DARK.1,
            },
        );
        let light = Theme::new(
            Look::Tinted,
            Colors::True {
                bg: Rgb(255, 255, 255),
                fg: Rgb(0, 0, 0),
            },
        );
        assert_ne!(dark.warn.fg, light.warn.fg);
        assert_eq!(dark.chrome, Chrome::Bands);
    }
}
