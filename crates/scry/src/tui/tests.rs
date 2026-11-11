//! Screen snapshots from fixtures and the key-handling state machine.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use scry_core::view::{Board, Load, Tab};

use super::app::{Action, App, Mode, SheetKind, Source};
use super::theme::{Colors, Look, Theme};
use super::ui::draw;

fn mono() -> Theme {
    Theme::new(Look::Tinted, Colors::Mono)
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

fn design() -> App {
    let board = Board::parse(include_str!("../../../../fixtures/design.json")).unwrap();
    App::new(board, Source::Fixture("design.json".into()), mono())
}

fn render(app: &App, w: u16, h: u16) -> TestBackend {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| {
        draw(f, app);
    })
    .unwrap();
    t.backend().clone()
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
fn keys_switch_views_and_esc_closes_help_before_quitting() {
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
    assert_eq!(a.on_key(key(KeyCode::Esc)), Action::Quit);
    let ctrl_z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(a.on_key(ctrl_z), Action::Suspend);
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
fn a_sheet_waits_for_enter_and_esc_sends_nothing() {
    let mut a = design();
    press(&mut a, "3jjx");
    assert!(matches!(&a.mode, Mode::Sheet(s) if s.targets[0].pid == 48213));
    press(&mut a, "\x1b");
    assert_eq!(a.mode, Mode::Normal);
    assert!(a.toast.is_none());
    press(&mut a, "x\n");
    assert_eq!(a.mode, Mode::Normal);
    assert!(
        a.toast
            .as_ref()
            .unwrap()
            .text
            .starts_with("Nothing was sent")
    );
}

#[test]
fn sort_cycles_columns_then_returns_to_the_natural_order() {
    let mut a = design();
    press(&mut a, "3s");
    assert_eq!(
        a.state().sort.map(|s| (s.column, s.descending)),
        Some((0, false))
    );
    press(&mut a, "s");
    assert_eq!(
        a.state().sort.map(|s| (s.column, s.descending)),
        Some((1, true))
    );
    press(&mut a, "ssssss");
    assert_eq!(a.state().sort, None);
}

#[test]
fn a_rescan_reports_changed_rows() {
    let mut a = design();
    press(&mut a, "3");
    let mut board = a.board.clone();
    if let Load::Ready(rows) = &mut board.agents {
        rows[2].ram = Some(1);
    }
    assert_eq!(a.replace_board(board), ["next-48213"]);
}
