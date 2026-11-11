//! Two quiet effects: the sheet fades in, and rows that changed on a rescan glow briefly.
//! Frames keep coming only while one runs. They need truecolor, because a fade between a
//! theme's named colours has no right answer.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tachyonfx::{Effect, EffectManager, Interpolation, fx};

use super::theme::{MotionColors, Rgb};
use super::ui::Painted;

const SHEET_IN: Duration = Duration::from_millis(180);
const ROW_GLOW: Duration = Duration::from_millis(900);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    #[default]
    Sheet,
    Rows,
}

pub struct Motion {
    colors: Option<MotionColors>,
    fx: EffectManager<Key>,
    last: Option<Instant>,
    sheet: bool,
    rows: Vec<String>,
}

impl Motion {
    pub fn new(colors: Option<MotionColors>) -> Self {
        Motion {
            colors,
            fx: EffectManager::default(),
            last: None,
            sheet: false,
            rows: Vec::new(),
        }
    }

    pub fn sheet_opened(&mut self) {
        self.sheet = self.colors.is_some();
    }

    pub fn rows_changed(&mut self, ids: Vec<String>) {
        if self.colors.is_some() {
            self.rows = ids;
        }
    }

    pub fn running(&self) -> bool {
        self.sheet || !self.rows.is_empty() || self.fx.is_running()
    }

    /// Runs after a frame is drawn, on that frame's buffer.
    pub fn apply(&mut self, buf: &mut Buffer, painted: &Painted) {
        let Some(c) = self.colors else {
            return;
        };
        if std::mem::take(&mut self.sheet)
            && let Some(area) = painted.sheet
        {
            let from = (Some(c.bg), Some(c.bg));
            self.fx
                .add_unique_effect(Key::Sheet, fade(area, from, c, SHEET_IN));
        }
        let ids = std::mem::take(&mut self.rows);
        let glows: Vec<Effect> = painted
            .rows
            .iter()
            .filter(|(id, _)| ids.contains(id))
            .map(|(_, area)| fade(*area, (None, Some(c.flash)), c, ROW_GLOW))
            .collect();
        if !glows.is_empty() {
            self.fx.add_unique_effect(Key::Rows, fx::parallel(&glows));
        }

        let now = Instant::now();
        let dt = self.last.map_or(Duration::ZERO, |t| now - t);
        self.fx.process_effects(dt, buf, buf.area);
        self.last = self.fx.is_running().then_some(now);
    }
}

/// Fades cells in `area` from `from` to whatever was drawn. Cells left at the terminal's
/// default colour fade towards the colours the terminal reported for it.
fn fade(area: Rect, from: (Option<Rgb>, Option<Rgb>), c: MotionColors, d: Duration) -> Effect {
    fx::effect_fn((), (d, Interpolation::QuadOut), move |_, ctx, cells| {
        let t = ctx.alpha();
        if t >= 1.0 {
            return;
        }
        for (_, cell) in cells {
            if let (Some(f), Some(to)) = (from.0, rgb(cell.fg, c.fg)) {
                cell.fg = f.mix(to, t).into();
            }
            if let (Some(f), Some(to)) = (from.1, rgb(cell.bg, c.bg)) {
                cell.bg = f.mix(to, t).into();
            }
        }
    })
    .with_area(area)
}

fn rgb(color: Color, reset: Rgb) -> Option<Rgb> {
    match color {
        Color::Reset => Some(reset),
        Color::Rgb(r, g, b) => Some(Rgb(r, g, b)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: MotionColors = MotionColors {
        bg: Rgb(10, 10, 10),
        fg: Rgb(200, 200, 200),
        flash: Rgb(90, 70, 20),
    };

    fn painted(area: Rect) -> Painted {
        Painted {
            sheet: Some(area),
            rows: vec![("row".into(), area)],
        }
    }

    #[test]
    fn a_sheet_starts_invisible_and_the_frames_stop_after() {
        let area = Rect::new(0, 0, 4, 1);
        let mut m = Motion::new(Some(C));
        m.sheet_opened();
        assert!(m.running());
        let mut buf = Buffer::empty(area);
        m.apply(&mut buf, &painted(area));
        assert_eq!(buf[(0, 0)].fg, Color::from(C.bg));
        assert!(m.running());
        std::thread::sleep(SHEET_IN + Duration::from_millis(20));
        let mut buf = Buffer::empty(area);
        m.apply(&mut buf, &painted(area));
        assert!(!m.running(), "frames must stop once the fade is done");
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
    }

    #[test]
    fn a_changed_row_glows_from_the_flash_colour() {
        let area = Rect::new(0, 0, 4, 1);
        let mut m = Motion::new(Some(C));
        m.rows_changed(vec!["row".into()]);
        let mut buf = Buffer::empty(area);
        m.apply(
            &mut buf,
            &Painted {
                sheet: None,
                ..painted(area)
            },
        );
        assert_eq!(buf[(0, 0)].bg, Color::from(C.flash));
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
    }

    #[test]
    fn without_truecolor_nothing_moves() {
        let mut m = Motion::new(None);
        m.sheet_opened();
        m.rows_changed(vec!["row".into()]);
        assert!(!m.running());
    }
}
