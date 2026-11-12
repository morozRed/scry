//! Screen snapshots from fixtures and the key-handling state machine.

use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier};
use scry_core::view::agents::AgentRow;
use scry_core::view::{Board, Detail, Evidence, Load, State, Tab, Tone, WorktreeList};

use super::app::{Action, App, Mode, SheetKind, Source};
use super::theme::{Colors, Look, Rgb, Theme};
use super::ui::draw;

fn mono() -> Theme {
    Theme::new(Look::Tinted, Colors::Mono)
}

fn tinted() -> Theme {
    Theme::new(
        Look::Tinted,
        Colors::True {
            bg: Rgb(255, 255, 255),
            fg: Rgb(0, 0, 0),
        },
    )
}

fn typical() -> App {
    let snap =
        scry_core::fixture::parse(include_str!("../../../../fixtures/typical.json")).unwrap();
    App::new(
        Board::from_snapshot(&snap),
        Source::Fixture("typical.json".into()),
        mono(),
    )
}

fn design_board() -> Board {
    Board::parse(include_str!("../../../../fixtures/design.json")).unwrap()
}

fn design() -> App {
    App::new(
        design_board(),
        Source::Fixture("design.json".into()),
        mono(),
    )
}

/// The design board with one more ended session that left `n` processes.
fn with_leftovers(n: usize) -> Board {
    let mut board = design_board();
    let Load::Ready(rows) = &mut board.agents else {
        unreachable!("the design fixture has agents");
    };
    let row = |id: String, depth, label: String, pid, state, evidence| AgentRow {
        id,
        depth,
        label,
        context: (depth == 0).then(|| "acme/batch".into()),
        pid,
        start_us: None,
        keep: None,
        ancestors: 0,
        session_id: None,
        ram: Some(48 << 20),
        ports: vec![],
        age_s: 5 * 3600,
        evidence,
        state: Some(state),
        detail: Detail::default(),
    };
    rows.push(row(
        "s-batch".into(),
        0,
        "claude".into(),
        None,
        State::Ended,
        None,
    ));
    for i in 0..n {
        rows.push(row(
            format!("w-{i}"),
            1,
            format!("node worker-{i:02}"),
            Some(61000 + i as i32),
            State::Leftover,
            Some(Evidence::Stamp),
        ));
    }
    board
}

fn render(app: &App, w: u16, h: u16) -> TestBackend {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| {
        draw(f, app);
    })
    .unwrap();
    t.backend().clone()
}

fn lines(b: &TestBackend) -> Vec<String> {
    let buf = b.buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn screen(b: &TestBackend) -> String {
    lines(b).join("\n")
}

/// Where `text` first starts on screen, reading down from the top.
fn find(buf: &Buffer, text: &str) -> Option<(u16, u16)> {
    find_from(buf, text, 0)
}

fn find_from(buf: &Buffer, text: &str, from: u16) -> Option<(u16, u16)> {
    for y in from..buf.area.height {
        let line: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        for x in 0..line.len() {
            if line[x..].concat().starts_with(text) {
                return Some((x as u16, y));
            }
        }
    }
    None
}

fn press(app: &mut App, keys: &str) {
    for c in keys.chars() {
        let code = match c {
            '\n' => KeyCode::Enter,
            '\x1b' => KeyCode::Esc,
            c => KeyCode::Char(c),
        };
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn sort(a: &App) -> Option<(usize, bool)> {
    a.state().sort.map(|s| (s.column, s.descending))
}

#[test]
fn overview_80x24() {
    insta::assert_snapshot!(render(&typical(), 80, 24));
}

#[test]
fn overview_120x40() {
    insta::assert_snapshot!(render(&typical(), 120, 40));
}

#[test]
fn help_overlay() {
    let mut a = typical();
    assert_eq!(a.on_key(key(KeyCode::Char('?'))), Action::Redraw);
    insta::assert_snapshot!(render(&a, 80, 24));
}

#[test]
fn too_small() {
    insta::assert_snapshot!(render(&typical(), 60, 10));
}

#[test]
fn unbuilt_views_say_so() {
    let mut a = typical();
    press(&mut a, "3");
    insta::assert_snapshot!(render(&a, 80, 24));
}

#[test]
fn design_overview_80x24() {
    insta::assert_snapshot!(render(&design(), 80, 24));
}

#[test]
fn design_agents_80x24() {
    let mut a = design();
    press(&mut a, "3jj");
    insta::assert_snapshot!(render(&a, 80, 24));
}

#[test]
fn design_agents_120x40() {
    let mut a = design();
    press(&mut a, "3jj");
    insta::assert_snapshot!(render(&a, 120, 40));
}

#[test]
fn design_agents_150x40() {
    let mut a = design();
    press(&mut a, "3jj");
    insta::assert_snapshot!(render(&a, 150, 40));
}

#[test]
fn design_ports_100x30() {
    let mut a = design();
    press(&mut a, "2");
    insta::assert_snapshot!(render(&a, 100, 30));
}

#[test]
fn reap_sheet_lists_children_first() {
    let mut a = design();
    press(&mut a, "3X");
    let Mode::Sheet(sheet) = &a.mode else {
        panic!("X should open a sheet, got {:?}", a.mode);
    };
    assert_eq!(sheet.kind, SheetKind::Reap);
    assert_eq!(sheet.targets[0].command, "next dev");
    insta::assert_snapshot!(render(&a, 120, 40));
}

#[test]
fn filter_keeps_the_tree_around_matches() {
    let mut a = design();
    press(&mut a, "3/vite");
    assert_eq!(a.mode, Mode::Filter);
    insta::assert_snapshot!(render(&a, 80, 24));
    press(&mut a, "\n");
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(a.state().filter, "vite");
    press(&mut a, "\x1b");
    assert_eq!(a.state().filter, "");
}

#[test]
fn every_view_has_loading_empty_and_failed_states() {
    let mut a = design();
    press(&mut a, "2");
    a.board.ports = Load::Loading;
    insta::assert_snapshot!("ports_loading", render(&a, 80, 24));
    a.board.ports = Load::Failed("sysctl net.inet.tcp.pcblist_n returned EPERM.".into());
    insta::assert_snapshot!("ports_failed", render(&a, 80, 24));
    a.board.ports = Load::Ready(Vec::new());
    insta::assert_snapshot!("ports_empty", render(&a, 80, 24));
}

#[test]
fn keys_switch_views_and_only_q_quits() {
    let mut a = typical();
    a.on_key(key(KeyCode::Char('3')));
    assert_eq!(a.tab, Tab::Agents);
    a.on_key(key(KeyCode::Tab));
    assert_eq!(a.tab, Tab::Disk);
    a.on_key(key(KeyCode::BackTab));
    a.on_key(key(KeyCode::BackTab));
    assert_eq!(a.tab, Tab::Ports);
    a.on_key(key(KeyCode::Char('?')));
    assert_eq!(a.on_key(key(KeyCode::Esc)), Action::Redraw);
    assert_eq!(a.mode, Mode::Normal);
    // One esc too many does nothing.
    assert_eq!(a.on_key(key(KeyCode::Esc)), Action::None);
    assert_eq!(a.on_key(key(KeyCode::Esc)), Action::None);
    assert_eq!(a.on_key(key(KeyCode::Char('q'))), Action::Quit);
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    assert_eq!(a.on_key(ctrl('c')), Action::Quit);
    assert_eq!(a.on_key(ctrl('z')), Action::Suspend);
}

#[test]
fn enter_on_an_attention_line_opens_its_row() {
    let mut a = design();
    press(&mut a, "\n");
    assert_eq!(a.tab, Tab::Agents);
    assert_eq!(a.state().selected.as_deref(), Some("s-4f2c"));
    press(&mut a, "\n");
    assert!(a.state().folded.contains("s-4f2c"));
}

#[test]
fn enter_on_a_process_does_nothing_and_h_goes_to_its_parent() {
    let mut a = design();
    press(&mut a, "3jj");
    assert_eq!(a.state().selected.as_deref(), Some("next-48213"));
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(a.state().selected.as_deref(), Some("next-48213"));
    assert!(a.state().folded.is_empty());
    press(&mut a, "h");
    assert_eq!(a.state().selected.as_deref(), Some("npm-48201"));
}

#[test]
fn a_sheet_waits_for_enter_and_esc_sends_nothing() {
    let mut a = design();
    press(&mut a, "3jjx");
    assert!(matches!(&a.mode, Mode::Sheet(s) if s.targets[0].pid == 48213));
    press(&mut a, "\x1b");
    assert_eq!(a.mode, Mode::Normal);
    assert!(a.toast.is_none());
    press(&mut a, "x\n");
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(
        a.toast.as_ref().unwrap().text,
        "This is a fixture, so nothing was sent."
    );
}

#[test]
fn sort_cycles_the_columns_on_screen_then_returns_to_the_natural_order() {
    // At 80 columns Agents hides PID, so s goes from SESSION straight to RAM.
    let mut a = design();
    press(&mut a, "3");
    render(&a, 80, 24);
    press(&mut a, "s");
    assert_eq!(sort(&a), Some((0, false)));
    press(&mut a, "s");
    assert_eq!(sort(&a), Some((2, true)));
    press(&mut a, "sssss");
    assert_eq!(sort(&a), None);

    // From 96 columns of list PID is on screen and takes its turn.
    let mut a = design();
    press(&mut a, "3");
    render(&a, 150, 40);
    press(&mut a, "ss");
    assert_eq!(sort(&a), Some((1, true)));
    // Narrowed so PID is hidden again, the list falls back to its natural order: no order
    // without an arrow.
    render(&a, 80, 24);
    assert_eq!(a.sort(), None);
    assert!(lines(&render(&a, 80, 24)).iter().all(|l| !l.contains('↓')));

    // Ports hides ADDR below 96.
    let mut a = design();
    press(&mut a, "2");
    render(&a, 80, 24);
    press(&mut a, "ss");
    assert_eq!(sort(&a).map(|s| s.0), Some(2));
}

#[test]
fn the_sort_arrow_never_clips_a_title() {
    let mut a = design();
    press(&mut a, "3");
    render(&a, 80, 24);
    // AGE, then EVIDENCE.
    press(&mut a, "ssss");
    assert_eq!(sort(&a).map(|s| s.0), Some(4));
    assert!(screen(&render(&a, 80, 24)).contains("AGE ↓"));
    press(&mut a, "s");
    assert_eq!(sort(&a).map(|s| s.0), Some(5));
    assert!(screen(&render(&a, 80, 24)).contains("EVIDENCE ↑"));

    let mut a = design();
    render(&a, 80, 24);
    press(&mut a, "s");
    assert!(screen(&render(&a, 80, 24)).contains("SIZE ↓"));
}

#[test]
fn growing_the_terminal_scrolls_rows_back_into_view() {
    let mut a = design();
    press(&mut a, "3");
    render(&a, 80, 24);
    press(&mut a, "G");
    let small = render(&a, 80, 24);
    assert!(
        !screen(&small).contains("acme/wt-auth"),
        "scrolled at 80×24"
    );
    let big = render(&a, 120, 40);
    let rows = lines(&big);
    assert!(
        rows[4].contains("claude") && rows[4].contains("acme/wt-auth"),
        "the first row is back at the top:\n{}",
        screen(&big)
    );
}

#[test]
fn a_long_reap_sheet_scrolls_and_keeps_its_totals() {
    let mut a = App::new(
        with_leftovers(20),
        Source::Fixture("design.json".into()),
        mono(),
    );
    press(&mut a, "3X");
    let Mode::Sheet(sheet) = &a.mode else {
        panic!("X should open a sheet");
    };
    let n = sheet.targets.len();
    assert_eq!(n, 24);
    let first = render(&a, 80, 24);
    insta::assert_snapshot!("reap_sheet_24_80x24", first);
    let text = screen(&first);
    assert!(text.contains("Reap 24 leftovers?"));
    assert!(text.contains("Frees about"));
    assert!(text.contains("more   j k scroll"));
    assert!(!text.contains("worker-19"));

    press(&mut a, "G");
    let last = screen(&render(&a, 80, 24));
    assert!(
        last.contains("worker-19"),
        "G shows the last target:\n{last}"
    );
    assert!(last.contains("above"));
    assert!(last.contains("Frees about"));
    // Every target was on screen at some point.
    let mut seen = std::collections::HashSet::new();
    press(&mut a, "g");
    for _ in 0..n {
        let s = screen(&render(&a, 80, 24));
        for i in 0..20 {
            if s.contains(&format!("worker-{i:02}")) {
                seen.insert(i);
            }
        }
        press(&mut a, "j");
    }
    assert_eq!(seen.len(), 20);
}

#[test]
fn the_frees_title_lines_up_with_its_numbers() {
    let mut a = design();
    press(&mut a, "3X");
    for w in [80, 120] {
        let b = render(&a, w, 40);
        let (x, y) = find(b.buffer(), "FREES").unwrap();
        let (nx, _) = find_from(b.buffer(), "412M", y + 1).unwrap();
        assert_eq!(x + 5, nx + 4, "at {w} columns:\n{}", screen(&b));
    }
}

#[test]
fn the_filter_matches_within_one_column_and_highlights_it() {
    let mut a = App::new(design_board(), Source::Fixture("d".into()), tinted());
    press(&mut a, "3/vite");
    let b = render(&a, 80, 24);
    let text = screen(&b);
    assert!(!text.contains("mcp-server-github"), "{text}");
    let (x, y) = find(b.buffer(), "vite").unwrap();
    let matched = a.theme.matched.fg;
    for dx in 0..4 {
        assert_eq!(b.buffer()[(x + dx, y)].fg, matched.unwrap());
    }
    // A match in another column is highlighted there.
    press(&mut a, "\x1b/guess");
    let b = render(&a, 80, 24);
    let (x, y) = find(b.buffer(), "guess").unwrap();
    assert_eq!(b.buffer()[(x, y)].fg, matched.unwrap());
}

#[test]
fn filtering_selects_the_first_match_not_its_session() {
    let mut a = design();
    press(&mut a, "3/vite");
    assert_eq!(a.state().selected.as_deref(), Some("vite-50110"));
    let b = render(&a, 80, 24);
    // The detail pane describes vite, not the codex session above it.
    assert!(screen(&b).contains("vite for the live Codex session"));
}

#[test]
fn folding_waits_until_the_filter_is_cleared() {
    let mut a = design();
    press(&mut a, "3/codex\n");
    assert_eq!(a.state().selected.as_deref(), Some("s-c0de"));
    press(&mut a, "\n");
    assert!(a.state().folded.is_empty(), "no hidden fold");
    assert!(a.toast.as_ref().unwrap().text.starts_with("Folding is off"));
    press(&mut a, "\x1b\n");
    assert!(a.state().folded.contains("s-c0de"));
}

#[test]
fn the_footer_offers_only_keys_that_work_here() {
    let mut a = design();
    press(&mut a, "4");
    let disk = lines(&render(&a, 80, 24)).pop().unwrap();
    for absent in ["move", "filter", "sort"] {
        assert!(!disk.contains(absent), "{disk}");
    }
    assert!(disk.contains("help") && disk.contains("quit"));
    press(&mut a, "3");
    let agents = lines(&render(&a, 80, 24)).pop().unwrap();
    for present in ["move", "fold", "reap", "keep", "filter", "refresh", "quit"] {
        assert!(agents.contains(present), "{agents}");
    }
    // Sort gives way at 80 columns; it is on the help card, and back when there is room.
    assert!(!agents.contains("sort"), "{agents}");
    let wide = lines(&render(&a, 120, 40)).pop().unwrap();
    assert!(wide.contains("s sort"), "{wide}");
}

#[test]
fn the_selected_row_has_a_cursor_and_a_tint() {
    let mut a = App::new(design_board(), Source::Fixture("d".into()), tinted());
    press(&mut a, "3jj");
    let b = render(&a, 80, 24);
    let (x, y) = find(b.buffer(), "next dev").unwrap();
    assert_eq!(b.buffer()[(0, y)].symbol(), "▌");
    assert_eq!(b.buffer()[(0, y)].fg, a.theme.accent.fg.unwrap());
    assert_eq!(b.buffer()[(x, y)].bg, a.theme.selection.bg.unwrap());
}

#[test]
fn a_fold_count_is_muted_not_faint() {
    let mut a = App::new(design_board(), Source::Fixture("d".into()), tinted());
    press(&mut a, "3\n");
    let b = render(&a, 80, 24);
    let (x, y) = find(b.buffer(), "+4").unwrap();
    assert_eq!(b.buffer()[(x, y)].fg, a.theme.muted.fg.unwrap());
}

#[test]
fn the_pane_moves_beside_the_list_only_when_the_list_keeps_96_columns() {
    let a = design();
    render(&a, 120, 40);
    assert_eq!(a.list_width.get(), 120);
    render(&a, 149, 40);
    assert_eq!(a.list_width.get(), 149);
    render(&a, 150, 40);
    assert!(a.list_width.get() >= 96);
    render(&a, 220, 50);
    assert!(a.list_width.get() >= 96);
}

#[test]
fn the_ansi_rule_keeps_a_blank_column_after_the_list() {
    let mut a = App::new(
        design_board(),
        Source::Fixture("d".into()),
        Theme::new(Look::Tinted, Colors::Ansi),
    );
    press(&mut a, "3");
    let b = render(&a, 150, 40);
    let list = a.list_width.get();
    let buf = b.buffer();
    for y in 5..20 {
        assert_eq!(buf[(list, y)].symbol(), " ", "row {y}:\n{}", screen(&b));
        assert_eq!(buf[(list + 1, y)].symbol(), "│");
    }
}

#[test]
fn a_rescan_glows_rows_that_are_new_or_changed_state() {
    let mut a = design();
    press(&mut a, "3");
    let mut board = a.board.clone();
    if let Load::Ready(rows) = &mut board.agents {
        rows[2].ram = Some(1);
    }
    assert!(
        a.replace_board(board.clone()).is_empty(),
        "RAM moves every sample"
    );
    if let Load::Ready(rows) = &mut board.agents {
        rows[6].state = Some(State::Leftover);
    }
    let fresh = with_leftovers(1);
    if let (Load::Ready(rows), Load::Ready(more)) = (&mut board.agents, &fresh.agents) {
        rows.extend(more[more.len() - 2..].iter().cloned());
    }
    assert_eq!(a.replace_board(board), ["mem-51244", "s-batch", "w-0"]);
}

#[test]
fn a_toast_never_hides_the_filter_being_typed() {
    let mut a = design();
    press(&mut a, "3");
    a.notify("Rescan failed: boom", Tone::Danger);
    press(&mut a, "/vi");
    let footer = lines(&render(&a, 80, 24)).pop().unwrap();
    assert!(
        footer.contains("/vi") && footer.contains("of 17"),
        "{footer}"
    );
    assert!(!footer.contains("Rescan failed"), "{footer}");
    // Once the filter is kept, the toast has the line again.
    press(&mut a, "\n");
    let footer = lines(&render(&a, 80, 24)).pop().unwrap();
    assert!(footer.contains("Rescan failed"), "{footer}");
}

#[test]
fn alt_chords_are_not_typed_into_the_filter() {
    let mut a = design();
    press(&mut a, "3/vi");
    a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
    a.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT));
    press(&mut a, "te");
    assert_eq!(a.state().filter, "vite");
}

#[test]
fn the_sheet_keeps_its_scroll_position_through_a_resize() {
    let mut a = App::new(
        with_leftovers(20),
        Source::Fixture("design.json".into()),
        mono(),
    );
    press(&mut a, "3X");
    render(&a, 80, 24);
    press(&mut a, "jjjjjjjjjj");
    let before = screen(&render(&a, 80, 24));
    assert!(
        before.contains("worker-06") && !before.contains("worker-05"),
        "{before}"
    );
    // Taller, every target fits and the sheet starts at the top; back at 24 rows it is
    // where it was.
    assert!(screen(&render(&a, 80, 50)).contains("48213"));
    assert_eq!(screen(&render(&a, 80, 24)), before);
    // After a shrink a key moves from what is on screen, not from an offset clamped away.
    press(&mut a, "G");
    render(&a, 80, 24);
    render(&a, 80, 30);
    press(&mut a, "k");
    let up = screen(&render(&a, 80, 30));
    assert!(!up.contains("worker-19"), "k moves up from the end:\n{up}");
}

#[test]
fn reaping_where_agents_are_not_tracked_says_so_neutrally() {
    let mut a = typical();
    press(&mut a, "3X");
    assert_eq!(a.mode, Mode::Normal);
    let toast = a.toast.as_ref().unwrap();
    assert_eq!(toast.text, "Agent sessions are not tracked yet.");
    assert_eq!(toast.tone, Tone::Quiet);
}

fn header_x(b: &TestBackend, title: &str) -> u16 {
    find(b.buffer(), title).unwrap().0
}

#[test]
fn column_titles_hold_still_while_a_filter_hides_their_numbers() {
    let mut a = design();
    press(&mut a, "3");
    let all = render(&a, 80, 24);
    // Neither row uvx keeps (its session and itself) has a port.
    press(&mut a, "/uvx");
    let few = render(&a, 80, 24);
    assert!(!screen(&few).contains(":3000"), "{}", screen(&few));
    for title in ["RAM", "PORT", "AGE", "STATE"] {
        assert_eq!(header_x(&all, title), header_x(&few, title), "{title}");
    }
}

#[test]
fn text_columns_take_their_content_and_leave_the_spare_width_at_the_right() {
    let mut a = design();
    press(&mut a, "3");
    for w in [120, 149] {
        let b = render(&a, w, 40);
        let (name, y) = find(b.buffer(), "node mcp-server-github").unwrap();
        let (pid, _) = find_from(b.buffer(), "48190", y).unwrap();
        let gap = pid - (name + "node mcp-server-github".len() as u16);
        assert!(gap <= 4, "{gap} columns between name and pid at {w}");
        let header = &lines(&b)[3];
        assert!(
            header.trim_end().len() < w as usize - 30,
            "the table ends well before the edge at {w}:\n{header}"
        );
    }
}

#[test]
fn the_detail_sits_under_the_list_and_the_footer_off_its_band() {
    let mut a = App::new(design_board(), Source::Fixture("d".into()), tinted());
    press(&mut a, "3jj");
    let b = render(&a, 120, 40);
    let buf = b.buffer();
    let surface = a.theme.surface.bg.unwrap();
    let (_, last) = find(buf, "python3 -m http.server").unwrap();
    assert_eq!(
        buf[(5, last + 1)].bg,
        surface,
        "the pane follows the last row"
    );
    assert!(screen(&b).contains(":3000 is held by next dev"));
    // Sized to the tallest detail in the view, then plain background down to the footer.
    let pane = (last + 1..40)
        .take_while(|&y| buf[(5, y)].bg == surface)
        .count();
    assert!((3..=7).contains(&pane), "pane is {pane} rows");
    for y in last + 1 + pane as u16..40 {
        assert_ne!(buf[(5, y)].bg, surface, "row {y} is off the band");
    }
    assert!(lines(&b)[39].contains("quit"));
}

#[test]
fn only_a_real_all_clear_gets_a_check_mark() {
    let mut a = design();
    press(&mut a, "3/zzzz");
    let text = screen(&render(&a, 80, 24));
    assert!(text.contains("○  No rows match the filter."), "{text}");
    assert!(!text.contains('✓'));
    // Every source read and nothing flagged: that is an all-clear.
    let mut board = design_board();
    board.overview.attention.clear();
    let a = App::new(board, Source::Fixture("d".into()), mono());
    assert!(screen(&render(&a, 80, 24)).contains("✓  Nothing needs attention."));
    // Live, where agents are not read, it says so instead.
    let text = screen(&render(&typical(), 80, 24));
    assert!(
        text.contains("○  Agent sessions are not collected yet"),
        "{text}"
    );
}

#[test]
fn the_help_card_is_raised_with_a_plain_margin() {
    let mut a = App::new(design_board(), Source::Fixture("d".into()), tinted());
    press(&mut a, "3?");
    let b = render(&a, 80, 24);
    let buf = b.buffer();
    let raised = a.theme.raised.bg.unwrap();
    assert_ne!(Some(raised), a.theme.surface.bg);
    let (x, y) = find(buf, "Keys").unwrap();
    // Three columns of padding inside the card, then two of plain background outside it.
    assert_eq!(buf[(x - 3, y)].bg, raised);
    for dx in [4, 5] {
        assert_eq!(buf[(x - dx, y)].bg, Color::Reset);
    }
    // The longest line keeps its padding before the right edge.
    let text = "fold and unfold; h goes to the parent";
    let (lx, ly) = find(buf, text).unwrap();
    let end = lx + text.len() as u16;
    for dx in 0..3 {
        assert_eq!(buf[(end + dx, ly)].bg, raised);
        assert_eq!(buf[(end + dx, ly)].symbol(), " ");
    }
}

#[test]
fn a_sheet_never_leaves_a_lone_list_row_above_it() {
    for n in 18..=26 {
        let mut a = App::new(with_leftovers(n), Source::Fixture("d".into()), tinted());
        press(&mut a, "3X");
        let b = render(&a, 150, 40);
        let (_, title) = find(b.buffer(), "Reap ").unwrap();
        // Below the tab bar and above the sheet's gap row and top padding.
        let above = lines(&b)[3..(title - 2).max(3) as usize]
            .iter()
            .filter(|l| !l.trim().is_empty())
            .count();
        assert!(
            above == 0 || above >= 3,
            "{n} workers leave {above} rows:\n{}",
            screen(&b)
        );
    }
}

#[test]
fn the_sheet_bolds_the_command_name_only() {
    let mut a = App::new(design_board(), Source::Fixture("d".into()), tinted());
    press(&mut a, "3X");
    let b = render(&a, 120, 40);
    let (_, head) = find(b.buffer(), "COMMAND").unwrap();
    let (x, y) = find_from(b.buffer(), "next dev", head).unwrap();
    let bold = |x| b.buffer()[(x, y)].modifier.contains(Modifier::BOLD);
    assert!(bold(x) && !bold(x + 5));
}

#[test]
fn a_long_ansi_sheet_has_one_rule_above_it() {
    let mut a = App::new(
        with_leftovers(20),
        Source::Fixture("d".into()),
        Theme::new(Look::Tinted, Colors::Ansi),
    );
    press(&mut a, "3X");
    let rules = lines(&render(&a, 80, 24))
        .iter()
        .filter(|l| l.chars().all(|c| c == '─'))
        .count();
    assert_eq!(rules, 2, "the top and bottom rules only");
}

#[test]
fn state_sorts_by_what_it_asks_of_you() {
    let mut a = design();
    press(&mut a, "3");
    render(&a, 120, 40);
    press(&mut a, "sssssss");
    assert_eq!(sort(&a), Some((6, false)));
    let Load::Ready(rows) = a.rows() else {
        unreachable!("the design fixture has agents")
    };
    let sessions: Vec<State> = a
        .shown(&rows)
        .iter()
        .filter(|s| s.row.depth == 0)
        .filter_map(|s| s.row.state())
        .collect();
    use State::{Ended, Live};
    assert_eq!(sessions, [Ended, Ended, Live, Live, Live]);
    // Leftovers lead within their session; the guess-owned process follows them.
    let text = screen(&render(&a, 120, 40));
    assert!(text.find("leftover").unwrap() < text.find("unowned").unwrap());
}

#[test]
fn filter_matches_are_marked_in_the_accent_not_the_warning() {
    let t = tinted();
    assert_eq!(t.matched.fg, t.accent.fg);
    assert_ne!(t.matched.fg, t.warn.fg);
    assert!(t.matched.add_modifier.contains(Modifier::UNDERLINED));
}

fn worktrees(f: impl FnOnce(&mut WorktreeList)) -> App {
    let mut board = design_board();
    if let Load::Ready(list) = &mut board.worktrees {
        f(list);
    }
    let mut a = App::new(board, Source::Fixture("design.json".into()), mono());
    press(&mut a, "5");
    a
}

#[test]
fn worktrees_show_state_size_creator_and_the_reason() {
    let mut a = worktrees(|_| {});
    insta::assert_snapshot!("design_worktrees_80x24", render(&a, 80, 24));
    press(&mut a, "j");
    insta::assert_snapshot!("design_worktrees_120x40", render(&a, 120, 40));
}

#[test]
fn worktrees_say_when_sizes_are_still_measured() {
    let a = worktrees(|list| {
        list.sizing = true;
        for w in &mut list.rows {
            w.size = None;
            w.frees = None;
        }
    });
    insta::assert_snapshot!("worktrees_sizing", render(&a, 80, 24));
    let text = screen(&render(&a, 80, 24));
    assert!(text.contains('…'), "{text}");
    assert!(!text.contains("1.3G"), "{text}");
}

#[test]
fn first_run_states_say_what_would_fill_each_view() {
    let mut a = worktrees(|list| list.rows.clear());
    insta::assert_snapshot!("worktrees_empty", render(&a, 80, 24));
    let text = screen(&render(&a, 80, 24));
    assert!(text.contains("✓  No linked worktrees."), "{text}");
    assert!(text.contains("~/sources, ~/work"), "{text}");
    assert!(text.contains("code_roots"), "{text}");

    // Without roots Codex's own folder is still read, and what it holds is listed.
    let c = worktrees(|list| list.roots.clear());
    assert!(screen(&render(&c, 80, 24)).contains("wt-auth"));

    let mut b = worktrees(|list| {
        list.rows.clear();
        list.roots.clear();
    });
    insta::assert_snapshot!("worktrees_no_roots", render(&b, 80, 24));
    b.board.worktrees = Load::Loading;
    insta::assert_snapshot!("worktrees_loading", render(&b, 80, 24));

    press(&mut a, "4");
    insta::assert_snapshot!("disk_first_run", render(&a, 80, 24));
    assert!(screen(&render(&a, 80, 24)).contains("scry daemon install"));

    press(&mut a, "2");
    a.board.ports = Load::Ready(Vec::new());
    assert!(screen(&render(&a, 80, 24)).contains("No process on this Mac is accepting"));
}

/// The header row, wide enough for the design board's pulse and the spinner's words.
fn header(a: &App) -> String {
    lines(&render(a, 120, 40)).remove(0)
}

#[test]
fn the_idle_header_shows_no_clock() {
    let a = App::new(design_board(), Source::Live, mono());
    insta::assert_snapshot!("header_idle", header(&a));
}

#[test]
fn the_header_spins_while_a_slow_refresh_runs() {
    let mut a = App::new(design_board(), Source::Live, mono());
    // 1.55 s in: frame 15 of the spinner, past the second after which it says what it does.
    a.fresh.scanning = Instant::now().checked_sub(Duration::from_millis(1550));
    insta::assert_snapshot!("header_refreshing", header(&a));
}

#[test]
fn the_spinner_shows_only_while_a_scan_is_slow_or_asked_for() {
    let mut a = App::new(design_board(), Source::Live, mono());
    let now = Instant::now();
    assert_eq!(a.activity(now), "");
    assert_eq!(a.activity_due(now), None, "idle wakes for nothing");
    // A quick automatic scan never shows.
    a.fresh.scanning = Some(now);
    assert_eq!(a.activity(now + Duration::from_millis(250)), "");
    assert_eq!(a.activity_due(now), Some(now + Duration::from_millis(300)));
    // Past 300 ms a quiet spinner turns, 10 frames a second, without words.
    assert_eq!(a.activity(now + Duration::from_millis(300)), "⠸");
    assert_eq!(a.activity(now + Duration::from_millis(450)), "⠼");
    assert_eq!(
        a.activity_due(now + Duration::from_millis(450)),
        Some(now + Duration::from_millis(500))
    );
    // Past a second it says what it is doing.
    assert_eq!(
        a.activity(now + Duration::from_millis(1000)),
        "⠋ refreshing…"
    );
    // One that `r` asked for spins at once.
    a.fresh.asked = true;
    assert_eq!(a.activity(now), "⠋");
    // The worktree view tells its own scan apart.
    a.fresh = Default::default();
    press(&mut a, "5");
    a.fresh.worktrees = Some(now);
    let slow = now + Duration::from_secs(2);
    assert_eq!(a.activity(slow), "⠋ reading worktrees…");
    if let Load::Ready(list) = &mut a.board.worktrees {
        list.sizing = true;
    }
    assert_eq!(a.activity(slow), "⠋ measuring sizes…");
    // A fixture names itself instead.
    assert!(header(&design()).ends_with("fixture design.json "));
}

#[test]
fn an_unchanged_refresh_changes_nothing_on_screen() {
    let mut a = App::new(design_board(), Source::Live, tinted());
    let mut motion = super::motion::Motion::new(a.theme.motion);
    for tab in "123".chars() {
        press(&mut a, &tab.to_string());
        let before = render(&a, 120, 40).buffer().clone();
        assert!(!super::receive(&mut a, &mut motion, super::Update::Started));
        // A few seconds later: ages move under text that still reads the same.
        let mut board = a.board.clone();
        if let Load::Ready(rows) = &mut board.agents {
            rows.iter_mut().for_each(|r| r.age_s += 5);
        }
        if let Load::Ready(rows) = &mut board.ports {
            for p in rows.iter_mut() {
                p.age_secs = p.age_secs.map(|s| s + 5);
            }
        }
        assert_ne!(board, a.board, "the reading itself differs");
        let drew = super::receive(&mut a, &mut motion, super::Update::Board(Box::new(board)));
        assert!(!drew, "tab {tab}: an unchanged reading asks for no frame");
        assert!(!motion.running(), "tab {tab}: and makes nothing glow");
        assert_eq!(a.activity(Instant::now()), "");
        assert_eq!(render(&a, 120, 40).buffer(), &before, "tab {tab}");
    }
    // An age whose text moves is a change.
    let mut board = a.board.clone();
    if let Load::Ready(rows) = &mut board.agents {
        rows[0].age_s += 3 * 86_400;
    }
    assert!(super::receive(
        &mut a,
        &mut motion,
        super::Update::Board(Box::new(board))
    ));
}

#[test]
fn r_refreshes_and_every_footer_lists_it() {
    let mut a = design();
    assert_eq!(a.on_key(key(KeyCode::Char('r'))), Action::Rescan);
    for tab in "12345".chars() {
        press(&mut a, &tab.to_string());
        let footer = lines(&render(&a, 80, 24)).pop().unwrap();
        assert!(footer.contains("r refresh"), "{footer}");
        assert!(footer.contains("q quit"), "{footer}");
    }
    press(&mut a, "?");
    assert!(screen(&render(&a, 80, 24)).contains("refresh now"));
}

#[test]
fn a_first_run_explains_what_agents_and_overview_will_show() {
    let mut board = design_board();
    board.overview.attention.clear();
    board.agents = Load::Ready(Vec::new());
    board.ports = Load::Ready(Vec::new());
    let mut a = App::new(board, Source::Fixture("d".into()), mono());
    let text = screen(&render(&a, 80, 24));
    assert!(text.contains("✓  Nothing needs attention."), "{text}");
    assert!(
        text.contains("Checked your processes for leftovers"),
        "{text}"
    );
    press(&mut a, "3");
    let text = screen(&render(&a, 80, 24));
    // No sessions is not an all-clear, so the mark is the neutral one.
    assert!(text.contains("○  No agent sessions are running."), "{text}");
    assert!(!text.contains('✓'), "{text}");
    assert!(
        text.contains("Claude Code, Codex, OpenCode and Gemini CLI"),
        "{text}"
    );
    assert!(text.contains("scry hook install claude"), "{text}");
}

/// The design board as the live TUI holds it: confirming acts instead of saying it is a
/// fixture. Nothing here sends a signal; the request is only returned.
fn live() -> App {
    App::new(design_board(), Source::Live, mono())
}

fn report(done: &[(i32, bool)]) -> scry_core::reap::Report {
    use scry_core::control::Outcome;
    use scry_core::reap::{Done, ReapTarget};
    scry_core::reap::Report {
        done: done
            .iter()
            .map(|&(pid, exited)| Done {
                target: ReapTarget {
                    id: scry_core::ProcId { pid, start_us: 1 },
                    command: "x".into(),
                    session: "claude 4f2c".into(),
                    session_id: None,
                    evidence: Some(Evidence::Stamp),
                    ram: None,
                },
                outcome: Some(if exited {
                    Outcome::Exited {
                        signal: "SIGTERM".into(),
                        after_ms: 40,
                    }
                } else {
                    Outcome::StillRunning { after_ms: 3000 }
                }),
                refusal: None,
                unaudited: None,
            })
            .collect(),
    }
}

#[test]
fn confirming_the_reap_sheet_acts_children_first_and_reports_in_the_footer() {
    let mut a = live();
    press(&mut a, "3X");
    let Action::Act(req) = a.on_key(key(KeyCode::Enter)) else {
        panic!("enter should act");
    };
    assert_eq!(req.kind, SheetKind::Reap);
    let pids: Vec<i32> = req.targets.iter().map(|t| t.pid).collect();
    assert_eq!(
        pids,
        [48213, 48201, 48190, 48311],
        "next dev before npm, its parent"
    );
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(a.toast.as_ref().unwrap().text, "Reaping 4 processes…");
    // A second confirmation while it runs is refused.
    press(&mut a, "X");
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::Redraw);
    assert!(a.toast.as_ref().unwrap().text.starts_with("Still reaping"));

    // 48311 ignored SIGTERM: the outcome is in the footer, and SIGKILL is offered apart.
    a.acted(
        &req,
        Ok(report(&[
            (48213, true),
            (48201, true),
            (48190, true),
            (48311, false),
        ])),
    );
    assert_eq!(
        a.toast.as_ref().unwrap().text,
        "Stopped 3, 1 still running."
    );
    assert_eq!(a.toast.as_ref().unwrap().tone, Tone::Warn);
    let Mode::Sheet(sheet) = &a.mode else {
        panic!("SIGKILL should be offered, got {:?}", a.mode);
    };
    assert_eq!(sheet.kind, SheetKind::Kill);
    assert_eq!(sheet.targets.len(), 1);
    assert_eq!(sheet.targets[0].pid, 48311);
    let text = screen(&render(&a, 120, 40));
    assert!(text.contains("Send SIGKILL to 1 process?"), "{text}");
    assert!(text.contains("Stopped 3, 1 still running."), "{text}");
    assert!(text.contains("⏎ send SIGKILL"), "{text}");

    // Declined: nothing more is sent, and X reaps afresh.
    press(&mut a, "");
    assert!(a.stubborn.is_none());
    assert_eq!(a.toast.as_ref().unwrap().text, "SIGKILL was not sent.");
    press(&mut a, "X");
    assert!(matches!(&a.mode, Mode::Sheet(s) if s.kind == SheetKind::Reap));
}

#[test]
fn sigkill_is_confirmed_on_its_own_and_follows_the_reap_checks() {
    let mut a = live();
    press(&mut a, "3X");
    let Action::Act(req) = a.on_key(key(KeyCode::Enter)) else {
        panic!("enter should act");
    };
    a.acted(&req, Ok(report(&[(48213, false)])));
    let Action::Act(kill) = a.on_key(key(KeyCode::Enter)) else {
        panic!("enter should send SIGKILL");
    };
    assert_eq!((kill.kind, kill.origin), (SheetKind::Kill, SheetKind::Reap));
    assert_eq!(kill.targets[0].pid, 48213);
    a.acted(&kill, Ok(report(&[(48213, true)])));
    assert_eq!(a.toast.as_ref().unwrap().text, "Stopped 1.");
    assert_eq!(a.mode, Mode::Normal, "no further sheet after SIGKILL");
}

#[test]
fn a_refused_reap_says_why_in_the_footer() {
    let mut a = live();
    press(&mut a, "3X");
    let Action::Act(req) = a.on_key(key(KeyCode::Enter)) else {
        panic!("enter should act");
    };
    // As the worker words it: short enough for the footer at 80 columns.
    let refusal = scry_core::control::Refusal::Disabled {
        config: "~/.config/scry/config.toml".into(),
        line: Some(2),
    };
    let why = format!("Nothing was sent: {}", refusal.brief());
    assert_eq!(
        why,
        "Nothing was sent: `destructive = false` in config.toml line 2."
    );
    a.acted(&req, Err(why.clone()));
    assert_eq!(a.toast.as_ref().unwrap().text, why);
    assert_eq!(a.toast.as_ref().unwrap().tone, Tone::Danger);
    assert_eq!(a.mode, Mode::Normal);
    let footer = lines(&render(&a, 80, 24)).pop().unwrap();
    assert!(footer.contains(&why), "{footer}");

    // A target refused at its own check says why once, and fits too.
    let mut report = report(&[(48213, false)]);
    report.done[0].outcome = None;
    report.done[0].refusal = Some(scry_core::control::Refusal::NotLeftover {
        pid: 48213,
        why: "It is on your keep list as `~/sources/acme/node_modules/next/dist/bin/next`.".into(),
    });
    a.acted(&req, Ok(report));
    let text = &a.toast.as_ref().unwrap().text;
    assert_eq!(
        text,
        "Stopped 0, 1 not signalled: pid 48213 is no longer a leftover."
    );
    let footer = lines(&render(&a, 80, 24)).pop().unwrap();
    assert!(footer.contains(text.as_str()), "{footer}");
}

#[test]
fn the_sigkill_sheet_shows_each_age_as_of_now() {
    let mut a = live();
    press(&mut a, "3X");
    let Action::Act(mut req) = a.on_key(key(KeyCode::Enter)) else {
        panic!("enter should act");
    };
    // Listed an hour ago as 2 minutes old: three hours old by now.
    let now_us = scry_core::sys::now_ms() * 1000;
    req.targets[0].start_us = Some(now_us - 3 * 3600 * 1_000_000);
    req.targets[0].age_s = 120;
    a.acted(&req, Ok(report(&[(req.targets[0].pid, false)])));
    let Mode::Sheet(sheet) = &a.mode else {
        panic!("SIGKILL should be offered");
    };
    assert_eq!(sheet.targets[0].age_s / 60, 180);
    let text = screen(&render(&a, 80, 24));
    assert!(text.contains(" 3h "), "{text}");
}

#[test]
fn the_keep_sheet_shows_the_whole_diff_wrapped_and_scrolls_when_it_cannot() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = scry_core::paths::Paths::new(tmp.path().into(), Some(tmp.path().join("scry")));
    std::fs::create_dir_all(tmp.path().join("scry")).unwrap();
    let long = format!(
        "~/sources/{}/node_modules/next/dist/bin/next",
        "a".repeat(90)
    );
    let mut board = keepable();
    if let Load::Ready(rows) = &mut board.agents {
        for r in rows.iter_mut().filter(|r| r.depth > 0) {
            r.keep = Some(scry_core::keep::KeepAs::Entry(long.clone()));
        }
    }
    std::fs::write(&paths.config, "keep = [\"postgres\"]\n").unwrap();
    let mut a = App::new(board.clone(), Source::Live, mono());
    a.paths = Some(paths.clone());
    press(&mut a, "3jjK");
    let text = screen(&render(&a, 80, 24));
    // Every character of the new line is on screen, across as many rows as it takes.
    let shown: String = text
        .lines()
        .filter(|l| l.contains("+ ") || l.trim_start().starts_with("aaa") || l.contains("/next"))
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join("");
    let want = format!("keep = [\"postgres\", \"{long}\"]");
    assert!(
        shown.replace(' ', "").contains(&want.replace(' ', "")),
        "{text}"
    );
    assert!(!text.contains("more"), "{text}");

    // A list of 40 entries, one per line, does not fit: the sheet scrolls through it.
    let many: String = (0..40).map(|i| format!("  \"p{i}\",\n")).collect();
    std::fs::write(&paths.config, format!("keep = [\n{many}]\n")).unwrap();
    let mut a = App::new(board, Source::Live, mono());
    a.paths = Some(paths);
    press(&mut a, "3jjK");
    let text = screen(&render(&a, 80, 24));
    assert!(text.contains("more   j k scroll"), "{text}");
    assert!(text.contains("- keep = ["), "{text}");
    press(&mut a, "G");
    let text = screen(&render(&a, 80, 24));
    assert!(text.contains("above"), "{text}");
    assert!(text.contains("aaaa"), "the new entry is reached: {text}");
}

#[test]
fn k_says_why_a_process_has_no_entry_in_one_line() {
    let mut board = keepable();
    if let Load::Ready(rows) = &mut board.agents {
        for r in rows.iter_mut().filter(|r| r.depth > 0) {
            r.keep = Some(scry_core::keep::KeepAs::Refused("runs code".into()));
        }
    }
    let mut a = App::new(board, Source::Live, mono());
    press(&mut a, "3jjK");
    assert_eq!(a.mode, Mode::Normal);
    let text = &a.toast.as_ref().unwrap().text;
    assert_eq!(
        text,
        "No entry keeps only `next dev`. Add one to keep by hand."
    );
    let footer = lines(&render(&a, 80, 24)).pop().unwrap();
    assert!(footer.contains(text.as_str()), "{footer}");
}

#[test]
fn x_in_overview_opens_the_same_reap_sheet() {
    let mut a = design();
    press(&mut a, "X");
    let Mode::Sheet(sheet) = &a.mode else {
        panic!("X should open the reap sheet on Overview, got {:?}", a.mode);
    };
    assert_eq!(sheet.kind, SheetKind::Reap);
    assert_eq!(sheet.targets.len(), 4);
    let footer = lines(&render(&a, 80, 24)).pop().unwrap();
    assert!(footer.contains("⏎ reap all"), "{footer}");
}

/// The design board with a keep entry on `next dev`, as a live reading has for every
/// process row.
fn keepable() -> Board {
    let mut board = design_board();
    let Load::Ready(rows) = &mut board.agents else {
        unreachable!();
    };
    for r in rows.iter_mut().filter(|r| r.depth > 0) {
        r.keep = Some(scry_core::keep::KeepAs::Entry(r.label.clone()));
    }
    board
}

#[test]
fn k_shows_the_change_and_writes_only_the_keep_list() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = scry_core::paths::Paths::new(tmp.path().into(), Some(tmp.path().join("scry")));
    std::fs::create_dir_all(tmp.path().join("scry")).unwrap();
    let config = "# mine\nkeep = [\"postgres\"]  # db\n\n[tui]\nrefresh_secs = 5\n";
    std::fs::write(&paths.config, config).unwrap();
    let mut a = App::new(keepable(), Source::Live, mono());
    a.paths = Some(paths.clone());
    press(&mut a, "3jjK");
    let Mode::Sheet(sheet) = &a.mode else {
        panic!("K should open a sheet, got {:?}", a.mode);
    };
    assert_eq!(sheet.kind, SheetKind::Keep);
    let text = screen(&render(&a, 80, 24));
    assert!(text.contains("Keep `next dev`?"), "{text}");
    assert!(text.contains(r#"- keep = ["postgres"]  # db"#), "{text}");
    assert!(
        text.contains(r#"+ keep = ["postgres", "next dev"]  # db"#),
        "{text}"
    );
    assert!(text.contains("⏎ write config"), "{text}");
    assert_eq!(
        std::fs::read_to_string(&paths.config).unwrap(),
        config,
        "not yet"
    );

    // A quiet reading follows, so "Refreshed." never replaces what was kept.
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::Reread);
    assert_eq!(
        std::fs::read_to_string(&paths.config).unwrap(),
        config.replace(r#"["postgres"]"#, r#"["postgres", "next dev"]"#)
    );
    super::finish_scan(&mut a, None);
    assert_eq!(
        a.toast.as_ref().unwrap().text,
        "Kept `next dev`: added to ~/scry/config.toml line 2."
    );
    // The edit was audited, before and after, though nothing was signalled.
    let rows = scry_core::store::Store::open(&paths.db())
        .unwrap()
        .audit_rows()
        .unwrap();
    let said: Vec<(&str, &str, &str)> = rows
        .iter()
        .map(|r| (r.actor.as_str(), r.action.as_str(), r.result.as_str()))
        .collect();
    assert_eq!(
        said,
        [
            ("tui", "keep", "adding at line 2"),
            ("tui", "keep", "added at line 2")
        ]
    );
    // The next reading spares it; asking again says so.
    let (c, _) = scry_core::config::Config::load(&paths).unwrap();
    assert_eq!(c.keep, ["postgres", "next dev"]);
    press(&mut a, "K");
    assert_eq!(a.mode, Mode::Normal);
    assert!(
        a.toast
            .as_ref()
            .unwrap()
            .text
            .contains("already on the keep list")
    );
}

#[test]
fn k_in_a_fixture_shows_the_change_and_writes_nothing() {
    let mut a = App::new(keepable(), Source::Fixture("d".into()), mono());
    press(&mut a, "3jjK");
    assert!(matches!(&a.mode, Mode::Sheet(s) if s.kind == SheetKind::Keep));
    press(
        &mut a, "
",
    );
    assert_eq!(
        a.toast.as_ref().unwrap().text,
        "This is a fixture, so nothing was written."
    );
    // A session row has nothing to keep.
    press(&mut a, "gK");
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn reap_sheet_after_confirm_offers_sigkill() {
    let mut a = live();
    press(&mut a, "3X");
    let Action::Act(req) = a.on_key(key(KeyCode::Enter)) else {
        panic!("enter should act");
    };
    a.acted(
        &req,
        Ok(report(&[
            (48213, true),
            (48201, true),
            (48190, true),
            (48311, false),
        ])),
    );
    insta::assert_snapshot!(render(&a, 120, 40));
}

#[test]
fn an_open_sigkill_sheet_keeps_its_targets_and_origin_whatever_finishes_meanwhile() {
    let mut a = live();
    press(&mut a, "3X");
    let Action::Act(reap) = a.on_key(key(KeyCode::Enter)) else {
        panic!("enter should act");
    };
    // A outlives SIGTERM while the help card is open, so its SIGKILL sheet waits for X.
    press(&mut a, "?");
    a.acted(&reap, Ok(report(&[(48213, false)])));
    assert_eq!(a.mode, Mode::Help);
    press(&mut a, "\x1b");
    // B is stopped from its own row meanwhile.
    press(&mut a, "gjx");
    let Action::Act(stop) = a.on_key(key(KeyCode::Enter)) else {
        panic!("enter should stop");
    };
    assert_ne!(stop.targets[0].pid, 48213);
    // While it runs, X opens A's SIGKILL sheet; B's outcome arrives and leaves it be.
    press(&mut a, "X");
    let Mode::Sheet(open) = a.mode.clone() else {
        panic!("X should offer SIGKILL, got {:?}", a.mode);
    };
    assert_eq!(open.kind, SheetKind::Kill);
    a.acted(&stop, Ok(report(&[(stop.targets[0].pid, true)])));
    assert_eq!(a.mode, Mode::Sheet(open), "the open sheet is unchanged");
    let Action::Act(kill) = a.on_key(key(KeyCode::Enter)) else {
        panic!("enter should send SIGKILL");
    };
    assert_eq!(
        (kill.kind, kill.origin),
        (SheetKind::Kill, SheetKind::Reap),
        "A is checked as a leftover, keep list included, before SIGKILL"
    );
    let pids: Vec<i32> = kill.targets.iter().map(|t| t.pid).collect();
    assert_eq!(pids, [48213]);
}
