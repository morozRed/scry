use scry_core::view::{Board, Load, Tab};

use super::{Row, ViewDef};

pub const VIEW: ViewDef = ViewDef {
    tab: Tab::Disk,
    columns: &[],
    rows,
    empty: ("Nothing to reclaim.", ""),
    badge: |_| 0,
    hints: &[],
    reap: None,
};

fn rows(_: &Board) -> Load<Vec<Row>> {
    Load::Unavailable("Disk scanning is not built yet.".into())
}
