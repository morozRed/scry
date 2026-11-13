//! Disk: the volume as a bar with its forecast, then the catalog's items as a treemap or a
//! list ranked by what deleting them frees, each with the cleanup it allows. `t` cleans up
//! the selected item.

use scry_core::disk::clean::Allows;
use scry_core::disk::forecast::Forecast;
use scry_core::view::disk::{DiskItem, DiskList};
use scry_core::view::worktrees::bytes;
use scry_core::view::{Board, Load, Tab, Tone, disk_bytes};

use super::{Bars, Cell, Chip, Column, Ink, Row, SortKey, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Disk,
    title: "WHAT FILLS THE DISK",
    note,
    about: "What",
    bars: Some(worth),
    columns: &[
        Column::fill("item", 18),
        Column::fixed("kind", 13).from(84),
        Column::fixed("size", 5),
        Column::fixed("frees", 6),
        Column::fixed("cleanup", 7),
        Column::fixed("made by", 15).from(110),
    ],
    rows,
    empty: (
        "Nothing in the catalog takes space here.",
        "Scry looks at Xcode, simulators, Docker, package caches, agent logs, app caches, \
         node_modules under the code roots, and worktrees.",
    ),
    badge,
    hints: &[("t", "clean up")],
    reap: None,
    fold_all: false,
};

fn rows(board: &Board) -> Load<Vec<Row>> {
    board
        .disk
        .as_ref()
        .map(|list| list.items.iter().map(|d| row(d, list.sizing)).collect())
}

fn row(d: &DiskItem, sizing: bool) -> Row {
    // A worktree whose folder is gone has nothing to wait for.
    let size = |b: Option<u64>, prefix: &str, gone: &str| match b {
        Some(b) if d.stale => Cell::num(format!("{prefix}{}…", disk_bytes(b)), b),
        Some(b) => Cell::num(format!("{prefix}{}", disk_bytes(b)), b),
        None if d.gone => Cell {
            key: SortKey::None,
            ..Cell::num(gone, 0)
        },
        None if sizing => Cell::num("…", 0),
        None => Cell {
            key: SortKey::None,
            ..Cell::num("-", 0)
        },
    };
    let context = match (&d.state, d.kind.word()) {
        (Some(state), _) => Some(state.word().to_string()),
        _ => None,
    };
    let mut chips = vec![
        Chip::Kind(d.kind.word().into(), d.kind),
        Chip::Tone(
            d.cleanup().to_string(),
            if cleanable(d) { Tone::Ok } else { Tone::Quiet },
        ),
    ];
    if let Some(f) = d.frees {
        let at_least = if d.at_least { "≥ " } else { "" };
        chips.push(Chip::Tone(
            format!("frees {at_least}{}", disk_bytes(f)),
            Tone::Quiet,
        ));
    }
    Row {
        id: d.id.clone(),
        depth: 0,
        chips,
        cells: vec![
            Cell::text(&d.name).with_context(context),
            Cell::muted(d.kind.word()),
            size(d.size, "", "gone"),
            size(d.frees, if d.at_least { "≥" } else { "" }, "-"),
            cleanup(d),
            Cell::muted(&d.creator),
        ],
        detail: Some(d.detail.clone()),
        ..Row::default()
    }
}

/// Whether Scry, or the tool's own command, can give the space back.
pub fn cleanable(d: &DiskItem) -> bool {
    !matches!(d.allows, Allows::Never { .. } | Allows::Off { .. })
}

/// The items cleanup can take, largest first, for the pane's ranking.
fn worth(board: &Board) -> Option<Bars> {
    let list = board.disk.ready()?;
    let mut rows: Vec<&DiskItem> = list
        .items
        .iter()
        .filter(|d| cleanable(d) && d.frees.is_some_and(|f| f > 0))
        .collect();
    if rows.is_empty() {
        return None;
    }
    rows.sort_by_key(|d| std::cmp::Reverse(d.frees));
    Some(Bars {
        title: "Worth cleaning".into(),
        note: if list.destructive {
            format!("≥ {}", disk_bytes(list.reclaimable))
        } else {
            String::new()
        },
        rows: rows
            .iter()
            .map(|d| (d.name.clone(), d.frees.unwrap_or(0), d.kind))
            .collect(),
    })
}

fn note(board: &Board) -> Option<String> {
    let list = board.disk.ready()?;
    list.destructive
        .then(|| format!("≥ {} to clean up", disk_bytes(list.reclaimable)))
}

/// The cleanup word, toned by what it would do: a delete is final, the Trash is not.
fn cleanup(d: &DiskItem) -> Cell {
    let word = d.cleanup();
    let order = match d.allows {
        Allows::Delete { .. } => 0,
        Allows::Trash => 1,
        Allows::Prune => 2,
        Allows::Command { .. } => 3,
        Allows::Off { .. } => 4,
        Allows::Never { .. } => 5,
    };
    Cell {
        key: SortKey::Num(order),
        ..Cell::muted(word)
    }
}

/// A mark on the tab only when the disk fills within a month; cleanable items alone are not
/// news.
fn badge(board: &Board) -> usize {
    let filling = board.disk.ready().and_then(|l| l.forecast.as_ref());
    usize::from(matches!(filling, Some(Forecast::FullIn { days, .. }) if *days <= 30.0))
}

/// The lines under the volume bar: free space, then the forecast, the Trash and whether
/// cleanup is on.
pub fn lines(board: &Board) -> Option<[Vec<(String, Ink)>; 2]> {
    let list = board.disk.ready()?;
    Some([space(list), outlook(list)])
}

fn space(list: &DiskList) -> Vec<(String, Ink)> {
    let Some(s) = &list.space else {
        return vec![("macOS did not report free space.".into(), Ink::Muted)];
    };
    let mut out = vec![
        (bytes(s.available), Ink::Strong),
        (
            format!(" available of {} on {}", bytes(s.total), s.volume),
            Ink::Muted,
        ),
    ];
    if s.purgeable > 0 {
        out.push((" · ".into(), Ink::Faint));
        out.push((bytes(s.purgeable), Ink::Text));
        out.push((" purgeable".into(), Ink::Muted));
    }
    match s.snapshots.len() {
        0 => {}
        n => {
            out.push((" · ".into(), Ink::Faint));
            out.push((
                format!("{n} snapshot{}", if n == 1 { "" } else { "s" }),
                Ink::Text,
            ));
        }
    }
    out
}

fn outlook(list: &DiskList) -> Vec<(String, Ink)> {
    let mut out = match &list.forecast {
        Some(Forecast::FullIn { days, per_day, .. }) => {
            let tone = if *days < 7.0 {
                Tone::Danger
            } else {
                Tone::Warn
            };
            vec![
                (
                    format!("full in ~{:.0} days", days.max(1.0)),
                    Ink::Tone(tone),
                ),
                (
                    format!(" at −{}/day", disk_bytes(per_day.unsigned_abs())),
                    Ink::Muted,
                ),
            ]
        }
        Some(Forecast::NotFilling { .. }) => vec![("not filling".into(), Ink::Tone(Tone::Ok))],
        Some(Forecast::Stale { .. }) => vec![(
            "no forecast: the recorder has stopped; scry daemon status".into(),
            Ink::Muted,
        )],
        Some(Forecast::Warming {
            have_hours,
            need_hours,
        }) => vec![(
            format!(
                "forecast in ~{:.0} h, after 48 h of samples",
                (need_hours - have_hours).max(1.0)
            ),
            Ink::Muted,
        )],
        Some(Forecast::NoSamples) | None => vec![(
            "no forecast: scry daemon install records history".into(),
            Ink::Muted,
        )],
    };
    if let Some(t) = list.trash.as_ref().filter(|t| t.bytes() > 0) {
        out.push((" · ".into(), Ink::Faint));
        out.push(("Trash ".into(), Ink::Muted));
        let at_least = matches!(t, scry_core::disk::trash::TrashSize::Scry { .. });
        out.push((
            format!("{}{}", if at_least { "≥ " } else { "" }, bytes(t.bytes())),
            Ink::Text,
        ));
    }
    out.push((" · ".into(), Ink::Faint));
    if list.destructive {
        out.push((format!("≥ {}", bytes(list.reclaimable)), Ink::Text));
        out.push((" to clean up".into(), Ink::Muted));
    } else {
        out.push(("cleanup off".into(), Ink::Muted));
    }
    out
}
