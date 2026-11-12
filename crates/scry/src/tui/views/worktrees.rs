//! Worktrees: state, size and probable creator of each linked worktree under the code
//! roots. Git state comes first and sizes after, so the size column waits with `…`.

use scry_core::config::DEFAULT_CODE_ROOTS;
use scry_core::view::{Board, Load, Tab, Tone, WorktreeItem, WorktreeList, human_bytes};

use super::{Cell, Column, Row, SortKey, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Worktrees,
    columns: &[
        Column::fill("WORKTREE", 16),
        Column::fill("REPO", 8).from(76),
        Column::fill("BRANCH", 10).from(100),
        Column::fixed("SIZE", 5),
        Column::fixed("CREATOR", 11).from(70),
        Column::fixed("LAST", 4).from(90),
        Column::fixed("STATE", 14),
    ],
    rows,
    // `empty` below says which folders were searched.
    empty: ("No linked worktrees.", ""),
    badge,
    hints: &[("t", "clean up")],
    reap: None,
};

fn rows(board: &Board) -> Load<Vec<Row>> {
    match board.worktrees.as_ref() {
        // Codex's own worktree folder is read even without roots.
        Load::Ready(list) if list.roots.is_empty() && list.rows.is_empty() => {
            Load::Unavailable(format!(
                "No code folders to search for worktrees.\nScry looks in {} when they \
                 exist. Set code_roots in {} to search your own.",
                DEFAULT_CODE_ROOTS.join(", "),
                list.config
            ))
        }
        other => other.map(|list| list.rows.iter().map(|w| row(w, list.sizing)).collect()),
    }
}

fn row(w: &WorktreeItem, sizing: bool) -> Row {
    let size = match w.size {
        Some(b) => Cell::num(human_bytes(b), b),
        // Every row waits for its size together, so all sort alike meanwhile.
        None if sizing => Cell::num("…", 0),
        // Unreadable, or a folder that is gone: sorts last.
        None => Cell {
            key: SortKey::None,
            ..Cell::num("-", 0)
        },
    };
    Row {
        id: w.id.clone(),
        depth: 0,
        cells: vec![
            Cell::text(&w.name),
            Cell::muted(&w.repo),
            Cell::muted(&w.branch),
            size,
            Cell::text(&w.creator),
            Cell::muted(w.last_commit.as_deref().unwrap_or("")),
            Cell::state(Some(w.state)),
        ],
        detail: Some(w.detail.clone()),
        target: None,
        jump: None,
    }
}

/// What a scan that found nothing says: where it looked, and where to change that.
pub fn empty(board: &Board) -> (String, String) {
    let Some(list) = board.worktrees.ready() else {
        return (VIEW.empty.0.into(), String::new());
    };
    (
        VIEW.empty.0.into(),
        format!(
            "Searched {} under {} and Codex's worktree folder. Set code_roots in {} to \
             search other folders.",
            repos(list),
            list.roots.join(", "),
            list.config
        ),
    )
}

fn repos(list: &WorktreeList) -> String {
    match list.repos {
        1 => "1 repo".into(),
        n => format!("{n} repos"),
    }
}

/// Worktrees that ask for something: can go, can prune, needs a look.
fn badge(board: &Board) -> usize {
    board.worktrees.ready().map_or(0, |list| {
        list.rows
            .iter()
            .filter(|w| w.state.tone() == Tone::Warn)
            .count()
    })
}
