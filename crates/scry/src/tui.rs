use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use scry_core::Snapshot;
use scry_core::view::{Overview, Tab};

const MIN_W: u16 = 80;
const MIN_H: u16 = 24;
const SAMPLE_EVERY: Duration = Duration::from_secs(2);
const INPUT_POLL: Duration = Duration::from_millis(250);

pub enum Source {
    Live,
    Fixture(String),
}

pub struct App {
    pub tab: Tab,
    pub help: bool,
    pub snapshot: Snapshot,
    pub source: Source,
    theme: Theme,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Quit,
    Suspend,
}

struct Theme {
    accent: Style,
    dim: Style,
}

impl Theme {
    fn from_env() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Theme {
            accent: if no_color {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            },
            dim: Style::new().add_modifier(Modifier::DIM),
        }
    }
}

impl App {
    pub fn new(snapshot: Snapshot, source: Source) -> Self {
        App {
            tab: Tab::Overview,
            help: false,
            snapshot,
            source,
            theme: Theme::from_env(),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => Action::Quit,
            KeyCode::Char('z') if ctrl => Action::Suspend,
            KeyCode::Esc if self.help => {
                self.help = false;
                Action::Redraw
            }
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('?') => {
                self.help = !self.help;
                Action::Redraw
            }
            KeyCode::Char(c @ '1'..='5') => {
                self.tab = Tab::ALL[(c as u8 - b'1') as usize];
                Action::Redraw
            }
            KeyCode::Tab => self.step_tab(1),
            KeyCode::BackTab => self.step_tab(Tab::ALL.len() - 1),
            _ => Action::None,
        }
    }

    fn step_tab(&mut self, by: usize) -> Action {
        let i = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
        self.tab = Tab::ALL[(i + by) % Tab::ALL.len()];
        Action::Redraw
    }
}

pub fn run(fixture: Option<PathBuf>) -> Result<()> {
    let source = match &fixture {
        Some(p) => Source::Fixture(p.file_name().map_or_else(
            || p.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )),
        None => Source::Live,
    };
    let live = fixture.is_none();
    let mut app = App::new(crate::load(fixture)?, source);

    let stop = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
    ] {
        signal_hook::flag::register(sig, Arc::clone(&stop))?;
    }

    let (tx, rx) = mpsc::channel();
    if live {
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(SAMPLE_EVERY);
                let Ok(snap) = scry_core::sys::scan() else {
                    continue;
                };
                if tx.send(snap).is_err() {
                    break;
                }
            }
        });
    }

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &rx, &stop);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mpsc::Receiver<Snapshot>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut dirty = true;
    while !stop.load(Ordering::Relaxed) {
        if let Some(snap) = rx.try_iter().last() {
            app.snapshot = snap;
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| draw(f, app))?;
            dirty = false;
        }
        if !event::poll(INPUT_POLL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => match app.on_key(key) {
                Action::Quit => break,
                Action::Suspend => {
                    suspend(terminal)?;
                    dirty = true;
                }
                Action::Redraw => dirty = true,
                Action::None => {}
            },
            Event::Resize(..) => dirty = true,
            _ => {}
        }
    }
    Ok(())
}

/// Ctrl-Z: give the shell its terminal back, stop, and take it again on `fg`.
fn suspend(terminal: &mut DefaultTerminal) -> Result<()> {
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen)?;
    unsafe { libc::raise(libc::SIGTSTP) };
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        let msg = format!(
            "Terminal too small. Scry needs {MIN_W}×{MIN_H}, this one is {}×{}.",
            area.width, area.height
        );
        f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
        return;
    }
    let [header, tabs, rule_top, body, rule_bottom, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let overview = Overview::from_snapshot(&app.snapshot);
    draw_header(f, header, app, &overview);
    draw_tabs(f, tabs, app);
    let rule = "─".repeat(area.width as usize);
    f.render_widget(Paragraph::new(rule.as_str()).style(app.theme.dim), rule_top);
    f.render_widget(
        Paragraph::new(rule.as_str()).style(app.theme.dim),
        rule_bottom,
    );
    draw_body(f, body, app, &overview);
    f.render_widget(
        Paragraph::new(" 1-5 views   tab next   ? help   q quit").style(app.theme.dim),
        footer,
    );
    if app.help {
        draw_help(f, body, app);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App, o: &Overview) {
    let source = match &app.source {
        Source::Live => "live".to_string(),
        Source::Fixture(name) => format!("fixture {name}"),
    };
    let [left, right] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(source.len() as u16 + 1),
    ])
    .areas(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" scry", app.theme.accent),
            Span::raw("   "),
            Span::raw(o.counts_line()),
        ])),
        left,
    );
    f.render_widget(Paragraph::new(source).style(app.theme.dim), right);
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::raw(" ")];
    for (i, tab) in Tab::ALL.iter().enumerate() {
        let label = format!("{} {}", i + 1, tab.title());
        if *tab == app.tab {
            spans.push(Span::styled(format!("[{label}]"), app.theme.accent));
        } else {
            spans.push(Span::raw(format!(" {label} ")));
        }
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(f: &mut Frame, area: Rect, app: &App, o: &Overview) {
    let lines = match app.tab {
        Tab::Overview => vec![
            Line::raw(""),
            Line::styled(
                format!("  {}", o.headline()),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Line::styled("  Port and agent checks are not built yet.", app.theme.dim),
        ],
        other => vec![
            Line::raw(""),
            Line::styled(
                format!("  {} is not built yet.", other.title()),
                app.theme.dim,
            ),
        ],
    };
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_help(f: &mut Frame, body: Rect, app: &App) {
    let rows = [
        ("1-5", "switch view"),
        ("tab / shift-tab", "next / previous view"),
        ("?", "close this help"),
        ("ctrl-z", "suspend to the shell"),
        ("q / esc", "quit"),
    ];
    let w = 44.min(body.width);
    let h = (rows.len() as u16 + 2).min(body.height);
    let area = Rect {
        x: body.x + (body.width - w) / 2,
        y: body.y + (body.height - h) / 2,
        width: w,
        height: h,
    };
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!(" {k:<16}"), app.theme.accent),
                Span::raw(*d),
            ])
        })
        .collect();
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEvent;

    fn app() -> App {
        let snap =
            scry_core::fixture::parse(include_str!("../../../fixtures/typical.json")).unwrap();
        let mut app = App::new(snap, Source::Fixture("typical.json".into()));
        app.theme = Theme {
            accent: Style::new(),
            dim: Style::new(),
        };
        app
    }

    fn render(app: &App, w: u16, h: u16) -> TestBackend {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        t.backend().clone()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn overview_80x24() {
        insta::assert_snapshot!(render(&app(), 80, 24));
    }

    #[test]
    fn overview_120x40() {
        insta::assert_snapshot!(render(&app(), 120, 40));
    }

    #[test]
    fn help_overlay() {
        let mut a = app();
        assert_eq!(a.on_key(key(KeyCode::Char('?'))), Action::Redraw);
        insta::assert_snapshot!(render(&a, 80, 24));
    }

    #[test]
    fn too_small() {
        insta::assert_snapshot!(render(&app(), 60, 10));
    }

    #[test]
    fn keys_switch_views_and_esc_closes_help_before_quitting() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('3')));
        assert_eq!(a.tab, Tab::Agents);
        a.on_key(key(KeyCode::Tab));
        assert_eq!(a.tab, Tab::Disk);
        a.on_key(key(KeyCode::BackTab));
        a.on_key(key(KeyCode::BackTab));
        assert_eq!(a.tab, Tab::Ports);
        a.on_key(key(KeyCode::Char('?')));
        assert_eq!(a.on_key(key(KeyCode::Esc)), Action::Redraw);
        assert!(!a.help);
        assert_eq!(a.on_key(key(KeyCode::Esc)), Action::Quit);
        let ctrl_z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(a.on_key(ctrl_z), Action::Suspend);
    }
}
