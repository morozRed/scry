//! The terminal outside ratatui: the colour question at startup, answers that come after
//! Scry stopped waiting for them, and putting the terminal back from the signal thread.
//!
//! Scry asks only when the environment says the terminal answers at once (see
//! `theme::Env::may_ask`), and asks in raw mode, before the event loop reads a key, so the
//! question changes no terminal modes of its own. Terminals answer in order and every one
//! answers DA1, so the DA1 answer marks the end: everything before it is the answer or keys
//! typed meanwhile, which are handed back. Only when the wait ends before DA1 can the rest
//! of an answer still come; [`LateReply`] then picks it out of the keys for a short while.

use std::os::raw::c_char;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::theme::Rgb;

/// Foreground (OSC 10), background (OSC 11), then primary device attributes (DA1).
const QUERY: &[u8] = b"\x1b]10;?\x07\x1b]11;?\x07\x1b[c";
/// A terminal that answers at once does so in a few milliseconds.
const WAIT: Duration = Duration::from_secs(1);
const SLICE: Duration = Duration::from_millis(50);
/// How long after an unfinished answer the rest of it is still picked out of the keys.
const LATE_FOR: Duration = Duration::from_secs(2);
/// After that, keys held as a possible answer go through once no key came for this long.
const LATE_QUIET: Duration = Duration::from_millis(100);
const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;
/// Longest colour spec and DA1 parameter list that still count as an answer.
const SPEC_MAX: u8 = 48;

struct Tty {
    /// Scry's own open of the terminal crossterm uses, so making it non-blocking leaves the
    /// shell's descriptor alone.
    fd: libc::c_int,
    cooked: libc::termios,
}

static TTY: OnceLock<Tty> = OnceLock::new();
/// Held for every change of terminal modes, and by `restore_and_exit` until the exit, so no
/// mode change can land after the terminal was put back.
static MODES: Mutex<()> = Mutex::new(());

/// Remembers the terminal's modes before Scry changes any. Uses the terminal crossterm
/// uses: stdin when it is one, else /dev/tty.
pub fn save() {
    let mut name = [0 as c_char; 128];
    // SAFETY: isatty accepts any descriptor. ttyname_r writes at most `name.len()` bytes,
    // NUL-terminated when it returns 0, into a buffer that lives across the call.
    let named = unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1
            && libc::ttyname_r(libc::STDIN_FILENO, name.as_mut_ptr(), name.len()) == 0
    };
    let path = if named {
        name.as_ptr()
    } else {
        c"/dev/tty".as_ptr()
    };
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: `path` points to a NUL-terminated string that outlives the call. The
    // descriptor stays open for the life of the process.
    let fd = unsafe { libc::open(path, flags) };
    if fd < 0 {
        return;
    }
    // `select` cannot watch a descriptor this high, so Scry does without one.
    if fd >= libc::FD_SETSIZE as libc::c_int {
        // SAFETY: `fd` was opened above and is not used again.
        unsafe { libc::close(fd) };
        return;
    }
    // SAFETY: termios is plain data, so all zeroes is a valid value for tcgetattr to fill.
    let mut cooked: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open and `cooked` is a valid termios to write into.
    if unsafe { libc::tcgetattr(fd, &mut cooked) } == 0 {
        let _ = TTY.set(Tty { fd, cooked });
    }
}

/// Runs `f`, which changes terminal modes. Once `restore_and_exit` has started it waits
/// for the exit instead, so it cannot undo the restore.
pub fn change<T>(f: impl FnOnce() -> T) -> T {
    let _held = MODES.lock().unwrap_or_else(PoisonError::into_inner);
    f()
}

/// Puts back the saved modes, leaves the alternate screen, shows the cursor and exits.
/// Safe from the signal thread: plain syscalls, and on a hung-up terminal each fails at
/// once with EIO instead of blocking. A second caller waits here until the first exits.
pub fn restore_and_exit(code: i32) -> ! {
    std::mem::forget(MODES.lock().unwrap_or_else(PoisonError::into_inner));
    if let Some(t) = TTY.get() {
        // SAFETY: `t.fd` stays open for the life of the process, and `t.cooked` is the
        // termios tcgetattr filled in `save`.
        unsafe { libc::tcsetattr(t.fd, libc::TCSANOW, &t.cooked) };
    }
    let seq = b"\x1b[?1049l\x1b[?25h";
    // SAFETY: writes `seq.len()` bytes from a live buffer.
    unsafe { libc::write(libc::STDOUT_FILENO, seq.as_ptr().cast(), seq.len()) };
    std::process::exit(code)
}

/// What the terminal said to the colour question.
#[derive(Debug, Default)]
pub struct Answer {
    /// `(background, foreground)`.
    pub colors: Option<(Rgb, Rgb)>,
    /// Keys typed while Scry waited, in order.
    pub typed: Vec<KeyEvent>,
    /// Set when the wait ended before the DA1 answer, so the rest may still come.
    pub late: Option<LateReply>,
}

/// Asks the terminal for its default background and foreground. Call it in raw mode, before
/// anything else reads the terminal. Stops at the DA1 answer, after `WAIT`, or once
/// `signal` is set.
pub fn ask(signal: &AtomicI32) -> Answer {
    let Some(t) = TTY.get() else {
        return Answer::default();
    };
    let deadline = Instant::now() + WAIT;
    let stopped = || signal.load(Ordering::Relaxed) != 0;
    let sent = send(t.fd, QUERY, deadline, &stopped);
    if sent == 0 {
        return Answer::default();
    }
    let mut got = Vec::new();
    let mut chunk = [0u8; 256];
    let scan = loop {
        let scan = Scan::of(&got);
        let left = deadline.saturating_duration_since(Instant::now());
        if scan.done || left.is_zero() || stopped() {
            break scan;
        }
        match ready(t.fd, Dir::Read, left.min(SLICE)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) if retry(e.kind()) => continue,
            Err(_) => break scan,
        }
        // SAFETY: reads at most `chunk.len()` bytes into a live buffer.
        let n = unsafe { libc::read(t.fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        match n {
            n if n > 0 => got.extend_from_slice(&chunk[..n as usize]),
            0 => break Scan::of(&got),
            _ if retry(std::io::Error::last_os_error().kind()) => {}
            _ => break Scan::of(&got),
        }
    };
    let late = (!scan.done).then(|| {
        let (at, held) = scan.open.clone().unwrap_or_default();
        LateReply::new(at, keys_of(&held), Instant::now() + LATE_FOR)
    });
    Answer {
        colors: scan.bg.zip(scan.fg),
        typed: keys_of(&scan.typed),
        late,
    }
}

fn retry(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
    )
}

/// Writes as much of `bytes` as the terminal takes before `deadline`. Returns how much.
fn send(fd: libc::c_int, bytes: &[u8], deadline: Instant, stopped: &dyn Fn() -> bool) -> usize {
    let mut sent = 0;
    while sent < bytes.len() && !stopped() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let rest = &bytes[sent..];
        // SAFETY: writes at most `rest.len()` bytes from a live slice. The descriptor is
        // non-blocking, so a full output queue returns EAGAIN instead of waiting.
        let n = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if n > 0 {
            sent += n as usize;
            continue;
        }
        let kind = std::io::Error::last_os_error().kind();
        if !retry(kind) {
            break;
        }
        if kind == std::io::ErrorKind::WouldBlock {
            let _ = ready(fd, Dir::Write, left.min(SLICE));
        }
    }
    sent
}

enum Dir {
    Read,
    Write,
}

/// `select`, because macOS `poll` does not work on /dev/tty.
fn ready(fd: libc::c_int, dir: Dir, timeout: Duration) -> std::io::Result<bool> {
    // SAFETY: fd_set is plain data, so all zeroes is a valid empty set.
    let mut set: libc::fd_set = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a valid fd_set, and `fd` is below FD_SETSIZE, checked in `save`.
    unsafe { libc::FD_SET(fd, &mut set) };
    let mut tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    let (read, write) = match dir {
        Dir::Read => (&raw mut set, std::ptr::null_mut()),
        Dir::Write => (std::ptr::null_mut(), &raw mut set),
    };
    // SAFETY: the sets are live or null, and `tv` lives across the call.
    let n = unsafe { libc::select(fd + 1, read, write, std::ptr::null_mut(), &mut tv) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n > 0)
}

/// Where a byte stream stands in the shape of an answer to `QUERY`: `OSC 1x ; spec` ended
/// by BEL or ST, or `CSI ? params c`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Grammar {
    /// Between answers. `step` returns this once an answer is complete.
    #[default]
    Ground,
    Esc,
    /// `ESC ]` and this many bytes of `1x;`.
    Osc(u8),
    /// This many bytes of the colour spec.
    Spec(u8),
    /// ESC inside the spec, which only ST (`ESC \`) may follow.
    St,
    Csi,
    /// `CSI ?` and this many parameter bytes.
    Da1(u8),
}

impl Grammar {
    /// The state after `b`: `Ground` when `b` ends an answer, `None` when no answer has
    /// this shape.
    fn step(self, b: u8) -> Option<Grammar> {
        use Grammar::*;
        let spec = |c: u8| c.is_ascii_hexdigit() || b"rgba:/#".contains(&c);
        Some(match (self, b) {
            (Ground, ESC) => Esc,
            (Esc, b']') => Osc(0),
            (Esc, b'[') => Csi,
            (Osc(0), b'1') => Osc(1),
            (Osc(1), b'0' | b'1') => Osc(2),
            (Osc(2), b';') => Spec(0),
            (Spec(_), BEL) => Ground,
            (Spec(_), ESC) => St,
            (Spec(n), c) if n < SPEC_MAX && spec(c) => Spec(n + 1),
            (St, b'\\') => Ground,
            (Csi, b'?') => Da1(0),
            (Da1(_), b'c') => Ground,
            (Da1(n), c) if n < SPEC_MAX && (c.is_ascii_digit() || c == b';') => Da1(n + 1),
            _ => return None,
        })
    }
}

/// What the terminal sent while Scry waited: the answers, and every other byte as typed.
#[derive(Debug, Default, PartialEq)]
struct Scan {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    /// The DA1 answer arrived, so nothing more is coming.
    done: bool,
    typed: Vec<u8>,
    /// An answer cut off at the end: where it stands, and its bytes so far.
    open: Option<(Grammar, Vec<u8>)>,
}

impl Scan {
    fn of(bytes: &[u8]) -> Scan {
        let mut s = Scan::default();
        let (mut at, mut start, mut i) = (Grammar::Ground, 0, 0);
        while i < bytes.len() {
            if s.done {
                s.typed.extend_from_slice(&bytes[i..]);
                return s;
            }
            match at.step(bytes[i]) {
                Some(Grammar::Ground) => {
                    s.answer(&bytes[start..=i]);
                    i += 1;
                    start = i;
                    at = Grammar::Ground;
                }
                Some(next) => {
                    at = next;
                    i += 1;
                }
                // Not an answer after all: its first byte was typed, and the rest is read
                // again, since it may hold the start of a real answer.
                None => {
                    s.typed.push(bytes[start]);
                    start += 1;
                    i = start;
                    at = Grammar::Ground;
                }
            }
        }
        if at != Grammar::Ground {
            s.open = Some((at, bytes[start..].to_vec()));
        }
        s
    }

    fn answer(&mut self, a: &[u8]) {
        if a[1] == b'[' {
            self.done = true;
            return;
        }
        // ESC ] 1 x ; spec, then BEL or ESC \.
        let end = a.len() - if a.ends_with(&[BEL]) { 1 } else { 2 };
        let color = std::str::from_utf8(&a[5..end]).ok().and_then(parse_color);
        match a[3] {
            b'0' => self.fg = color,
            _ => self.bg = color,
        }
    }
}

/// Picks the rest of an answer out of the keys after Scry stopped waiting for it.
/// Crossterm reads `ESC ] 11;rgb:…` as alt-], then a key per character, then ctrl-g (BEL)
/// or alt-\ (ST); an ESC that arrives in a read of its own comes as Esc. Keys that fit that
/// shape are held until the answer ends, and go through after all when it breaks.
#[derive(Debug)]
pub struct LateReply {
    /// Until when an answer may still start.
    until: Instant,
    at: Grammar,
    held: Vec<KeyEvent>,
    last: Instant,
}

impl LateReply {
    fn new(at: Grammar, held: Vec<KeyEvent>, until: Instant) -> LateReply {
        LateReply {
            until,
            at,
            held,
            last: Instant::now(),
        }
    }

    /// The keys to handle now, in order: none while `key` may be part of an answer, else
    /// any held keys that turned out not to be, then `key`.
    pub fn feed(&mut self, key: KeyEvent, now: Instant) -> Vec<KeyEvent> {
        self.last = now;
        if self.at == Grammar::Ground && now >= self.until {
            return vec![key];
        }
        let Some(bytes) = bytes_of(&key) else {
            return self.release(key, now);
        };
        let mut at = self.at;
        for b in bytes {
            match at.step(b) {
                Some(next) => at = next,
                None => return self.release(key, now),
            }
        }
        self.at = at;
        if at == Grammar::Ground {
            self.held.clear();
        } else {
            self.held.push(key);
        }
        Vec::new()
    }

    fn release(&mut self, key: KeyEvent, now: Instant) -> Vec<KeyEvent> {
        if self.held.is_empty() && self.at == Grammar::Ground {
            return vec![key];
        }
        let mut out = std::mem::take(&mut self.held);
        self.at = Grammar::Ground;
        out.extend(self.feed(key, now));
        out
    }

    /// Once the window has passed and no key came for a moment: the keys still held, which
    /// were typed after all. The guard is done then.
    pub fn expire(&mut self, now: Instant) -> Option<Vec<KeyEvent>> {
        if now < self.until || now < self.last + LATE_QUIET {
            return None;
        }
        self.at = Grammar::Ground;
        Some(std::mem::take(&mut self.held))
    }
}

/// The bytes crossterm read to make `key`, for the keys an answer can turn into.
fn bytes_of(key: &KeyEvent) -> Option<Vec<u8>> {
    let mods = key.modifiers - KeyModifiers::SHIFT;
    let mut buf = [0u8; 4];
    match key.code {
        KeyCode::Esc if mods.is_empty() => Some(vec![ESC]),
        KeyCode::Char('g') if mods == KeyModifiers::CONTROL => Some(vec![BEL]),
        KeyCode::Char(c) if mods.is_empty() => Some(c.encode_utf8(&mut buf).as_bytes().to_vec()),
        KeyCode::Char(c) if mods == KeyModifiers::ALT => {
            Some([&[ESC], c.encode_utf8(&mut buf).as_bytes()].concat())
        }
        _ => None,
    }
}

/// The keys crossterm reads from `bytes`, for what a person types while Scry waits:
/// characters, enter, tab, backspace, esc, ctrl and alt chords and the arrow keys. Other
/// sequences are dropped.
fn keys_of(bytes: &[u8]) -> Vec<KeyEvent> {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    let mut out = Vec::new();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(key_of(c));
            continue;
        }
        let key = match chars.next() {
            None | Some('\x1b') => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Some('[') => {
                let end = chars.by_ref().find(|c| ('@'..='~').contains(c));
                let code = match end {
                    Some('A') => KeyCode::Up,
                    Some('B') => KeyCode::Down,
                    Some('C') => KeyCode::Right,
                    Some('D') => KeyCode::Left,
                    _ => continue,
                };
                KeyEvent::new(code, KeyModifiers::NONE)
            }
            Some(c) => {
                let mut key = key_of(c);
                key.modifiers |= KeyModifiers::ALT;
                key
            }
        };
        out.push(key);
    }
    out
}

fn key_of(c: char) -> KeyEvent {
    let ctrl = |c: u8| KeyEvent::new(KeyCode::Char(c as char), KeyModifiers::CONTROL);
    match c {
        '\r' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        '\t' => KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        '\x7f' => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        '\0' => ctrl(b' '),
        '\x01'..='\x1a' => ctrl(c as u8 - 1 + b'a'),
        '\x1c'..='\x1f' => ctrl(c as u8 - 0x1c + b'4'),
        c if c.is_uppercase() => KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT),
        c => KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
    }
}

/// `rgb:r/g/b` with 1 to 4 hex digits per channel (what terminals send), `rgba:r/g/b/a`,
/// or `#rgb` with 1 to 4 digits per channel.
pub(crate) fn parse_color(spec: &str) -> Option<Rgb> {
    let channel = |h: &str| -> Option<u8> {
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len())) - 1;
        Some(((v * 255 + max / 2) / max) as u8)
    };
    let parts: Vec<&str> = if let Some(rest) = spec.strip_prefix("rgb:") {
        rest.split('/').collect()
    } else if let Some(rest) = spec.strip_prefix("rgba:") {
        rest.split('/').take(3).collect()
    } else {
        let hex = spec.strip_prefix('#')?;
        if hex.is_empty() || hex.len() % 3 != 0 || hex.len() > 12 || !hex.is_ascii() {
            return None;
        }
        let n = hex.len() / 3;
        vec![&hex[..n], &hex[n..2 * n], &hex[2 * n..]]
    };
    match parts[..] {
        [r, g, b] => Some(Rgb(channel(r)?, channel(g)?, channel(b)?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FG: &[u8] = b"\x1b]10;rgb:c5c5/c8c8/c6c6\x1b\\";
    const BG: &[u8] = b"\x1b]11;rgb:1d1d/1f1f/2121\x07";
    const DA1: &[u8] = b"\x1b[?62;22c";

    /// The keys crossterm makes of `bytes` when they arrive in one read.
    fn read(bytes: &[u8]) -> Vec<KeyEvent> {
        keys_of(bytes)
    }

    fn feed_all(late: &mut LateReply, keys: &[KeyEvent], now: Instant) -> Vec<KeyEvent> {
        keys.iter().flat_map(|&k| late.feed(k, now)).collect()
    }

    fn chars(keys: &[KeyEvent]) -> String {
        keys.iter()
            .map(|k| match k.code {
                KeyCode::Char(c) => c,
                KeyCode::Esc => '⎋',
                _ => '?',
            })
            .collect()
    }

    fn guard() -> (LateReply, Instant) {
        let now = Instant::now();
        (
            LateReply::new(Grammar::Ground, Vec::new(), now + LATE_FOR),
            now,
        )
    }

    #[test]
    fn a_whole_answer_gives_the_colours_and_ends_at_da1() {
        let s = Scan::of(&[FG, BG, DA1].concat());
        assert_eq!(s.fg, Some(Rgb(0xc5, 0xc8, 0xc6)));
        assert_eq!(s.bg, Some(Rgb(0x1d, 0x1f, 0x21)));
        assert!(s.done && s.typed.is_empty() && s.open.is_none());
        // A terminal without the colour query answers DA1 alone.
        let s = Scan::of(DA1);
        assert!(s.done && s.fg.is_none() && s.bg.is_none());
    }

    #[test]
    fn keys_typed_around_the_answer_are_kept_in_order() {
        let s = Scan::of(&[b"3".as_slice(), FG, b"/x", BG, DA1, b"j\x1b"].concat());
        assert!(s.done);
        assert_eq!(s.bg, Some(Rgb(0x1d, 0x1f, 0x21)));
        assert_eq!(chars(&keys_of(&s.typed)), "3/xj⎋");
        // An escape that starts no answer is a key too.
        let s = Scan::of(b"\x1bj\x1b]1x");
        assert_eq!(s.typed, b"\x1bj\x1b]1x");
        assert_eq!(keys_of(b"\x1bj")[0].modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn an_answer_cut_at_the_deadline_keeps_its_place() {
        let s = Scan::of(&[FG, &BG[..12]].concat());
        assert!(!s.done);
        assert_eq!(s.fg, Some(Rgb(0xc5, 0xc8, 0xc6)));
        assert_eq!(s.open, Some((Grammar::Spec(7), BG[..12].to_vec())));
        assert!(s.typed.is_empty());
    }

    #[test]
    fn a_late_answer_is_swallowed_and_the_next_key_is_not() {
        let (mut late, now) = guard();
        let answer = [FG, BG].concat();
        assert!(feed_all(&mut late, &read(&answer), now).is_empty());
        assert_eq!(chars(&late.feed(read(b"q")[0], now)), "q");
    }

    #[test]
    fn an_answer_whose_escape_came_alone_is_swallowed() {
        let (mut late, now) = guard();
        // The ESC of the answer and of its ST each arrive in a read of their own, then DA1
        // also loses its ESC.
        let mut keys = read(b"\x1b");
        keys.extend(read(b"]11;rgb:1d1d/1f1f/2121"));
        keys.extend(read(b"\x1b"));
        keys.extend(read(b"\\"));
        keys.extend(read(b"\x1b"));
        keys.extend(read(b"[?62;22c"));
        assert!(feed_all(&mut late, &keys, now).is_empty());
        assert_eq!(chars(&late.feed(read(b"j")[0], now)), "j");
    }

    #[test]
    fn the_rest_of_an_answer_cut_at_the_deadline_is_swallowed() {
        let s = Scan::of(&BG[..12]);
        let (at, held) = s.open.unwrap();
        let now = Instant::now();
        let mut late = LateReply::new(at, keys_of(&held), now + LATE_FOR);
        assert!(feed_all(&mut late, &read(&[&BG[12..], DA1].concat()), now).is_empty());
        assert_eq!(chars(&late.feed(read(b"/")[0], now)), "/");
    }

    #[test]
    fn keys_that_only_start_like_an_answer_go_through_in_order() {
        let (mut late, now) = guard();
        // Esc, then alt-], then 1 and j: none of it was an answer.
        let keys = [read(b"\x1b"), read(b"\x1b]"), read(b"1j")].concat();
        let out = feed_all(&mut late, &keys, now);
        assert_eq!(out.len(), 4, "{out:?}");
        assert_eq!(out[0].code, KeyCode::Esc);
        assert_eq!(
            (out[1].code, out[1].modifiers),
            (KeyCode::Char(']'), KeyModifiers::ALT)
        );
        assert_eq!(chars(&out[2..]), "1j");
    }

    #[test]
    fn held_keys_go_through_once_the_window_has_passed() {
        let (mut late, now) = guard();
        assert!(late.feed(read(b"\x1b")[0], now).is_empty());
        assert_eq!(late.expire(now), None);
        let after = now + LATE_FOR + LATE_QUIET;
        assert_eq!(chars(&late.expire(after).unwrap()), "⎋");
        // After the window an answer can no longer start.
        assert_eq!(chars(&late.feed(read(b"\x1b")[0], after)), "⎋");
    }

    #[test]
    fn typed_bytes_become_the_keys_crossterm_reads() {
        let keys = keys_of(b"a\r\t\x7f\x03G\x1b[A\x1b[1;5C\x1bx");
        let codes: Vec<(KeyCode, KeyModifiers)> =
            keys.iter().map(|k| (k.code, k.modifiers)).collect();
        assert_eq!(
            codes,
            [
                (KeyCode::Char('a'), KeyModifiers::NONE),
                (KeyCode::Enter, KeyModifiers::NONE),
                (KeyCode::Tab, KeyModifiers::NONE),
                (KeyCode::Backspace, KeyModifiers::NONE),
                (KeyCode::Char('c'), KeyModifiers::CONTROL),
                (KeyCode::Char('G'), KeyModifiers::SHIFT),
                (KeyCode::Up, KeyModifiers::NONE),
                (KeyCode::Right, KeyModifiers::NONE),
                (KeyCode::Char('x'), KeyModifiers::ALT),
            ]
        );
    }

    #[test]
    fn colour_specs_scale_to_eight_bits() {
        assert_eq!(parse_color("rgb:ffff/0000/8080"), Some(Rgb(255, 0, 128)));
        assert_eq!(parse_color("rgb:f/0/8"), Some(Rgb(255, 0, 136)));
        assert_eq!(parse_color("rgba:fd/f6/e3/ff"), Some(Rgb(0xfd, 0xf6, 0xe3)));
        assert_eq!(parse_color("#fdf6e3"), Some(Rgb(0xfd, 0xf6, 0xe3)));
        assert_eq!(parse_color("rgb:12345/0/0"), None);
        assert_eq!(parse_color("cmyk:0/0/0/0"), None);
    }
}
