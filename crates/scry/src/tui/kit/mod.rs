//! The component kit. Every view is drawn from these, with tokens from the theme.

pub mod chrome;
pub mod detail;
pub mod overlay;
pub mod states;
pub mod table;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;
use unicode_width::UnicodeWidthStr;

pub fn fill(f: &mut Frame, area: Rect, style: Style) {
    f.render_widget(Block::new().style(style), area);
}

pub fn width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Cuts spans to `max` columns, ending in `…` when anything was dropped.
pub fn fit<'a>(spans: Vec<Span<'a>>, max: usize) -> Line<'a> {
    if width(&spans) <= max {
        return Line::from(spans);
    }
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = span.content.width();
        if used + w < max {
            used += w;
            out.push(span);
            continue;
        }
        let mut cut = String::new();
        for ch in span.content.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw + 1 > max {
                break;
            }
            used += cw;
            cut.push(ch);
        }
        let style = span.style;
        out.push(Span::styled(cut.trim_end().to_string(), style));
        out.push(Span::styled("…", style));
        break;
    }
    Line::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_cuts_with_an_ellipsis_only_when_needed() {
        let line = |l: Line| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(line(fit(vec![Span::raw("next dev")], 8)), "next dev");
        assert_eq!(
            line(fit(vec![Span::raw("next"), Span::raw(" dev")], 6)),
            "next…"
        );
        assert_eq!(line(fit(vec![Span::raw("python3 -m")], 5)), "pyth…");
    }
}
