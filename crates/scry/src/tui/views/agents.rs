//! Agents: sessions as a tree with the processes they own, leftovers flagged.

use scry_core::view::agents::{reap_targets, stop_target};
use scry_core::view::{Board, Load, State, Tab, Target, human_age, human_bytes};

use super::{Cell, Column, Row, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Agents,
    columns: &[
        Column::fill("SESSION / PROCESS", 20),
        Column::fixed("PID", 6).from(96),
        Column::fixed("RAM", 5),
        Column::fixed("PORT", 6),
        Column::fixed("AGE", 4),
        Column::fixed("EVIDENCE", 8).from(70),
        Column::fixed("STATE", 10),
    ],
    rows,
    empty: (
        "No agent sessions are running.",
        "Scry recognises Claude Code, Codex, OpenCode and Gemini CLI.\n\
         Their sessions appear here as they run, with what each one started.\n\
         `scry hook install claude` makes Claude Code attribution exact.",
    ),
    badge,
    hints: &[("⏎", "fold"), ("x", "stop"), ("X", "reap"), ("K", "keep")],
    reap: Some(reap),
};

fn rows(board: &Board) -> Load<Vec<Row>> {
    board.agents.as_ref().map(|agents| {
        agents
            .iter()
            .enumerate()
            .map(|(i, a)| Row {
                id: a.id.clone(),
                depth: a.depth,
                cells: vec![
                    Cell::text(&a.label).with_context(a.context.as_deref()),
                    a.pid
                        .map_or_else(Cell::empty, |p| Cell::num(p.to_string(), p as u64)),
                    a.ram
                        .map_or_else(Cell::empty, |r| Cell::num(human_bytes(r), r)),
                    ports(&a.ports),
                    Cell::num(human_age(a.age_s), a.age_s),
                    Cell::muted(a.evidence.map_or("", |e| e.word())),
                    Cell::state(a.state),
                ],
                detail: Some(a.detail.clone()),
                target: stop_target(agents, i),
                jump: None,
            })
            .collect()
    })
}

fn ports(ports: &[u16]) -> Cell {
    match ports {
        [] => Cell::empty(),
        [p] => Cell::num(format!(":{p}"), (*p).into()),
        [p, rest @ ..] => Cell::num(format!(":{p}+{}", rest.len()), (*p).into()),
    }
}

fn badge(board: &Board) -> usize {
    board.agents.ready().map_or(0, |rows| {
        rows.iter()
            .filter(|r| r.state == Some(State::Leftover))
            .count()
    })
}

pub(super) fn reap(board: &Board) -> Vec<Target> {
    board
        .agents
        .ready()
        .map_or_else(Vec::new, |r| reap_targets(r))
}
