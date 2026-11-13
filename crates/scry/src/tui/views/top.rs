//! Top: what uses the CPU and memory, your processes grouped by app, heaviest first, and
//! one line for the rest of the machine.

use scry_core::view::usage::{REST_ID, percent};
use scry_core::view::{Board, Load, State, Tab, Tone, UsageRow, human_age, human_bytes};

use super::{Cell, CellKind, Chip, Column, Row, SortKey, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Top,
    title: "WHAT USES YOUR MAC",
    note,
    about: "Why",
    bars: None,
    columns: &[
        Column::fill("process", 20),
        Column::fixed("load", METER as u16).from(60).unsorted(),
        Column::fixed("cpu", 5),
        Column::fixed("ram", 5),
        Column::fixed("procs", 4).from(90),
        Column::fixed("age", 4).from(110),
        Column::fixed("state", 8),
    ],
    rows,
    empty: (
        "Nothing to measure.",
        "Scry reads the CPU time and memory of your own processes. A first reading \
         ranks them by memory; CPU shares follow with the next one.",
    ),
    badge,
    hints: &[("x", "stop"), ("space", "open")],
    reap: None,
    fold_all: true,
};

/// The meter's width in cells.
const METER: usize = 8;
const EIGHTHS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

fn rows(board: &Board) -> Load<Vec<Row>> {
    board.usage.as_ref().map(|u| {
        let measured = u.cpu.is_some();
        // Meters compare rows with each other: the heaviest line fills its meter.
        let top = u
            .rows
            .iter()
            .filter(|r| r.depth == 0)
            .map(|r| load(r, measured))
            .fold(0.0_f64, f64::max);
        u.rows.iter().map(|r| row(r, measured, top)).collect()
    })
}

/// What a meter shows: the CPU share once it is measured, else memory.
fn load(r: &UsageRow, measured: bool) -> f64 {
    if measured {
        f64::from(r.cpu.unwrap_or(0.0))
    } else {
        r.ram.unwrap_or(0) as f64
    }
}

fn row(r: &UsageRow, measured: bool, top: f64) -> Row {
    let mut chips = Vec::new();
    if let Some(pid) = r.pid {
        chips.push(Chip::Tone(format!("pid {pid}"), Tone::Quiet));
    }
    if r.count > 1 {
        chips.push(Chip::Tone(format!("{} processes", r.count), Tone::Quiet));
    }
    let share = if top > 0.0 {
        load(r, measured) / top
    } else {
        0.0
    };
    Row {
        id: r.id.clone(),
        depth: r.depth,
        cells: vec![
            Cell::text(&r.label).with_context(r.context.as_deref()),
            meter(share),
            r.cpu.map_or_else(Cell::empty, |c| {
                Cell::num(percent(c), (f64::from(c) * 1000.0) as u64)
            }),
            r.ram
                .map_or_else(Cell::empty, |b| Cell::num(human_bytes(b), b)),
            if r.count > 1 {
                Cell::num(format!("×{}", r.count), r.count as u64)
            } else {
                Cell::empty()
            },
            r.age_s
                .filter(|_| r.depth > 0 || r.count == 1)
                .map_or_else(Cell::empty, |a| Cell::num(human_age(a), a)),
            Cell::state(r.state),
        ],
        detail: Some(r.detail.clone()),
        chips,
        target: r.stop_target(),
        jump: None,
    }
}

/// A bar of `share` (0 to 1) of the meter, in eighths of a cell; a sliver for anything
/// above zero, so a busy row never looks idle.
fn meter(share: f64) -> Cell {
    let eighths = (share.clamp(0.0, 1.0) * (METER * 8) as f64).round() as usize;
    let eighths = if share > 0.0 { eighths.max(1) } else { 0 };
    let mut text = "█".repeat(eighths / 8);
    if eighths % 8 > 0 {
        text.push(EIGHTHS[eighths % 8 - 1]);
    }
    Cell {
        text,
        context: None,
        kind: CellKind::Meter,
        key: SortKey::None,
    }
}

/// The totals are in the bars above the list, so the note only counts leftovers.
fn note(board: &Board) -> Option<String> {
    match badge(board) {
        0 => None,
        1 => Some("1 group holds a leftover".into()),
        n => Some(format!("{n} groups hold leftovers")),
    }
}

/// Groups that hold a leftover: load you can get back.
fn badge(board: &Board) -> usize {
    board.usage.ready().map_or(0, |u| {
        u.rows
            .iter()
            .filter(|r| r.depth == 0 && r.id != REST_ID && r.state == Some(State::Leftover))
            .count()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_meter_fills_in_eighths_and_never_hides_a_busy_row() {
        assert_eq!(meter(1.0).text, "████████");
        assert_eq!(meter(0.5).text, "████");
        assert_eq!(meter(0.0).text, "");
        assert_eq!(meter(0.001).text, "▏");
        assert_eq!(meter(0.3).text, "██▍");
    }
}
