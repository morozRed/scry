use scry_core::view::{Board, Load, State, Tab, human_age};

use super::{Cell, Column, Row, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Ports,
    columns: &[
        Column::fixed("PORT", 6),
        Column::fixed("ADDR", 9).from(96),
        Column::fill("PROCESS", 12),
        Column::fill("PROJECT", 10),
        Column::fill("SESSION", 12).from(76),
        Column::fixed("AGE", 4),
        Column::fixed("STATE", 10),
    ],
    rows,
    empty: (
        "No listening ports.",
        "Nothing on this Mac is accepting connections.",
    ),
    badge,
    hints: &[("x", "stop")],
    reap: None,
};

fn rows(board: &Board) -> Load<Vec<Row>> {
    board.ports.as_ref().map(|ports| {
        ports
            .iter()
            .map(|p| Row {
                id: p.id.clone(),
                depth: 0,
                cells: vec![
                    Cell::num(format!(":{}", p.port), p.port.into()),
                    Cell::muted(&p.addr),
                    Cell::text(&p.command).with_context(p.owner.as_deref()),
                    Cell::muted(p.project.as_deref().unwrap_or("")),
                    Cell::text(p.session.as_deref().unwrap_or("")),
                    Cell::num(human_age(p.age_s), p.age_s),
                    Cell::state(Some(p.state)),
                ],
                detail: Some(p.detail.clone()),
                target: p.stop_target(),
                jump: None,
            })
            .collect()
    })
}

fn badge(board: &Board) -> usize {
    board.ports.ready().map_or(0, |ports| {
        ports.iter().filter(|p| p.state == State::Leftover).count()
    })
}
