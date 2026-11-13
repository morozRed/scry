//! Ports: listening sockets with the process, project and session behind each.

use scry_core::view::{Board, Load, State, Tab, human_age};

use super::{Cell, Column, Row, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Ports,
    title: "LISTENING PORTS",
    note,
    about: "Why",
    bars: None,
    columns: &[
        Column::fixed("port", 6),
        Column::fixed("address", 9).from(96),
        Column::fill("process", 12),
        Column::fill("project", 10),
        Column::fill("session", 12).from(76),
        Column::fixed("age", 4),
        Column::fixed("state", 8),
    ],
    rows,
    empty: (
        "No listening ports.",
        "No process on this Mac is accepting connections. A server you start, such as a dev \
         server, shows up here with its project and the agent session that started it.",
    ),
    badge,
    hints: &[("x", "stop")],
    reap: None,
    fold_all: false,
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
                        Cell::text(p.holder())
                            .with_context((p.owner != "you").then_some(p.owner.as_str())),
                        Cell::muted(p.project.as_deref().unwrap_or("")),
                        Cell::text(p.session.as_ref().map(|s| s.short()).unwrap_or_default()),
                        Cell::num(human_age(age), age),
                        Cell::state(p.state),
                    ],
                    detail: Some(p.detail()),
                    target: p.stop_target(),
                    ..Row::default()
                }
            })
            .collect()
    })
}

fn note(board: &Board) -> Option<String> {
    match badge(board) {
        0 => None,
        1 => Some("1 leftover".into()),
        n => Some(format!("{n} leftovers")),
    }
}

fn badge(board: &Board) -> usize {
    board.ports.ready().map_or(0, |ports| {
        ports
            .iter()
            .filter(|p| p.state == Some(State::Leftover))
            .count()
    })
}
