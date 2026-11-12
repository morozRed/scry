//! Worktrees: state, size and probable creator. Not collected yet, so it says so.

use scry_core::view::{Board, Load, Tab};

use super::{Row, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Worktrees,
    columns: &[],
    rows,
    empty: ("No worktrees.", ""),
    badge: |_| 0,
    hints: &[],
    reap: None,
};

fn rows(_: &Board) -> Load<Vec<Row>> {
    Load::Unavailable("Worktree scanning is not built yet.".into())
}
