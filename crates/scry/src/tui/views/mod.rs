//! One file per view. Each fills a [`ViewDef`]; [`VIEWS`] is the only place that lists
//! them, so a later PR adds or fills a view by editing its own file and at most one line here.

mod agents;
mod disk;
mod overview;
mod ports;
mod worktrees;

use std::cmp::Ordering;
use std::collections::HashSet;

use scry_core::view::{Board, Detail, Load, State, Tab, Target};

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

    pub fn state(state: Option<State>) -> Cell {
        match state {
            Some(s) => Cell {
                kind: CellKind::State(s),
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
    pub hits: Vec<u32>,
}

/// Sorts siblings, applies the filter (keeping a match's ancestors and descendants so the
/// tree still reads), folds collapsed rows, and draws the tree glyphs.
pub fn shown<'a>(
    rows: &'a [Row],
    sort: Option<Sort>,
    filter: &str,
    collapsed: &HashSet<String>,
) -> Vec<Shown<'a>> {
    let tree = rows.iter().any(|r| r.depth > 0);
    let order = sorted(rows, sort);
    let rows: Vec<&Row> = order.iter().map(|&i| &rows[i]).collect();
    let has_children = |i: usize| rows.get(i + 1).is_some_and(|n| n.depth > rows[i].depth);

    let mut fuzzy = crate::tui::filter::Fuzzy::new(filter);
    let keep: Vec<bool> = match &mut fuzzy {
        None => vec![true; rows.len()],
        Some(f) => {
            let hit: Vec<bool> = rows.iter().map(|r| f.matches(&haystack(r))).collect();
            within_matches(&rows, &hit)
        }
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
        let fold = fuzzy.is_none() && has_children(i) && collapsed.contains(&row.id);
        let folded = if fold {
            skip_below = Some(row.depth);
            rows[i + 1..]
                .iter()
                .take_while(|r| r.depth > row.depth)
                .count()
        } else {
            0
        };
        let hits = match (&mut fuzzy, row.cells.first()) {
            (Some(f), Some(c)) => f.hits(&c.text),
            _ => Vec::new(),
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
            hits,
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

fn haystack(row: &Row) -> String {
    let mut s = String::new();
    for c in &row.cells {
        s.push_str(&c.text);
        s.push(' ');
        if let Some(ctx) = &c.context {
            s.push_str(ctx);
            s.push(' ');
        }
    }
    s
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
    let cmp = |a: &Row, b: &Row| {
        let ka = a.cells.get(sort.column).map(|c| &c.key);
        let kb = b.cells.get(sort.column).map(|c| &c.key);
        let o = ka.partial_cmp(&kb).unwrap_or(Ordering::Equal);
        if sort.descending { o.reverse() } else { o }
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

    #[test]
    fn guides_follow_depth_and_last_sibling() {
        let rows = tree();
        let out = shown(&rows, None, "", &HashSet::new());
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
        let out = shown(&rows, None, "", &folded);
        assert_eq!(lines(&out), ["▸ claude", "▾ codex", "  └ vite"]);
        assert_eq!(out[0].folded, 3);
    }

    #[test]
    fn filter_keeps_ancestors_and_ignores_folds() {
        let rows = tree();
        let folded = HashSet::from(["claude".to_string()]);
        let out = shown(&rows, None, "nxt", &folded);
        assert_eq!(lines(&out), ["▾ claude", "  └ npm", "    └ next"]);
        assert_eq!(out[2].hits, [0, 2, 3]);
    }

    #[test]
    fn sort_orders_siblings_and_keeps_subtrees() {
        let rows = tree();
        let by_ram = Some(Sort {
            column: 1,
            descending: true,
        });
        let out = shown(&rows, by_ram, "", &HashSet::new());
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
