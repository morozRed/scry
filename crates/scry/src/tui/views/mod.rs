//! One file per view. Each fills a [`ViewDef`]; [`VIEWS`] is the only place that lists
//! them, so a later PR adds or fills a view by editing its own file and at most one line here.

mod agents;
mod disk;
mod overview;
mod ports;
mod worktrees;

use std::cmp::Ordering;
use std::collections::HashSet;

use scry_core::disk::catalog::Kind;
use scry_core::view::{Board, Detail, DiskItem, Load, State, Tab, Target, Tone};

pub static VIEWS: [ViewDef; 5] = [
    overview::VIEW,
    ports::VIEW,
    agents::VIEW,
    disk::VIEW,
    worktrees::VIEW,
];

pub fn view(tab: Tab) -> &'static ViewDef {
    VIEWS
        .iter()
        .find(|v| v.tab == tab)
        .expect("every tab is registered in VIEWS")
}

/// What an empty list says: a title, and a hint about what would fill it. Worktrees names
/// the folders it searched, which only the board knows.
pub fn empty(view: &ViewDef, board: &Board) -> (String, String) {
    match view.tab {
        Tab::Worktrees => worktrees::empty(board),
        _ => (view.empty.0.into(), view.empty.1.into()),
    }
}

/// Whether an empty list is an all-clear, drawn with ✓. No agent sessions says nothing
/// about the machine's health, so Agents shows the neutral ○.
pub fn empty_is_clear(view: &ViewDef) -> bool {
    view.tab != Tab::Agents
}

/// How a band's text is inked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    Text,
    Strong,
    Muted,
    Faint,
    Tone(Tone),
}

/// Lines a view shows above its list: Disk's free space and forecast.
pub fn band(view: &ViewDef, board: &Board) -> Option<Vec<Vec<(String, Ink)>>> {
    match view.tab {
        Tab::Disk => disk::band(board),
        _ => None,
    }
}

/// The disk item a row stands for, which `t` cleans up: the row itself on Disk, and the
/// worktree's item on Worktrees, whose rows are named by their path.
pub fn disk_item<'a>(tab: Tab, board: &'a Board, row: &str) -> Option<&'a DiskItem> {
    let list = board.disk.ready()?;
    match tab {
        Tab::Disk => list.items.iter().find(|d| d.id == row),
        Tab::Worktrees => list
            .items
            .iter()
            .find(|d| d.kind == Kind::Worktree && d.path == row),
        _ => None,
    }
}

pub struct ViewDef {
    pub tab: Tab,
    pub columns: &'static [Column],
    /// The rows in their natural order. A tree is flattened depth first.
    pub rows: fn(&Board) -> Load<Vec<Row>>,
    /// What an empty list says, and what to do about it.
    pub empty: (&'static str, &'static str),
    /// The count shown beside the tab title, when it is worth a look.
    pub badge: fn(&Board) -> usize,
    /// Keys this view adds to the footer.
    pub hints: &'static [(&'static str, &'static str)],
    /// What `X` would act on.
    pub reap: Option<fn(&Board) -> Vec<Target>>,
}

pub struct Column {
    pub title: &'static str,
    pub width: Width,
    /// Hidden when the list is narrower than this.
    pub min_list_width: u16,
    pub sortable: bool,
}

#[derive(Clone, Copy)]
pub enum Width {
    Fixed(u16),
    /// Shares what is left, never below this.
    Fill(u16),
}

/// The columns shown in a list this wide, by index.
pub fn visible(columns: &[Column], list_width: u16) -> Vec<usize> {
    (0..columns.len())
        .filter(|&i| columns[i].min_list_width <= list_width)
        .collect()
}

impl Column {
    pub const fn fixed(title: &'static str, width: u16) -> Column {
        Column {
            title,
            width: Width::Fixed(width),
            min_list_width: 0,
            sortable: true,
        }
    }

    pub const fn fill(title: &'static str, min: u16) -> Column {
        Column {
            title,
            width: Width::Fill(min),
            min_list_width: 0,
            sortable: true,
        }
    }

    pub const fn from(mut self, list_width: u16) -> Column {
        self.min_list_width = list_width;
        self
    }

    pub const fn unsorted(mut self) -> Column {
        self.sortable = false;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub depth: u8,
    pub cells: Vec<Cell>,
    pub detail: Option<Detail>,
    /// What `x` would stop.
    pub target: Option<Target>,
    /// Where `enter` goes, for rows that stand for something in another view.
    pub jump: Option<(Tab, Option<String>)>,
}

impl Row {
    /// The state its state column shows, if it has one.
    pub fn state(&self) -> Option<State> {
        self.cells.iter().find_map(|c| match c.kind {
            CellKind::State(s) => Some(s),
            _ => None,
        })
    }

    /// The same on screen: text, nesting and detail, whatever the sort keys under the text.
    pub fn looks_like(&self, other: &Row) -> bool {
        self.id == other.id
            && self.depth == other.depth
            && self.detail == other.detail
            && self.cells.len() == other.cells.len()
            && self
                .cells
                .iter()
                .zip(&other.cells)
                .all(|(a, b)| a.text == b.text && a.context == b.context && a.kind == b.kind)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub text: String,
    /// Shown muted after the text, as a project beside its agent.
    pub context: Option<String>,
    pub kind: CellKind,
    pub key: SortKey,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellKind {
    Text,
    Muted,
    /// Right-aligned.
    Num,
    /// Glyph and word.
    State(State),
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum SortKey {
    None,
    Num(u64),
    Text(String),
}

impl Cell {
    pub fn text(s: impl Into<String>) -> Cell {
        let text = s.into();
        Cell {
            key: SortKey::Text(text.to_lowercase()),
            text,
            context: None,
            kind: CellKind::Text,
        }
    }

    pub fn muted(s: impl Into<String>) -> Cell {
        Cell {
            kind: CellKind::Muted,
            ..Cell::text(s)
        }
    }

    pub fn num(s: impl Into<String>, key: u64) -> Cell {
        Cell {
            kind: CellKind::Num,
            key: SortKey::Num(key),
            ..Cell::text(s)
        }
    }

    /// Sorts by how much the state asks of the user: full, then leftover, stale and
    /// filling, then kept, the quiet ones, and live last.
    pub fn state(state: Option<State>) -> Cell {
        match state {
            Some(s) => Cell {
                kind: CellKind::State(s),
                key: SortKey::Num(match s.tone() {
                    Tone::Danger => 0,
                    Tone::Warn => 1,
                    Tone::Info => 2,
                    Tone::Quiet => 3,
                    Tone::Ok => 4,
                }),
                ..Cell::text(s.word())
            },
            None => Cell::empty(),
        }
    }

    pub fn empty() -> Cell {
        Cell {
            key: SortKey::None,
            ..Cell::text("")
        }
    }

    pub fn with_context(mut self, context: Option<impl Into<String>>) -> Cell {
        self.context = context.map(Into::into);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub column: usize,
    pub descending: bool,
}

/// A row as the list shows it: with its tree glyphs and the characters the filter matched.
#[derive(Debug)]
pub struct Shown<'a> {
    pub row: &'a Row,
    pub prefix: String,
    /// Descendants hidden under a folded row.
    pub folded: usize,
    /// The row matched the filter itself, rather than being kept as a match's ancestor or
    /// child. Always true without a filter.
    pub matched: bool,
    /// What the filter matched, per cell; empty without a filter.
    pub hits: Vec<Hits>,
}

/// Grapheme indices the filter matched in one cell's text and in its context.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hits {
    pub text: Vec<usize>,
    pub context: Vec<usize>,
}

/// What the list is asked to show.
pub struct Query<'a> {
    pub sort: Option<Sort>,
    pub filter: &'a str,
    pub folded: &'a HashSet<String>,
    /// The columns on screen. The filter only matches these, so every match it keeps has
    /// something highlighted.
    pub columns: &'a [usize],
}

/// Sorts siblings, applies the filter (keeping a match's ancestors and descendants so the
/// tree still reads), folds collapsed rows, and draws the tree glyphs.
pub fn shown<'a>(rows: &'a [Row], q: &Query) -> Vec<Shown<'a>> {
    let tree = rows.iter().any(|r| r.depth > 0);
    let order = sorted(rows, q.sort);
    let rows: Vec<&Row> = order.iter().map(|&i| &rows[i]).collect();
    let has_children = |i: usize| rows.get(i + 1).is_some_and(|n| n.depth > rows[i].depth);

    let mut fuzzy = crate::tui::filter::Fuzzy::new(q.filter);
    let found: Vec<Option<Vec<Hits>>> = match &mut fuzzy {
        None => vec![Some(Vec::new()); rows.len()],
        Some(f) => rows.iter().map(|r| matches(f, r, q.columns)).collect(),
    };
    let hit: Vec<bool> = found.iter().map(Option::is_some).collect();
    let keep = if fuzzy.is_some() {
        within_matches(&rows, &hit)
    } else {
        hit.clone()
    };

    let mut out: Vec<Shown> = Vec::new();
    let mut skip_below: Option<u8> = None;
    for (i, row) in rows.iter().enumerate() {
        if skip_below.is_some_and(|d| row.depth > d) {
            continue;
        }
        skip_below = None;
        if !keep[i] {
            continue;
        }
        let fold = fuzzy.is_none() && has_children(i) && q.folded.contains(&row.id);
        let folded = if fold {
            skip_below = Some(row.depth);
            rows[i + 1..]
                .iter()
                .take_while(|r| r.depth > row.depth)
                .count()
        } else {
            0
        };
        let marker = match (has_children(i), fold) {
            (true, true) => "▸ ",
            (true, false) if row.depth == 0 => "▾ ",
            _ if row.depth == 0 => "  ",
            _ => "",
        };
        out.push(Shown {
            row,
            prefix: marker.to_string(),
            folded,
            matched: hit[i],
            hits: found[i].clone().unwrap_or_default(),
        });
    }
    if tree {
        draw_guides(&mut out);
    } else {
        out.iter_mut().for_each(|s| s.prefix.clear());
    }
    out
}

/// Numbers align right; the first row that has the column decides.
pub fn is_numeric<'a>(rows: impl IntoIterator<Item = &'a Row>, col: usize) -> bool {
    rows.into_iter()
        .find_map(|r| r.cells.get(col).filter(|c| c.key != SortKey::None))
        .is_some_and(|c| c.kind == CellKind::Num)
}

/// Matches a row field by field: each shown cell's text, and its context.
fn matches(f: &mut crate::tui::filter::Fuzzy, row: &Row, columns: &[usize]) -> Option<Vec<Hits>> {
    let mut fields = Vec::new();
    let mut owner = Vec::new();
    for &c in columns {
        let Some(cell) = row.cells.get(c) else {
            continue;
        };
        fields.push(cell.text.as_str());
        owner.push((c, false));
        if let Some(ctx) = &cell.context {
            fields.push(ctx.as_str());
            owner.push((c, true));
        }
    }
    let found = f.row(&fields)?;
    let mut hits = vec![Hits::default(); row.cells.len()];
    for ((c, context), at) in owner.into_iter().zip(found) {
        if context {
            hits[c].context = at;
        } else {
            hits[c].text = at;
        }
    }
    Some(hits)
}

/// A row stays if it matched, or sits above or below a row that did.
fn within_matches(rows: &[&Row], hit: &[bool]) -> Vec<bool> {
    let mut keep = hit.to_vec();
    for i in 0..rows.len() {
        if !hit[i] {
            continue;
        }
        let d = rows[i].depth;
        let mut want = d;
        for j in (0..i).rev() {
            if rows[j].depth < want {
                keep[j] = true;
                want = rows[j].depth;
            }
        }
        for j in i + 1..rows.len() {
            if rows[j].depth <= d {
                break;
            }
            keep[j] = true;
        }
    }
    keep
}

fn draw_guides(out: &mut [Shown]) {
    let depths: Vec<u8> = out.iter().map(|s| s.row.depth).collect();
    let mut open: Vec<bool> = Vec::new();
    for i in 0..out.len() {
        let d = depths[i] as usize;
        if d == 0 {
            open.clear();
            continue;
        }
        let last = depths[i + 1..]
            .iter()
            .find(|&&n| n as usize <= d)
            .is_none_or(|&n| (n as usize) < d);
        let mut prefix = String::from("  ");
        for k in 1..d {
            prefix.push_str(if open.get(k).copied().unwrap_or(false) {
                "│ "
            } else {
                "  "
            });
        }
        prefix.push_str(if last { "└ " } else { "├ " });
        prefix.push_str(&out[i].prefix);
        out[i].prefix = prefix;
        open.resize(d + 1, false);
        open[d] = !last;
    }
}

/// Indices in display order: siblings sorted by the column, each subtree kept under its
/// parent.
fn sorted(rows: &[Row], sort: Option<Sort>) -> Vec<usize> {
    let Some(sort) = sort else {
        return (0..rows.len()).collect();
    };
    // Rows without a value come last whichever way the column sorts.
    fn key(r: &Row, column: usize) -> Option<&SortKey> {
        r.cells
            .get(column)
            .map(|c| &c.key)
            .filter(|k| **k != SortKey::None)
    }
    let cmp = |a: &Row, b: &Row| match (key(a, sort.column), key(b, sort.column)) {
        (Some(ka), Some(kb)) => {
            let o = ka.partial_cmp(kb).unwrap_or(Ordering::Equal);
            if sort.descending { o.reverse() } else { o }
        }
        (a, b) => b.is_some().cmp(&a.is_some()),
    };
    let mut out = Vec::with_capacity(rows.len());
    sort_level(rows, 0, rows.len(), &cmp, &mut out);
    out
}

fn sort_level(
    rows: &[Row],
    start: usize,
    end: usize,
    cmp: &impl Fn(&Row, &Row) -> Ordering,
    out: &mut Vec<usize>,
) {
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut i = start;
    while i < end {
        let d = rows[i].depth;
        let mut j = i + 1;
        while j < end && rows[j].depth > d {
            j += 1;
        }
        groups.push((i, j));
        i = j;
    }
    groups.sort_by(|a, b| cmp(&rows[a.0], &rows[b.0]));
    for (s, e) in groups {
        out.push(s);
        sort_level(rows, s + 1, e, cmp, out);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn r(id: &str, depth: u8, ram: u64) -> Row {
        Row {
            id: id.into(),
            depth,
            cells: vec![Cell::text(id), Cell::num(ram.to_string(), ram)],
            detail: None,
            target: None,
            jump: None,
        }
    }

    fn tree() -> Vec<Row> {
        vec![
            r("claude", 0, 10),
            r("npm", 1, 1),
            r("next", 2, 5),
            r("chrome", 1, 3),
            r("codex", 0, 20),
            r("vite", 1, 4),
        ]
    }

    fn lines(shown: &[Shown]) -> Vec<String> {
        shown
            .iter()
            .map(|s| format!("{}{}", s.prefix, s.row.id))
            .collect()
    }

    fn query<'a>(sort: Option<Sort>, filter: &'a str, folded: &'a HashSet<String>) -> Query<'a> {
        Query {
            sort,
            filter,
            folded,
            columns: &[0, 1],
        }
    }

    #[test]
    fn guides_follow_depth_and_last_sibling() {
        let rows = tree();
        let out = shown(&rows, &query(None, "", &HashSet::new()));
        assert_eq!(
            lines(&out),
            [
                "▾ claude",
                "  ├ npm",
                "  │ └ next",
                "  └ chrome",
                "▾ codex",
                "  └ vite"
            ]
        );
    }

    #[test]
    fn folding_hides_descendants_and_counts_them() {
        let rows = tree();
        let folded = HashSet::from(["claude".to_string()]);
        let out = shown(&rows, &query(None, "", &folded));
        assert_eq!(lines(&out), ["▸ claude", "▾ codex", "  └ vite"]);
        assert_eq!(out[0].folded, 3);
    }

    #[test]
    fn filter_keeps_ancestors_and_ignores_folds() {
        let rows = tree();
        let folded = HashSet::from(["claude".to_string()]);
        let out = shown(&rows, &query(None, "nxt", &folded));
        assert_eq!(lines(&out), ["▾ claude", "  └ npm", "    └ next"]);
        assert_eq!(out[2].hits[0].text, [0, 2, 3]);
        assert_eq!(
            out.iter().map(|s| s.matched).collect::<Vec<_>>(),
            [false, false, true]
        );
    }

    #[test]
    fn filter_matches_one_column_at_a_time_and_only_shown_ones() {
        let mut rows = tree();
        rows[3].cells.push(Cell::text("stamp"));
        // "chrome" + "stamp" holds c, h, s, t in order, but no single field does.
        let none = HashSet::new();
        let q = Query {
            columns: &[0, 1, 2],
            ..query(None, "chst", &none)
        };
        assert!(shown(&rows, &q).is_empty());
        // A hidden column does not match.
        let q = Query {
            columns: &[0],
            ..query(None, "stamp", &none)
        };
        assert!(shown(&rows, &q).is_empty());
        let q = Query {
            columns: &[0, 2],
            ..query(None, "stamp", &none)
        };
        let out = shown(&rows, &q);
        let chrome = out.iter().find(|s| s.row.id == "chrome").unwrap();
        assert_eq!(chrome.hits[2].text, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn sort_orders_siblings_and_keeps_subtrees() {
        let rows = tree();
        let by_ram = Some(Sort {
            column: 1,
            descending: true,
        });
        let out = shown(&rows, &query(by_ram, "", &HashSet::new()));
        assert_eq!(
            lines(&out),
            [
                "▾ codex",
                "  └ vite",
                "▾ claude",
                "  ├ chrome",
                "  └ npm",
                "    └ next"
            ]
        );
    }

    #[test]
    fn every_tab_has_one_view() {
        for tab in Tab::ALL {
            assert_eq!(VIEWS.iter().filter(|v| v.tab == tab).count(), 1);
        }
    }
}
