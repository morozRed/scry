//! Semantic tokens. Components ask for `muted` or `warn`, never for a colour, so a look is
//! one constructor here and the degraded variants come for free.

use ratatui::style::{Color, Modifier, Style};
use scry_core::disk::catalog::Kind;
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

/// Terminals known to answer the colour query at once, by `TERM_PROGRAM` or, for those
/// that do not set it, by `TERM`.
const ANSWER_AT_ONCE: [(&str, &str); 5] = [
    ("ghostty", "xterm-ghostty"),
    ("iTerm.app", ""),
    ("Apple_Terminal", ""),
    ("WezTerm", ""),
    ("kitty", "xterm-kitty"),
];

/// What a terminal Scry did not ask is taken to show: Terminal's Basic profile when light,
/// a neutral dark grey when dark. As `(background, foreground)`.
const ASSUMED_LIGHT: (Rgb, Rgb) = (Rgb(0xff, 0xff, 0xff), Rgb(0x00, 0x00, 0x00));
const ASSUMED_DARK: (Rgb, Rgb) = (Rgb(0x1c, 0x1c, 0x1c), Rgb(0xd0, 0xd0, 0xd0));

/// The environment variables that decide the colours, read once at startup.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub no_color: bool,
    pub colorterm: Option<String>,
    pub term_program: Option<String>,
    pub term: Option<String>,
    /// `SSH_CONNECTION` or `SSH_TTY` is set.
    pub ssh: bool,
    /// `SCRY_COLOR_QUERY`: `1` always asks, `0` never does.
    pub ask: Option<String>,
    /// `COLORFGBG`, as rxvt and others set it: `fg;bg` in ANSI colour numbers.
    pub colorfgbg: Option<String>,
}

impl Env {
    pub fn read() -> Env {
        let var = |k| std::env::var(k).ok().filter(|v| !v.is_empty());
        Env {
            no_color: var("NO_COLOR").is_some(),
            colorterm: var("COLORTERM"),
            term_program: var("TERM_PROGRAM"),
            term: var("TERM"),
            ssh: var("SSH_CONNECTION").is_some() || var("SSH_TTY").is_some(),
            ask: var("SCRY_COLOR_QUERY"),
            colorfgbg: var("COLORFGBG"),
        }
    }

    fn truecolor(&self) -> bool {
        matches!(self.colorterm.as_deref(), Some("truecolor" | "24bit"))
    }

    /// Only a local terminal known to answer at once is asked, so no answer can arrive after
    /// Scry stopped waiting. Over ssh, in tmux or in an unknown terminal an answer may be
    /// slow or never come.
    pub fn may_ask(&self) -> bool {
        match self.ask.as_deref() {
            Some("1") => true,
            Some("0") => false,
            _ => {
                let is = |v: &Option<String>, name: &str| {
                    !name.is_empty() && v.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(name))
                };
                !self.ssh
                    && ANSWER_AT_ONCE.iter().any(|(program, term)| {
                        is(&self.term_program, program) || is(&self.term, term)
                    })
            }
        }
    }

    /// Whether `COLORFGBG` names a dark background: ANSI 0 to 6 and 8 are dark.
    fn dark_background(&self) -> Option<bool> {
        let bg: u8 = self
            .colorfgbg
            .as_deref()?
            .rsplit(';')
            .next()?
            .parse()
            .ok()?;
        Some(matches!(bg, 0..=6 | 8))
    }
}

impl Colors {
    /// Truecolor needs `COLORTERM`; without it the ANSI palette is right in every theme.
    /// With it, `ask` runs only where [`Env::may_ask`] allows, and when it does not run or
    /// gets no answer, light or dark comes from `COLORFGBG`, then from `dark`, the macOS
    /// appearance, then dark.
    pub fn detect(
        env: &Env,
        ask: impl FnOnce() -> Option<(Rgb, Rgb)>,
        dark: impl FnOnce() -> Option<bool>,
    ) -> Colors {
        if env.no_color {
            return Colors::Mono;
        }
        if !env.truecolor() {
            return Colors::Ansi;
        }
        if env.may_ask()
            && let Some((bg, fg)) = ask()
        {
            return Colors::True { bg, fg };
        }
        let dark = env.dark_background().or_else(dark).unwrap_or(true);
        let (bg, fg) = if dark { ASSUMED_DARK } else { ASSUMED_LIGHT };
        Colors::True { bg, fg }
    }
}

/// How overlays are edged. The screen itself is set apart by rules in every look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chrome {
    /// Lines around overlays, which have no fill of their own.
    Rules,
    /// Overlays edged by their raised fill, no lines.
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
    /// What the terminal shows, for the tokens computed on demand: chips and tiles.
    pub colors: Colors,
    pub chrome: Chrome,
    pub text: Style,
    pub muted: Style,
    pub faint: Style,
    pub strong: Style,
    pub accent: Style,
    pub heading: Style,
    /// A section's title: the list's name over it, and each section of the detail pane.
    pub title: Style,
    pub ok: Style,
    pub info: Style,
    pub warn: Style,
    pub danger: Style,
    pub selection: Style,
    /// Background of what is drawn over the screen: the sheets, the help card, the palette
    /// and quiet chips.
    pub raised: Style,
    pub border: Style,
    pub key: Style,
    pub matched: Style,
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

    /// A filled chip in the detail pane: the tone's hue behind the terminal's background
    /// colour. A quiet chip sits on the raised surface. Without truecolor, reverse video.
    pub fn chip(&self, tone: Tone) -> Style {
        let bold = Modifier::BOLD;
        match self.colors {
            Colors::True { .. } if tone == Tone::Quiet => self.raised,
            Colors::True { bg, .. } => match self.tone(tone).fg {
                Some(hue) => Style::new().bg(hue).fg(bg.into()).add_modifier(bold),
                None => self.raised,
            },
            Colors::Ansi if tone == Tone::Quiet => Style::new().add_modifier(Modifier::REVERSED),
            Colors::Ansi => self.tone(tone).add_modifier(Modifier::REVERSED | bold),
            Colors::Mono => Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// A chip in a disk kind's hue.
    pub fn kind_chip(&self, kind: Kind) -> Style {
        let bold = Modifier::BOLD;
        match self.colors {
            Colors::True { bg, .. } => Style::new()
                .bg(kind_hue(kind, self.dark()).into())
                .fg(bg.into())
                .add_modifier(bold),
            Colors::Ansi => Style::new()
                .fg(kind_ansi(kind))
                .add_modifier(Modifier::REVERSED | bold),
            Colors::Mono => Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// A swatch of a kind's hue, as the legend and the bars in the pane draw it.
    pub fn kind(&self, kind: Kind) -> Style {
        match self.colors {
            Colors::True { .. } => Style::new().fg(kind_hue(kind, self.dark()).into()),
            Colors::Ansi => Style::new().fg(kind_ansi(kind)),
            Colors::Mono => self.text,
        }
    }

    /// How one treemap tile is drawn: its label bar in the kind's hue, a tinted fill, and the
    /// hatch over what Scry can clean up.
    pub fn tile(&self, kind: Kind, selected: bool) -> Tile {
        match self.colors {
            Colors::True { bg, fg } => {
                let dark = self.dark();
                let hue = kind_hue(kind, dark);
                let fill = bg.mix(
                    hue,
                    match (dark, selected) {
                        (true, false) => 0.20,
                        (true, true) => 0.34,
                        (false, false) => 0.13,
                        (false, true) => 0.24,
                    },
                );
                let text = self.text.fg.map_or(fg, rgb_of);
                let ink = if selected { bg } else { text };
                // As much hue as the label's text allows at 4.5:1.
                let mut t: f32 = if dark { 0.45 } else { 0.30 };
                while !selected && t > 0.1 && text.contrast(bg.mix(hue, t)) < 4.5 {
                    t -= 0.02;
                }
                let bar = if selected { hue } else { bg.mix(hue, t) };
                Tile {
                    bar: Style::new().bg(bar.into()),
                    name: Style::new()
                        .bg(bar.into())
                        .fg(ink.into())
                        .add_modifier(Modifier::BOLD),
                    size: Style::new().bg(bar.into()).fg(ink.mix(bar, 0.3).into()),
                    fill: Style::new().bg(fill.into()).fg(fill.mix(hue, 0.45).into()),
                    note: Style::new().bg(fill.into()).fg(text.mix(fill, 0.3).into()),
                }
            }
            Colors::Ansi => {
                let reversed = Modifier::REVERSED | Modifier::BOLD;
                let bar = if selected {
                    Style::new().add_modifier(reversed)
                } else {
                    Style::new().fg(kind_ansi(kind)).add_modifier(reversed)
                };
                Tile {
                    bar,
                    name: bar,
                    size: bar,
                    fill: Style::new().fg(kind_ansi(kind)),
                    note: self.muted,
                }
            }
            Colors::Mono => {
                let mut bar = Style::new().add_modifier(Modifier::REVERSED);
                if selected {
                    bar = bar.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                }
                Tile {
                    bar,
                    name: bar,
                    size: bar,
                    fill: self.faint,
                    note: self.muted,
                }
            }
        }
    }

    fn dark(&self) -> bool {
        match self.colors {
            Colors::True { bg, fg } => bg.luma() < fg.luma(),
            _ => true,
        }
    }
}

/// The styles of one treemap tile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    /// The label row's background.
    pub bar: Style,
    pub name: Style,
    pub size: Style,
    /// The body; its foreground draws the hatch.
    pub fill: Style,
    /// Text on the body, such as a size that did not fit the label row.
    pub note: Style,
}

fn rgb_of(c: Color) -> Rgb {
    match c {
        Color::Rgb(r, g, b) => Rgb(r, g, b),
        _ => Rgb(0x80, 0x80, 0x80),
    }
}

/// Each kind's hue. On a dark terminal the chip's text is the dark background on the hue,
/// on a light one the light background, so each keeps 4.5:1 against both.
fn kind_hue(kind: Kind, dark: bool) -> Rgb {
    let (d, l) = match kind {
        Kind::Docker => (Rgb(0x6f, 0x9b, 0xf0), Rgb(0x23, 0x55, 0xa8)),
        Kind::Xcode => (Rgb(0xa8, 0x90, 0xf5), Rgb(0x5e, 0x42, 0xb4)),
        Kind::Simulators => (Rgb(0xe0, 0x82, 0xc4), Rgb(0x9a, 0x2f, 0x7a)),
        Kind::AgentLogs => (Rgb(0xe5, 0xa4, 0x5a), Rgb(0x84, 0x4a, 0x05)),
        Kind::PackageCache => (Rgb(0x52, 0xc4, 0xb3), Rgb(0x0b, 0x66, 0x5d)),
        Kind::AppCaches => (Rgb(0xa2, 0xab, 0xbb), Rgb(0x4e, 0x56, 0x64)),
        Kind::Worktree => (Rgb(0x80, 0xc8, 0x83), Rgb(0x27, 0x6b, 0x31)),
        Kind::NodeModules => (Rgb(0xee, 0x86, 0x7e), Rgb(0xa3, 0x2e, 0x28)),
    };
    if dark { d } else { l }
}

fn kind_ansi(kind: Kind) -> Color {
    match kind {
        Kind::Docker => Color::Blue,
        Kind::Xcode => Color::Magenta,
        Kind::Simulators => Color::LightMagenta,
        Kind::AgentLogs => Color::Yellow,
        Kind::PackageCache => Color::Cyan,
        Kind::AppCaches => Color::Gray,
        Kind::Worktree => Color::Green,
        Kind::NodeModules => Color::Red,
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
        colors,
        chrome: Chrome::Rules,
        text: s,
        muted: dim,
        faint: dim,
        strong: bold,
        accent: paint(Color::Cyan),
        heading: dim,
        title: paint(Color::Yellow).add_modifier(Modifier::BOLD),
        ok: paint(Color::Green),
        info: paint(Color::Cyan),
        warn: paint(Color::Yellow),
        danger: paint(Color::Red).add_modifier(Modifier::BOLD),
        selection: s.add_modifier(Modifier::REVERSED),
        raised: s,
        border: dim,
        key: bold,
        matched: paint(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        motion: None,
    }
}

/// The selected row's tint against the background: enough to see on its own.
const SELECTION_MIN: f32 = 1.3;
/// Body text, muted text (labels, context) and faint marks (tree guides, separators), each
/// against every surface it sits on: background, overlays and the selected row.
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
    let raised = bg.mix(fg, if dark { 0.13 } else { 0.09 });
    let mut t = if dark { 0.16 } else { 0.10 };
    while bg.mix(accent, t).contrast(bg) < SELECTION_MIN && t < 1.0 {
        t += 0.01;
    }
    let select = bg.mix(accent, t);
    let on = [bg, raised, select];
    let text = ink(bg, fg, 0.94, TEXT_MIN, &on);
    let muted = ink(bg, fg, 0.58, MUTED_MIN, &on);
    let faint = ink(bg, fg, 0.34, FAINT_MIN, &on);
    Theme {
        colors: Colors::True { bg, fg },
        chrome: Chrome::Bands,
        text: s.fg(text.into()),
        muted: s.fg(muted.into()),
        faint: s.fg(faint.into()),
        strong: s.fg(text.into()).add_modifier(Modifier::BOLD),
        accent: s.fg(accent.into()),
        heading: s.fg(muted.into()).add_modifier(Modifier::BOLD),
        title: s.fg(warn.into()).add_modifier(Modifier::BOLD),
        ok: s.fg(ok.into()),
        info: s.fg(info.into()),
        warn: s.fg(warn.into()),
        danger: s.fg(danger.into()).add_modifier(Modifier::BOLD),
        selection: s.bg(select.into()),
        raised: s.fg(text.into()).bg(raised.into()),
        border: s.fg(fg.mix(bg, 0.78).into()),
        key: s.fg(accent.into()).add_modifier(Modifier::BOLD),
        matched: s
            .fg(accent.into())
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
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

    fn env(colorterm: &str, term_program: &str) -> Env {
        let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
        Env {
            colorterm: some(colorterm),
            term_program: some(term_program),
            ..Env::default()
        }
    }

    fn never<T>() -> T {
        panic!("must not be called")
    }

    fn assumed((bg, fg): (Rgb, Rgb)) -> Colors {
        Colors::True { bg, fg }
    }

    #[test]
    fn truecolor_needs_colorterm_and_only_a_known_terminal_is_asked() {
        let ghostty = env("truecolor", "ghostty");
        assert_eq!(
            Colors::detect(&ghostty, || Some(DARK), never),
            Colors::True {
                bg: DARK.0,
                fg: DARK.1
            }
        );
        // No COLORTERM: the ANSI palette, and nothing is asked or read.
        assert_eq!(
            Colors::detect(&env("", "ghostty"), never, never),
            Colors::Ansi
        );
        assert_eq!(
            Colors::detect(&env("yes", "iTerm.app"), never, never),
            Colors::Ansi
        );
        let mono = Env {
            no_color: true,
            ..ghostty.clone()
        };
        assert_eq!(Colors::detect(&mono, never, never), Colors::Mono);
        // An unknown terminal is not asked; the macOS appearance picks light or dark.
        for program in ["", "tmux", "vscode"] {
            let e = env("24bit", program);
            assert_eq!(
                Colors::detect(&e, never, || Some(false)),
                assumed(ASSUMED_LIGHT)
            );
            assert_eq!(Colors::detect(&e, never, || None), assumed(ASSUMED_DARK));
        }
        // A known terminal that gives no colours is treated the same.
        assert_eq!(
            Colors::detect(&ghostty, || None, || Some(false)),
            assumed(ASSUMED_LIGHT)
        );
    }

    #[test]
    fn colorfgbg_decides_before_the_appearance() {
        let with = |v: &str| Env {
            colorfgbg: Some(v.into()),
            ..env("truecolor", "")
        };
        assert_eq!(
            Colors::detect(&with("15;0"), never, never),
            assumed(ASSUMED_DARK)
        );
        assert_eq!(
            Colors::detect(&with("0;default;15"), never, never),
            assumed(ASSUMED_LIGHT)
        );
        assert_eq!(
            Colors::detect(&with("default;default"), never, || Some(false)),
            assumed(ASSUMED_LIGHT)
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
            let surfaces = [bg, rgb(t.raised.bg), select];
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
    fn kind_chips_and_tile_labels_stay_readable_in_every_theme() {
        let kinds = [
            Kind::Docker,
            Kind::Xcode,
            Kind::Simulators,
            Kind::AgentLogs,
            Kind::PackageCache,
            Kind::AppCaches,
            Kind::Worktree,
            Kind::NodeModules,
        ];
        for (name, bg, fg) in BACKGROUNDS {
            let t = Theme::new(Look::Tinted, Colors::True { bg, fg });
            for k in kinds {
                let chip = t.kind_chip(k);
                let c = rgb(chip.fg).contrast(rgb(chip.bg));
                assert!(c >= 4.5, "{name}: {k:?} chip is {c:.2}:1");
                for selected in [false, true] {
                    let tile = t.tile(k, selected);
                    let c = rgb(tile.name.fg).contrast(rgb(tile.name.bg));
                    assert!(c >= 4.5, "{name}: {k:?} label ({selected}) is {c:.2}:1");
                }
            }
            for tone in [Tone::Ok, Tone::Info, Tone::Warn, Tone::Danger] {
                let chip = t.chip(tone);
                let c = rgb(chip.fg).contrast(rgb(chip.bg));
                assert!(c >= 4.5, "{name}: {tone:?} chip is {c:.2}:1");
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
    fn the_query_needs_a_local_known_terminal_unless_forced() {
        for (program, term) in [("ghostty", ""), ("iTerm.app", ""), ("", "xterm-kitty")] {
            let e = Env {
                term: Some(term.into()).filter(|t: &String| !t.is_empty()),
                ..env("truecolor", program)
            };
            assert!(e.may_ask(), "{program} {term}");
            let ssh = Env { ssh: true, ..e };
            assert!(!ssh.may_ask(), "{program} {term} over ssh");
            let forced = Env {
                ask: Some("1".into()),
                ..ssh
            };
            assert!(forced.may_ask());
        }
        for program in ["", "tmux", "vscode", "WarpTerminal"] {
            assert!(!env("truecolor", program).may_ask(), "{program}");
        }
        let off = Env {
            ask: Some("0".into()),
            ..env("truecolor", "ghostty")
        };
        assert!(!off.may_ask());
    }
}
