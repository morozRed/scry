//! Disk: free space and its forecast in a band above the list, then the catalog's items
//! ranked by what deleting them frees, each with the cleanup it allows. `t` cleans up the
//! selected item.

use scry_core::disk::clean::Allows;
use scry_core::disk::forecast::Forecast;
use scry_core::view::disk::{DiskItem, DiskList};
use scry_core::view::worktrees::bytes;
use scry_core::view::{Board, Load, Tab, Tone, disk_bytes};

use super::{Cell, Column, Ink, Row, SortKey, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Disk,
    columns: &[
        Column::fill("ITEM", 18),
        Column::fixed("KIND", 13).from(84),
        Column::fixed("SIZE", 5),
        Column::fixed("FREES", 6),
        Column::fixed("CLEANUP", 7),
        Column::fixed("BY", 15).from(110),
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
    Row {
        id: d.id.clone(),
        depth: 0,
        cells: vec![
            Cell::text(&d.name).with_context(context),
            Cell::muted(d.kind.word()),
            size(d.size, "", "gone"),
            size(d.frees, if d.at_least { "≥" } else { "" }, "-"),
            cleanup(d),
            Cell::muted(&d.creator),
        ],
        detail: Some(d.detail.clone()),
        target: None,
        jump: None,
    }
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

/// The band above the list: free space, then the forecast, the Trash and whether cleanup
/// is on.
pub fn band(board: &Board) -> Option<Vec<Vec<(String, Ink)>>> {
    let list = board.disk.ready()?;
    Some(vec![space(list), outlook(list)])
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
