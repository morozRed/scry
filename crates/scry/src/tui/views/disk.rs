//! Disk: free space and what can be reclaimed. Not built yet, so it says what it will show
//! and what its history needs.

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
    Load::Unavailable(
        "The Disk view is coming in a later version.\nIt will show free space, its trend and \
         what can be reclaimed.\nThe trend needs history, which the background recorder \
         keeps.\nStart it with scry daemon install."
            .into(),
    )
}
