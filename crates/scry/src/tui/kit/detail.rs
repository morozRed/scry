//! The detail pane: the selected row in one sentence, then the evidence. Below the list it
//! packs facts onto the title line; beside it each fact gets a line.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Wrap};
use scry_core::view::Detail;

use super::{fill, width};
use crate::tui::theme::{Chrome, Theme};

pub fn detail(f: &mut Frame, area: Rect, theme: &Theme, d: &Detail, beside: bool) {
    let block = match theme.chrome {
        Chrome::Bands => {
            fill(f, area, theme.surface);
            Block::new().padding(Padding::new(2, 2, u16::from(beside), 0))
        }
        Chrome::Rules => Block::new().padding(Padding::horizontal(1)),
    };
    let lines = if beside {
        tall(theme, d)
    } else {
        wide(theme, d, block.inner(area).width as usize)
    };
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Facts share the title line; the ones that do not fit are left for the wider layout.
fn wide<'a>(theme: &Theme, d: &'a Detail, room: usize) -> Vec<Line<'a>> {
    let mut head = vec![Span::styled(d.title.as_str(), theme.strong)];
    for (i, fact) in d.facts.iter().enumerate() {
        let sep = if i == 0 { "   " } else { "  ·  " };
        let more = [
            Span::styled(sep, theme.faint),
            Span::styled(format!("{} ", fact.label), theme.muted),
            Span::styled(fact.value.as_str(), theme.text),
        ];
        if width(&head) + width(&more) > room {
            break;
        }
        head.extend(more);
    }
    let mut lines = vec![
        Line::from(head),
        Line::styled(d.sentence.as_str(), theme.text),
    ];
    for e in &d.evidence {
        lines.push(Line::from(vec![
            Span::styled("evidence  ", theme.muted),
            Span::styled(e.as_str(), theme.text.add_modifier(Modifier::ITALIC)),
        ]));
    }
    lines
}

fn tall<'a>(theme: &Theme, d: &'a Detail) -> Vec<Line<'a>> {
    let mut lines = vec![Line::styled(d.title.as_str(), theme.strong), Line::raw("")];
    let pad = d.facts.iter().map(|f| f.label.len()).max().unwrap_or(0) + 2;
    for fact in &d.facts {
        lines.push(Line::from(vec![
            Span::styled(format!("{:pad$}", fact.label), theme.muted),
            Span::styled(fact.value.as_str(), theme.text),
        ]));
    }
    if !d.facts.is_empty() {
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(d.sentence.as_str(), theme.text));
    if !d.evidence.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("Evidence", theme.heading));
        for e in &d.evidence {
            lines.push(Line::from(vec![
                Span::styled("· ", theme.faint),
                Span::styled(e.as_str(), theme.text.add_modifier(Modifier::ITALIC)),
            ]));
        }
    }
    lines
}
