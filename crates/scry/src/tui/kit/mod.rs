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
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn fill(f: &mut Frame, area: Rect, style: Style) {
    f.render_widget(Block::new().style(style), area);
}

/// Display columns, as the terminal draws them: wide CJK and emoji take two.
pub fn width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Lines `text` takes when word-wrapped to `width` columns.
pub fn wrapped_lines(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let mut lines = 1;
    let mut used = 0;
    for word in text.split_whitespace() {
        let w = word.width();
        if used > 0 && used + 1 + w > width {
            lines += 1;
            used = w;
        } else {
            used += w + usize::from(used > 0);
        }
    }
    lines
}

/// Pads `text` with spaces to `cols` display columns.
pub fn pad(text: &str, cols: usize) -> String {
    format!("{text}{}", " ".repeat(cols.saturating_sub(text.width())))
}

/// Cuts spans to `max` columns, ending in `…` when anything was dropped. Cuts fall between
/// graphemes, so an accent or an emoji sequence is never split.
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
        for g in span.content.graphemes(true) {
            let gw = g.width();
            if used + gw + 1 > max {
                break;
            }
            used += gw;
            cut.push_str(g);
        }
        let style = span.style;
        out.push(Span::styled(cut, style));
        // The ellipsis follows the last character kept, not a gap before a cut-off context.
        while let Some(last) = out.last_mut() {
            let kept = last.content.trim_end().len();
            if kept > 0 {
                let trimmed = last.content[..kept].to_string();
                last.content = trimmed.into();
                break;
            }
            out.pop();
        }
        out.push(Span::styled("…", style));
        break;
    }
    Line::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(l: Line) -> String {
        l.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn fit_cuts_with_an_ellipsis_only_when_needed() {
        assert_eq!(line(fit(vec![Span::raw("next dev")], 8)), "next dev");
        assert_eq!(
            line(fit(vec![Span::raw("next"), Span::raw(" dev")], 6)),
            "next…"
        );
        assert_eq!(line(fit(vec![Span::raw("python3 -m")], 5)), "pyth…");
        // A context that does not fit at all leaves no gap before the ellipsis.
        let spans = vec![
            Span::raw("mDNSResponder"),
            Span::raw("  "),
            Span::raw("_mdns"),
        ];
        assert_eq!(line(fit(spans, 16)), "mDNSResponder…");
    }

    #[test]
    fn fit_counts_display_columns_and_keeps_graphemes_whole() {
        // The ZWJ emoji is one grapheme two columns wide.
        assert_eq!(line(fit(vec![Span::raw("👩‍💻XY")], 3)), "👩‍💻…");
        assert_eq!(line(fit(vec![Span::raw("👩‍💻XY")], 2)), "…");
        // CJK takes two columns a character.
        let cut = fit(vec![Span::raw("東京都庁")], 5);
        assert_eq!(line(cut.clone()), "東京…");
        assert_eq!(cut.width(), 5);
        // A combining accent stays on its letter.
        assert_eq!(
            line(fit(vec![Span::raw("cafe\u{301}st")], 5)),
            "cafe\u{301}…"
        );
    }

    #[test]
    fn wrapped_lines_breaks_between_words() {
        assert_eq!(wrapped_lines("one two three", 13), 1);
        assert_eq!(wrapped_lines("one two three", 12), 2);
        assert_eq!(wrapped_lines("東京 東京", 4), 2);
        assert_eq!(wrapped_lines("", 10), 1);
    }

    #[test]
    fn pad_counts_display_columns() {
        assert_eq!(pad("pid", 6), "pid   ");
        assert_eq!(pad("東京", 6), "東京  ");
        assert_eq!(pad("cafe\u{301}", 6), "cafe\u{301}  ");
    }
}
