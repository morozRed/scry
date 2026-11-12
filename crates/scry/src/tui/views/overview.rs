//! Overview: one ranked list of what needs attention; enter opens the row it is about.

use scry_core::view::{Board, Load, Tab, human_bytes};

use super::{Cell, Column, Row, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Overview,
    columns: &[
        Column::fixed("STATE", 10).unsorted(),
        Column::fill("NEEDS ATTENTION", 24).unsorted(),
        Column::fixed("SIZE", 5),
        Column::fixed("VIEW", 11).from(70).unsorted(),
    ],
    rows,
    empty: (
        "Nothing needs attention.",
        "Checked your processes for leftovers of ended agent sessions,\n\
         and every listening port for one held by a leftover.\n\
         A filling disk and stale worktrees will show up here too.",
    ),
    badge,
    hints: &[("⏎", "go to")],
    reap: None,
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
            target: None,
            jump: Some((a.tab, a.row.clone())),
        })
        .collect();
    Load::Ready(rows)
}

fn badge(board: &Board) -> usize {
    board.overview.attention.len()
}
