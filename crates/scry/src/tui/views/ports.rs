//! Ports: listening sockets with the process, project and session behind each.

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
            .map(|p| {
                let age = p.age_secs.unwrap_or(0).max(0) as u64;
                Row {
                    id: format!("{}-{}-{}", p.proto.name(), p.port, p.pid),
                    depth: 0,
                    cells: vec![
                        Cell::num(format!(":{}", p.port), p.port.into()),
                        Cell::muted(p.addresses.join(", ")),
                        Cell::text(&p.process)
                            .with_context((p.owner != "you").then_some(p.owner.as_str())),
                        Cell::muted(p.project.as_deref().unwrap_or("")),
                        Cell::text(p.session.as_ref().map(|s| s.short()).unwrap_or_default()),
                        Cell::num(human_age(age), age),
                        Cell::state(p.state),
                    ],
                    detail: Some(p.detail()),
                    target: p.stop_target(),
                    jump: None,
                }
            })
            .collect()
    })
}

fn badge(board: &Board) -> usize {
    board.ports.ready().map_or(0, |ports| {
        ports
            .iter()
            .filter(|p| p.state == Some(State::Leftover))
            .count()
    })
}
