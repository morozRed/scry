//! Agents: sessions as a tree with the processes they own, leftovers flagged.

use scry_core::view::agents::{reap_targets, stop_target};
use scry_core::view::{Board, Load, State, Tab, Target, Tone, human_age, human_bytes};

use super::{Cell, Chip, Column, Row, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Agents,
    title: "AGENT SESSIONS",
    note,
    about: "Why",
    bars: None,
    // The pid and the evidence are in the detail pane; the list keeps what tells rows apart.
    columns: &[
        Column::fill("process", 20),
        Column::fixed("port", 6),
        Column::fixed("ram", 5),
        Column::fixed("age", 4),
        Column::fixed("state", 8),
    ],
    rows,
    empty: (
        "No agent sessions are running.",
        "Scry recognises Claude Code, Codex, OpenCode and Gemini CLI.\n\
         Their sessions appear here as they run, with what each one started.\n\
         `scry hook install claude` makes Claude Code attribution exact.",
    ),
    badge,
    hints: &[
        ("x", "stop"),
        ("X", "reap"),
        ("K", "keep"),
        ("space", "fold"),
    ],
    reap: Some(reap),
    fold_all: false,
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
                    ports(&a.ports),
                    a.ram
                        .map_or_else(Cell::empty, |r| Cell::num(human_bytes(r), r)),
                    Cell::num(human_age(a.age_s), a.age_s),
                    Cell::state(a.state),
                ],
                detail: Some(a.detail.clone()),
                chips: a
                    .evidence
                    .map(|e| Chip::Tone(format!("by {}", e.word()), Tone::Quiet))
                    .into_iter()
                    .chain(a.pid.map(|p| Chip::Tone(format!("pid {p}"), Tone::Quiet)))
                    .collect(),
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

fn note(board: &Board) -> Option<String> {
    match badge(board) {
        0 => None,
        1 => Some("1 leftover".into()),
        n => Some(format!("{n} leftovers")),
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
