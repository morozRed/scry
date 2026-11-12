//! Overview: one ranked list of what needs attention; enter opens the row it is about.

use scry_core::view::{Board, Load, Tab, human_bytes};

use super::{Cell, Column, Row, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Overview,
    title: "NEEDS ATTENTION",
    note,
    about: "Why",
    bars: None,
    columns: &[
        // The gutter shows the state; the word would repeat it.
        Column::fixed("state", 10).hidden(),
        Column::fill("item", 24).unsorted(),
        Column::fixed("size", 5),
        Column::fixed("view", 11).from(60).unsorted(),
    ],
    rows,
    empty: (
        "Nothing needs attention.",
        "Checked your processes for leftovers of ended agent sessions,\n\
         and every listening port for one held by a leftover.\n\
         A filling disk and stale worktrees will show up here too.",
    ),
    badge,
    hints: &[("⏎", "go to"), ("X", "reap")],
    reap: Some(super::agents::reap),
};

fn rows(board: &Board) -> Load<Vec<Row>> {
    // An empty list is an all-clear only once every source it draws from was read.
    let missing = match (board.ports.ready(), board.agents.ready()) {
        (Some(_), Some(_)) => None,
        (None, Some(_)) => Some("Ports are"),
        (Some(_), None) => Some("Agent sessions are"),
        (None, None) => Some("Ports and agent sessions are"),
    };
    if let Some(what) = missing
        && board.overview.attention.is_empty()
    {
        return Load::Unavailable(format!("{what} not collected yet, so nothing is flagged."));
    }
    let rows = board
        .overview
        .attention
        .iter()
        .map(|a| Row {
            id: a.id.clone(),
            depth: 0,
            cells: vec![
                Cell::state(Some(a.state)),
                Cell::text(&a.title).with_context(a.context.as_deref()),
                a.size
                    .map_or_else(Cell::empty, |s| Cell::num(human_bytes(s), s)),
                Cell::muted(format!("→ {}", a.tab.title())),
            ],
            detail: Some(a.detail.clone()),
            jump: Some((a.tab, a.row.clone())),
            ..Row::default()
        })
        .collect();
    Load::Ready(rows)
}

fn note(board: &Board) -> Option<String> {
    match badge(board) {
        0 => None,
        n => Some(n.to_string()),
    }
}

fn badge(board: &Board) -> usize {
    board.overview.attention.len()
}
