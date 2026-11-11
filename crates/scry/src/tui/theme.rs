//! Semantic tokens. Components ask for `muted` or `warn`, never for a colour, so a look is
//! one constructor here and the degraded variants come for free.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use scry_core::view::Tone;

use super::tty;

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

    /// WCAG relative luminance.
    fn luminance(self) -> f32 {
        let lin = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }

    /// WCAG contrast ratio, from 1 to 21.
    pub fn contrast(self, other: Rgb) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
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

/// How long to wait for the terminal's answer. Local terminals answer in a few
/// milliseconds; the wait ends early once the DA1 reply marks the answer complete.
const QUERY_WAIT: Duration = Duration::from_secs(1);

impl Colors {
    /// Returns early, with no colours, when `stop` is set during the query.
    pub fn detect(stop: &AtomicBool) -> Colors {
        let var = |k| std::env::var(k).ok().filter(|v| !v.is_empty());
        let ask = may_query(
            var("SSH_CONNECTION").is_some(),
            var("SCRY_COLOR_QUERY").as_deref(),
        );
        Colors::from_env(
            var("NO_COLOR").is_some(),
            var("COLORTERM").as_deref(),
            || ask.then(|| tty::query_colors(QUERY_WAIT, stop)).flatten(),
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

/// Over ssh a reply can take longer than the wait, so Scry does not ask there unless
/// `SCRY_COLOR_QUERY=1`. `SCRY_COLOR_QUERY=0` never asks.
fn may_query(ssh: bool, force: Option<&str>) -> bool {
    match force {
        Some("1") => true,
        Some("0") => false,
        _ => !ssh,
    }
}

/// How regions are set apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chrome {
    /// Thin rules between header, list, detail and footer.
    Rules,
    /// Tinted bands behind the header, detail and footer, no lines.
    Bands,
}

/// The looks compared in docs/design.md. Native is also what Tinted falls back to
/// without truecolor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Native,
    Tinted,
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
    /// Drawn in the column left of the selected row.
    pub cursor: (&'static str, Style),
    /// Background of bands, sheets and overlays.
    pub surface: Style,
    pub border: Style,
    pub key: Style,
    pub matched: Style,
    pub tab_active: Style,
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
    pub fn detect(stop: &AtomicBool) -> Theme {
        Theme::new(SHIPPED, Colors::detect(stop))
    }

    pub fn new(look: Look, colors: Colors) -> Theme {
        match (look, colors) {
            (Look::Tinted, Colors::True { bg, fg }) => tinted(bg, fg),
            (Look::Tinted, other) => Theme::new(Look::Native, other),
            (Look::Native, c) => native(c),
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
        // Reverse video turns this blank into a bar in the accent colour.
        cursor: (" ", paint(Color::Cyan)),
        surface: s,
        border: dim,
        key: bold,
        matched: paint(Color::Yellow).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        tab_active: paint(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        motion: None,
    }
}

/// The selected row's tint against the background: enough to see without the cursor glyph.
const SELECTION_MIN: f32 = 1.3;
/// Body text, muted text (labels, context) and faint marks (tree guides, separators), each
/// against every surface it sits on: background, bands and the selected row.
const TEXT_MIN: f32 = 6.0;
const MUTED_MIN: f32 = 4.5;
const FAINT_MIN: f32 = 3.0;

/// Each hue keeps 5:1 contrast or more on white, Solarized light and dark, black, Tomorrow
/// Night and One Dark backgrounds, and 3.7:1 on the selection tint. The neutrals are mixed
/// from the terminal's own background and foreground, then pushed further from the
/// background until each reaches its minimum contrast, so a low-contrast theme such as
/// Solarized still gets readable labels.
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
            Rgb(0x1b, 0x73, 0x40),
            Rgb(0x0c, 0x74, 0x80),
            Rgb(0x93, 0x57, 0x00),
            Rgb(0xc0, 0x2d, 0x3b),
        ]
    };
    let s = Style::new();
    let surface = bg.mix(fg, if dark { 0.07 } else { 0.05 });
    let mut t = if dark { 0.16 } else { 0.10 };
    while bg.mix(accent, t).contrast(bg) < SELECTION_MIN && t < 1.0 {
        t += 0.01;
    }
    let select = bg.mix(accent, t);
    let on = [bg, surface, select];
    let text = ink(bg, fg, 0.94, TEXT_MIN, &on);
    let muted = ink(bg, fg, 0.58, MUTED_MIN, &on);
    let faint = ink(bg, fg, 0.34, FAINT_MIN, &on);
    Theme {
        chrome: Chrome::Bands,
        text: s.fg(text.into()),
        muted: s.fg(muted.into()),
        faint: s.fg(faint.into()),
        strong: s.fg(text.into()).add_modifier(Modifier::BOLD),
        accent: s.fg(accent.into()),
        heading: s.fg(muted.into()).add_modifier(Modifier::BOLD),
        ok: s.fg(ok.into()),
        info: s.fg(info.into()),
        warn: s.fg(warn.into()),
        danger: s.fg(danger.into()).add_modifier(Modifier::BOLD),
        selection: s.bg(select.into()),
        cursor: ("▌", s.fg(accent.into())),
        surface: s.fg(text.into()).bg(surface.into()),
        border: s.fg(fg.mix(bg, 0.78).into()),
        key: s.fg(accent.into()).add_modifier(Modifier::BOLD),
        matched: s.fg(warn.into()).add_modifier(Modifier::BOLD),
        tab_active: s
            .fg(accent.into())
            .bg(select.into())
            .add_modifier(Modifier::BOLD),
        motion: Some(MotionColors {
            bg,
            fg: text,
            flash: bg.mix(warn, if dark { 0.30 } else { 0.22 }),
        }),
    }
}

/// A neutral on the line from `bg` (0) through `fg` (1) on to black or white (2): `from`
/// when that already reaches `min` contrast on every surface in `on`, else the first point
/// past it that does.
fn ink(bg: Rgb, fg: Rgb, from: f32, min: f32, on: &[Rgb]) -> Rgb {
    let far = if bg.luma() < fg.luma() {
        Rgb(255, 255, 255)
    } else {
        Rgb(0, 0, 0)
    };
    let at = |s: f32| {
        if s <= 1.0 {
            bg.mix(fg, s)
        } else {
            fg.mix(far, s - 1.0)
        }
    };
    let mut s = from;
    while s < 2.0 && on.iter().any(|&c| at(s).contrast(c) < min) {
        s += 0.01;
    }
    at(s.min(2.0))
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

    fn rgb(c: Option<Color>) -> Rgb {
        match c {
            Some(Color::Rgb(r, g, b)) => Rgb(r, g, b),
            other => panic!("expected truecolor, got {other:?}"),
        }
    }

    /// Light and dark terminals with low and high contrast: (background, foreground).
    const BACKGROUNDS: [(&str, Rgb, Rgb); 6] = [
        (
            "Terminal Basic",
            Rgb(0xff, 0xff, 0xff),
            Rgb(0x00, 0x00, 0x00),
        ),
        (
            "Solarized light",
            Rgb(0xfd, 0xf6, 0xe3),
            Rgb(0x65, 0x7b, 0x83),
        ),
        ("black", Rgb(0x00, 0x00, 0x00), Rgb(0xff, 0xff, 0xff)),
        (
            "Tomorrow Night",
            Rgb(0x1d, 0x1f, 0x21),
            Rgb(0xc5, 0xc8, 0xc6),
        ),
        ("One Dark", Rgb(0x28, 0x2c, 0x34), Rgb(0xab, 0xb2, 0xbf)),
        (
            "Solarized dark",
            Rgb(0x00, 0x2b, 0x36),
            Rgb(0x83, 0x94, 0x96),
        ),
    ];

    #[test]
    fn neutrals_reach_their_contrast_on_every_surface_in_every_theme() {
        for (name, bg, fg) in BACKGROUNDS {
            let t = Theme::new(Look::Tinted, Colors::True { bg, fg });
            let select = rgb(t.selection.bg);
            let surfaces = [bg, rgb(t.surface.bg), select];
            assert!(
                select.contrast(bg) >= SELECTION_MIN,
                "{name}: selection {:.2}:1",
                select.contrast(bg)
            );
            for (token, style, min) in [
                ("text", t.text, TEXT_MIN),
                ("muted", t.muted, MUTED_MIN),
                ("faint", t.faint, FAINT_MIN),
            ] {
                for s in surfaces {
                    let c = rgb(style.fg).contrast(s);
                    assert!(c >= min, "{name}: {token} on {s:?} is {c:.2}:1");
                }
            }
            for (token, style) in [
                ("ok", t.ok),
                ("info", t.info),
                ("warn", t.warn),
                ("danger", t.danger),
            ] {
                let hue = rgb(style.fg);
                assert!(hue.contrast(bg) >= 5.0, "{name}: {token} on background");
                assert!(hue.contrast(select) >= 3.7, "{name}: {token} on selection");
            }
        }
    }

    #[test]
    fn a_high_contrast_theme_keeps_the_designed_mixes() {
        let (bg, fg) = (BACKGROUNDS[0].1, BACKGROUNDS[0].2);
        let t = Theme::new(Look::Tinted, Colors::True { bg, fg });
        assert_eq!(rgb(t.text.fg), fg.mix(bg, 0.06));
    }

    #[test]
    fn the_colour_query_is_skipped_over_ssh_unless_forced() {
        assert!(may_query(false, None));
        assert!(!may_query(true, None));
        assert!(may_query(true, Some("1")));
        assert!(!may_query(false, Some("0")));
    }
}
