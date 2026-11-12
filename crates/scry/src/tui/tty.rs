//! The terminal outside ratatui: the modes Scry found it in and putting them back, from the
//! signal thread too, the colour question at startup, and answers or keys that arrive after
//! Scry stopped reading for them.
//!
//! Scry asks only when the environment says the terminal answers at once (see
//! `theme::Env::may_ask`), and asks in raw mode, before the event loop reads a key, so the
//! question changes no terminal modes of its own. Terminals answer in order and every one
//! answers DA1, so the DA1 answer marks the end: everything before it is the answer or keys
//! typed meanwhile, which are handed back. Only when the wait ends before DA1 can the rest
//! of an answer still come; [`LateReply`] then picks it out of the keys for a short while.
//! It also finishes a key whose first bytes Scry read and whose rest crossterm reads.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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
/// How long the rest of a key is still read after the reads stopped inside it. A terminal
/// writes a key in one go, so the rest is normally waiting already.
const SPLIT: Duration = Duration::from_millis(100);
/// How long after an unfinished answer the rest of it is still picked out of the keys.
const LATE_FOR: Duration = Duration::from_secs(2);
/// After that, keys held as a possible answer go through once no key came for this long.
const LATE_QUIET: Duration = Duration::from_millis(100);
const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;
/// Longest colour spec and DA1 parameter list that still count as an answer.
const SPEC_MAX: u8 = 48;
/// Leaves the alternate screen and shows the cursor.
const LEAVE: &[u8] = b"\x1b[?1049l\x1b[?25h";

/// The terminal crossterm reads, and its modes before Scry changed any.
struct Saved {
    /// Scry's own non-blocking open of that terminal, for the colour question, so the
    /// shell's descriptor stays blocking. `None` when it could not be opened, as for a
    /// terminal owned by another user; stdin is then that terminal, and the modes go back
    /// through it.
    fd: Option<OwnedFd>,
    cooked: libc::termios,
}

impl Saved {
    /// Reads the modes through `fd`, or through stdin without one. `None`, with `fd`
    /// closed, when that is not a terminal.
    fn of(fd: Option<OwnedFd>) -> Option<Saved> {
        let raw = fd.as_ref().map_or(libc::STDIN_FILENO, AsRawFd::as_raw_fd);
        // SAFETY: termios is plain data, so all zeroes is a valid value for tcgetattr to fill.
        let mut cooked: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `raw` is open, and `cooked` is a valid termios to write into.
        if unsafe { libc::tcgetattr(raw, &mut cooked) } != 0 {
            return None;
        }
        Some(Saved { fd, cooked })
    }

    fn raw_fd(&self) -> RawFd {
        self.fd
            .as_ref()
            .map_or(libc::STDIN_FILENO, AsRawFd::as_raw_fd)
    }

    /// Sets the saved modes again. TCSANOW does not wait for output to drain.
    fn put_back(&self) {
        // SAFETY: the descriptor is open while `self` lives, and `self.cooked` is the
        // termios tcgetattr filled.
        unsafe { libc::tcsetattr(self.raw_fd(), libc::TCSANOW, &self.cooked) };
    }
}

static SAVED: OnceLock<Saved> = OnceLock::new();
/// Scry's own non-blocking open of the terminal stdout is, so leaving the alternate screen
/// from the signal thread cannot block on a terminal that stopped reading.
static SCREEN: OnceLock<OwnedFd> = OnceLock::new();
/// Held while raw mode goes on, and by `restore_and_exit` until the exit, so raw mode cannot
/// come back after the terminal was put back. Only mode changes take it, never a write: a
/// write blocks while the terminal is not reading, and the signal thread must always get
/// the lock.
static MODES: Mutex<()> = Mutex::new(());

/// Remembers the modes of the terminal crossterm uses (stdin when it is one, else /dev/tty)
/// before Scry changes any, and opens the terminal stdout is, for `restore_and_exit`.
pub fn save() {
    let input = name_of(libc::STDIN_FILENO);
    let path = input.as_deref().unwrap_or(c"/dev/tty");
    if let Some(saved) = Saved::of(open(path, libc::O_RDWR)) {
        // A second save keeps the first and closes its own descriptor.
        let _ = SAVED.set(saved);
    }
    if let Some(screen) = name_of(libc::STDOUT_FILENO).and_then(|name| open(&name, libc::O_WRONLY))
    {
        let _ = SCREEN.set(screen);
    }
}

/// The path of the terminal `fd` is, if it is one.
fn name_of(fd: RawFd) -> Option<CString> {
    let mut name = [0 as c_char; 128];
    // SAFETY: isatty accepts any descriptor. ttyname_r writes at most `name.len()` bytes,
    // NUL-terminated when it returns 0, into a buffer that lives across the call.
    let named =
        unsafe { libc::isatty(fd) == 1 && libc::ttyname_r(fd, name.as_mut_ptr(), name.len()) == 0 };
    // SAFETY: ttyname_r returned 0, so `name` holds a NUL-terminated string.
    named.then(|| unsafe { CStr::from_ptr(name.as_ptr()) }.to_owned())
}

/// Opens the terminal at `path` non-blocking. The descriptor is owned, so it is closed on
/// every path that does not keep it.
fn open(path: &CStr, access: libc::c_int) -> Option<OwnedFd> {
    let flags = access | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    // SAFETY: open returned a new descriptor that nothing else owns.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Runs `f`, which puts the terminal in raw mode. Once `restore_and_exit` has started it
/// waits for the exit instead, so raw mode cannot come back after the restore. Leaving raw
/// mode needs no lock: landing after the restore changes nothing.
pub fn change<T>(f: impl FnOnce() -> T) -> T {
    let _held = MODES.lock().unwrap_or_else(PoisonError::into_inner);
    f()
}

/// Puts back the saved modes, then leaves the alternate screen and shows the cursor if the
/// terminal takes those bytes at once, and exits. Nothing here waits on the terminal, so one
/// that stopped reading or hung up cannot keep Scry from exiting. A second caller waits here
/// until the first exits.
pub fn restore_and_exit(code: i32) -> ! {
    std::mem::forget(MODES.lock().unwrap_or_else(PoisonError::into_inner));
    if let Some(saved) = SAVED.get() {
        saved.put_back();
    }
    if let Some(screen) = SCREEN.get() {
        // SAFETY: writes `LEAVE.len()` bytes from a static buffer to a descriptor that stays
        // open for the life of the process. It is non-blocking, so a full output queue
        // fails with EAGAIN instead of waiting.
        unsafe { libc::write(screen.as_raw_fd(), LEAVE.as_ptr().cast(), LEAVE.len()) };
    }
    // SAFETY: _exit ends the process and runs nothing else. `process::exit` would first
    // flush Rust's stdout buffer, a write that can block on the same full queue.
    unsafe { libc::_exit(code) }
}

/// What the terminal said to the colour question.
#[derive(Debug, Default)]
pub struct Answer {
    /// `(background, foreground)`.
    pub colors: Option<(Rgb, Rgb)>,
    /// Keys typed while Scry waited, in order.
    pub typed: Vec<KeyEvent>,
    /// Set when the wait ended before the DA1 answer, so the rest may still come, or when
    /// the reads stopped inside a key.
    pub late: Option<LateReply>,
}

/// Asks the terminal for its default background and foreground. Call it in raw mode, before
/// anything else reads the terminal. Stops at the DA1 answer, after `WAIT`, or once
/// `signal` is set.
pub fn ask(signal: &AtomicI32) -> Answer {
    let Some(fd) = SAVED
        .get()
        .and_then(|s| s.fd.as_ref())
        .map(AsRawFd::as_raw_fd)
    else {
        return Answer::default();
    };
    let deadline = Instant::now() + WAIT;
    let stopped = || signal.load(Ordering::Relaxed) != 0;
    if send(fd, QUERY, deadline, &stopped) == 0 {
        return Answer::default();
    }
    let mut got = Vec::new();
    read_until(fd, &mut got, deadline, &stopped, |scan| scan.done);
    // A terminal writes a key in one go, so when the reads stopped inside one its rest is
    // on the way. It is read here because crossterm drops the rest of a character whose
    // first byte it did not see.
    let scan = read_until(fd, &mut got, Instant::now() + SPLIT, &stopped, |scan| {
        scan.cut().is_empty()
    });
    Answer {
        colors: scan.bg.zip(scan.fg),
        typed: keys_of(&scan.typed).0,
        late: LateReply::after(&scan, Instant::now()),
    }
}

/// Reads from `fd` into `got` until its scan is `enough`, `until` passes, the terminal fails
/// or `stopped` says so. Returns the last scan.
fn read_until(
    fd: RawFd,
    got: &mut Vec<u8>,
    until: Instant,
    stopped: &dyn Fn() -> bool,
    enough: impl Fn(&Scan) -> bool,
) -> Scan {
    let mut chunk = [0u8; 256];
    loop {
        let scan = Scan::of(got);
        let left = until.saturating_duration_since(Instant::now());
        if enough(&scan) || left.is_zero() || stopped() {
            return scan;
        }
        match ready(fd, Dir::Read, left.min(SLICE)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) if retry(e.kind()) => continue,
            Err(_) => return scan,
        }
        // SAFETY: reads at most `chunk.len()` bytes into a live buffer.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        match n {
            n if n > 0 => got.extend_from_slice(&chunk[..n as usize]),
            0 => return scan,
            _ if retry(io::Error::last_os_error().kind()) => {}
            _ => return scan,
        }
    }
}

fn retry(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock)
}

/// Writes as much of `bytes` as the terminal takes before `deadline`. Returns how much.
fn send(fd: RawFd, bytes: &[u8], deadline: Instant, stopped: &dyn Fn() -> bool) -> usize {
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
        let kind = io::Error::last_os_error().kind();
        if !retry(kind) {
            break;
        }
        if kind == io::ErrorKind::WouldBlock && ready(fd, Dir::Write, left.min(SLICE)).is_err() {
            break;
        }
    }
    sent
}

enum Dir {
    Read,
    Write,
}

/// Waits up to `timeout` for `fd`. `poll` takes a descriptor of any number. macOS answers
/// POLLNVAL for /dev/tty itself, which Scry opens only when stdin is not a terminal, and
/// there crossterm cannot read keys either; that is an error here.
fn ready(fd: RawFd, dir: Dir, timeout: Duration) -> io::Result<bool> {
    let events = match dir {
        Dir::Read => libc::POLLIN,
        Dir::Write => libc::POLLOUT,
    };
    let mut watched = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let ms = timeout
        .as_micros()
        .div_ceil(1000)
        .min(libc::c_int::MAX as u128) as libc::c_int;
    // SAFETY: `watched` is one live pollfd for the length of the call.
    let n = unsafe { libc::poll(&mut watched, 1, ms) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if watched.revents & libc::POLLNVAL != 0 {
        return Err(io::Error::from_raw_os_error(libc::EBADF));
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

    /// The first bytes of a key the reads stopped inside: the end of what was typed, or an
    /// answer cut at `ESC [`, which may still turn out to be a key.
    fn cut(&self) -> &[u8] {
        let tail = self
            .open
            .as_ref()
            .map_or(self.typed.as_slice(), |(_, bytes)| bytes.as_slice());
        &tail[keys_of(tail).1..]
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
    /// Keys that may be part of an answer.
    held: Vec<KeyEvent>,
    /// The first bytes of a key that Scry read before the event loop. Crossterm reads the
    /// rest as keys of their own, which complete it here.
    cut: Vec<u8>,
    last: Instant,
}

impl LateReply {
    /// What still needs picking out after a wait that ended with `scan`: the rest of an
    /// answer when the wait ended before DA1, and the rest of a key the reads stopped
    /// inside.
    fn after(scan: &Scan, now: Instant) -> Option<LateReply> {
        let cut = scan.cut().to_vec();
        if scan.done && cut.is_empty() {
            return None;
        }
        let (at, held) = match &scan.open {
            // An answer cut at `ESC [` is the cut key too, so it stays bytes.
            Some((at, _)) if !cut.is_empty() => (*at, Vec::new()),
            Some((at, bytes)) => (*at, keys_of(bytes).0),
            None => (Grammar::Ground, Vec::new()),
        };
        let until = if scan.done { now } else { now + LATE_FOR };
        Some(LateReply {
            until,
            at,
            held,
            cut,
            last: now,
        })
    }

    /// The keys to handle now, in order: none while `key` may be part of an answer or of a
    /// cut key, else any held keys that turned out not to be, then `key`.
    pub fn feed(&mut self, key: KeyEvent, now: Instant) -> Vec<KeyEvent> {
        self.last = now;
        if self.at != Grammar::Ground || now < self.until {
            let next = bytes_of(&key)
                .and_then(|bytes| bytes.iter().try_fold(self.at, |at, &b| at.step(b)));
            match next {
                Some(Grammar::Ground) => {
                    // A whole answer, which a cut key was the start of.
                    self.at = Grammar::Ground;
                    self.held.clear();
                    self.cut.clear();
                    return Vec::new();
                }
                Some(at) => {
                    self.at = at;
                    self.held.push(key);
                    return Vec::new();
                }
                // Not an answer after all, and `key` may start the next one.
                None if self.at != Grammar::Ground => {
                    let mut out = self.release();
                    out.extend(self.feed(key, now));
                    return out;
                }
                None => {}
            }
        }
        self.typed(key)
    }

    /// Ends a possible answer, whose held keys were typed after all.
    fn release(&mut self) -> Vec<KeyEvent> {
        self.at = Grammar::Ground;
        let held = std::mem::take(&mut self.held);
        held.into_iter().flat_map(|k| self.typed(k)).collect()
    }

    /// `key` as typed. While a cut key waits for its rest, a key that continues it
    /// completes it instead. One that does not goes through, and the cut key is dropped,
    /// as crossterm drops a broken sequence.
    fn typed(&mut self, key: KeyEvent) -> Vec<KeyEvent> {
        if self.cut.is_empty() {
            return vec![key];
        }
        let mut joined = std::mem::take(&mut self.cut);
        let Some(bytes) = bytes_of(&key) else {
            return vec![key];
        };
        joined.extend(bytes);
        match next_key(&joined) {
            Next::Short => {
                self.cut = joined;
                Vec::new()
            }
            Next::Key(whole, n) if n == joined.len() => whole.into_iter().collect(),
            Next::Key(..) => vec![key],
        }
    }

    /// Once the window has passed and no key came for a moment: the keys still held, which
    /// were typed after all. The guard is done then, and a cut key whose rest never came
    /// is dropped with it.
    pub fn expire(&mut self, now: Instant) -> Option<Vec<KeyEvent>> {
        if now < self.until || now < self.last + LATE_QUIET {
            return None;
        }
        Some(self.release())
    }
}

/// The bytes crossterm read to make `key`, for the keys an answer or the rest of a key can
/// turn into.
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

/// One key read from the front of some bytes.
#[derive(Debug, PartialEq)]
enum Next {
    /// A whole key of this many bytes. `None` for a key Scry has no use for, or for bytes
    /// that make no key.
    Key(Option<KeyEvent>, usize),
    /// The start of a key whose rest has not come.
    Short,
}

/// Reads the key at the front of `bytes` as crossterm does, for every key Scry handles:
/// characters, enter, tab, backspace, esc, ctrl and alt chords, and the arrows, Home, End,
/// PageUp, PageDown and BackTab with their modifiers. An ESC with nothing after it is Esc,
/// as crossterm reads one that ends a read.
fn next_key(bytes: &[u8]) -> Next {
    match bytes {
        [] => Next::Short,
        [ESC] => Next::Key(Some(plain(KeyCode::Esc)), 1),
        [ESC, ESC, ..] => Next::Key(Some(plain(KeyCode::Esc)), 2),
        [ESC, b'[', rest @ ..] => csi(rest),
        [ESC, b'O'] => Next::Short,
        [ESC, b'O', c, ..] => Next::Key(cursor(*c).map(plain), 3),
        [ESC, rest @ ..] => match next_key(rest) {
            Next::Key(key, n) => Next::Key(
                key.map(|mut k| {
                    k.modifiers |= KeyModifiers::ALT;
                    k
                }),
                n + 1,
            ),
            Next::Short => Next::Short,
        },
        [b, ..] if b.is_ascii() => Next::Key(Some(key_of(char::from(*b))), 1),
        [lead, rest @ ..] => {
            let len = match lead {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Next::Key(None, 1),
            };
            let tail = &rest[..rest.len().min(len - 1)];
            if tail.iter().any(|b| b & 0xc0 != 0x80) {
                return Next::Key(None, 1);
            }
            if tail.len() < len - 1 {
                return Next::Short;
            }
            let c = std::str::from_utf8(&bytes[..len])
                .ok()
                .and_then(|s| s.chars().next());
            Next::Key(c.map(key_of), len)
        }
    }
}

/// The key of `ESC [ params final`, read from the bytes after `ESC [`.
fn csi(rest: &[u8]) -> Next {
    // Parameter and intermediate bytes, then one final byte.
    let Some(end) = rest.iter().position(|b| !(0x20..=0x3f).contains(b)) else {
        return Next::Short;
    };
    let last = rest[end];
    if !(0x40..=0x7e).contains(&last) {
        // No sequence after all: drop it up to the stray byte, which is read again.
        return Next::Key(None, end + 2);
    }
    let params = std::str::from_utf8(&rest[..end]).unwrap_or_default();
    let mut fields = params.split(';');
    let first = fields.next().unwrap_or_default();
    let mask = fields
        .next()
        .and_then(|f| f.split(':').next()?.parse::<u8>().ok());
    let key = match last {
        b'Z' if params.is_empty() => Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        b'~' => match first.parse::<u8>() {
            Ok(1 | 7) => Some(KeyCode::Home),
            Ok(4 | 8) => Some(KeyCode::End),
            Ok(5) => Some(KeyCode::PageUp),
            Ok(6) => Some(KeyCode::PageDown),
            _ => None,
        }
        .map(|code| KeyEvent::new(code, modifiers(mask))),
        // `ESC [ 5 C` carries its modifiers in its only parameter.
        c => cursor(c).map(|code| {
            let digit = || params.bytes().last()?.checked_sub(b'0').filter(|d| *d <= 9);
            KeyEvent::new(code, modifiers(mask.or_else(digit)))
        }),
    };
    Next::Key(key, end + 3)
}

/// The modifiers in an xterm key parameter: one plus a bit each for shift, alt, ctrl,
/// super, hyper and meta.
fn modifiers(mask: Option<u8>) -> KeyModifiers {
    let bits = mask.unwrap_or(1).saturating_sub(1);
    [
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL,
        KeyModifiers::SUPER,
        KeyModifiers::HYPER,
        KeyModifiers::META,
    ]
    .into_iter()
    .enumerate()
    .filter(|(i, _)| bits & (1 << i) != 0)
    .fold(KeyModifiers::NONE, |all, (_, m)| all | m)
}

fn cursor(c: u8) -> Option<KeyCode> {
    Some(match c {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        _ => return None,
    })
}

fn plain(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The keys crossterm reads from `bytes`, and how many bytes they take. Whatever is left is
/// the start of a key cut off at the end.
fn keys_of(bytes: &[u8]) -> (Vec<KeyEvent>, usize) {
    let (mut keys, mut used) = (Vec::new(), 0);
    while let Next::Key(key, n) = next_key(&bytes[used..]) {
        keys.extend(key);
        used += n;
    }
    (keys, used)
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
        keys_of(bytes).0
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

    fn codes(keys: &[KeyEvent]) -> Vec<(KeyCode, KeyModifiers)> {
        keys.iter().map(|k| (k.code, k.modifiers)).collect()
    }

    /// The guard after a wait that ended with `bytes`.
    fn after(bytes: &[u8]) -> (LateReply, Instant) {
        let now = Instant::now();
        (LateReply::after(&Scan::of(bytes), now).unwrap(), now)
    }

    fn guard() -> (LateReply, Instant) {
        after(b"")
    }

    #[test]
    fn a_whole_answer_gives_the_colours_and_ends_at_da1() {
        let s = Scan::of(&[FG, BG, DA1].concat());
        assert_eq!(s.fg, Some(Rgb(0xc5, 0xc8, 0xc6)));
        assert_eq!(s.bg, Some(Rgb(0x1d, 0x1f, 0x21)));
        assert!(s.done && s.typed.is_empty() && s.open.is_none());
        assert!(LateReply::after(&s, Instant::now()).is_none());
        // A terminal without the colour query answers DA1 alone.
        let s = Scan::of(DA1);
        assert!(s.done && s.fg.is_none() && s.bg.is_none());
    }

    #[test]
    fn keys_typed_around_the_answer_are_kept_in_order() {
        let s = Scan::of(&[b"3".as_slice(), FG, b"/x", BG, DA1, b"j\x1b"].concat());
        assert!(s.done);
        assert_eq!(s.bg, Some(Rgb(0x1d, 0x1f, 0x21)));
        assert_eq!(chars(&read(&s.typed)), "3/xj⎋");
        // An escape that starts no answer is a key too.
        let s = Scan::of(b"\x1bj\x1b]1x");
        assert_eq!(s.typed, b"\x1bj\x1b]1x");
        assert_eq!(read(b"\x1bj")[0].modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn an_answer_cut_at_the_deadline_keeps_its_place() {
        let s = Scan::of(&[FG, &BG[..12]].concat());
        assert!(!s.done);
        assert_eq!(s.fg, Some(Rgb(0xc5, 0xc8, 0xc6)));
        assert_eq!(s.open, Some((Grammar::Spec(7), BG[..12].to_vec())));
        assert!(s.typed.is_empty());
        // The cut answer is no key cut in two, so there is nothing more to read for it.
        assert!(s.cut().is_empty());
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
        let (mut late, now) = after(&BG[..12]);
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
        let typed = [
            b"a\r\t\x7f\x03G\x1bx\x1b\x1b".as_slice(),
            b"\x1b[A\x1b[1;5C\x1b[1;2B\x1b[1;3D\x1b[5D\x1bOA",
            b"\x1b[H\x1b[F\x1b[1~\x1b[4~\x1b[5~\x1b[6~\x1b[6;5~\x1b[Z",
            "é\x1bé".as_bytes(),
            b"\x1b[15~\x1b[?62;22c\x1b[I",
        ]
        .concat();
        let (keys, used) = keys_of(&typed);
        assert_eq!(used, typed.len());
        use KeyCode::*;
        let (none, shift, alt, ctrl) = (
            KeyModifiers::NONE,
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        );
        assert_eq!(
            codes(&keys),
            [
                (Char('a'), none),
                (Enter, none),
                (Tab, none),
                (Backspace, none),
                (Char('c'), ctrl),
                (Char('G'), shift),
                (Char('x'), alt),
                (Esc, none),
                (Up, none),
                (Right, ctrl),
                (Down, shift),
                (Left, alt),
                (Left, ctrl),
                (Up, none),
                (Home, none),
                (End, none),
                (Home, none),
                (End, none),
                (PageUp, none),
                (PageDown, none),
                (PageDown, ctrl),
                (BackTab, shift),
                (Char('é'), none),
                (Char('é'), alt),
                // F5, a DA1 answer and focus: read whole, and dropped.
            ]
        );
    }

    #[test]
    fn a_key_cut_at_the_end_waits_for_its_rest() {
        for (bytes, keys, cut) in [
            (b"j\x1b[1;5".as_slice(), "j", b"\x1b[1;5".as_slice()),
            (b"/\xc3", "/", b"\xc3"),
            (b"\x1b\xe2\x82", "", b"\x1b\xe2\x82"),
            (b"\x1bO", "", b"\x1bO"),
            // A lone ESC is Esc, as when it ends crossterm's read.
            (b"\x1b", "⎋", b""),
        ] {
            let (read, used) = keys_of(bytes);
            assert_eq!((chars(&read).as_str(), &bytes[used..]), (keys, cut));
        }
        // The first byte of a character never becomes U+FFFD.
        assert_eq!(read(b"\xc3j"), read(b"j"));
    }

    #[test]
    fn a_key_split_where_scry_stopped_reading_is_finished_by_the_keys_after_it() {
        // BackTab cut at the 1 s deadline: `ESC [` could have been DA1, so it is held.
        let (mut late, now) = after(b"\x1b[");
        assert_eq!(
            codes(&late.feed(read(b"Z")[0], now)),
            [(KeyCode::BackTab, KeyModifiers::SHIFT)]
        );
        // The late answer that follows is still swallowed.
        assert!(feed_all(&mut late, &read(&[FG, BG].concat()), now).is_empty());

        // Ctrl-Right cut after DA1: crossterm reads `5C` as two characters.
        let (mut late, now) = after(&[DA1, b"\x1b[1;"].concat());
        let out = feed_all(&mut late, &read(b"5C"), now);
        assert_eq!(codes(&out), [(KeyCode::Right, KeyModifiers::CONTROL)]);
        assert_eq!(chars(&late.feed(read(b"j")[0], now)), "j");

        // A key that does not continue the cut one goes through.
        let (mut late, now) = after(&[DA1, b"\xc3"].concat());
        assert_eq!(chars(&late.feed(read(b"j")[0], now)), "j");

        // DA1 cut at `ESC [`: its rest is the answer, not keys.
        let (mut late, now) = after(b"\x1b[");
        assert!(feed_all(&mut late, &read(&DA1[2..]), now).is_empty());
        assert_eq!(chars(&late.feed(read(b"q")[0], now)), "q");
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

    /// A new pty: the terminal emulator's end, then the program's end.
    fn pty() -> (OwnedFd, OwnedFd) {
        let (mut ours, mut theirs) = (0, 0);
        let none = std::ptr::null_mut();
        // SAFETY: openpty writes two descriptors into live integers. Null name, modes and
        // size are allowed and mean the defaults.
        let made = unsafe { libc::openpty(&mut ours, &mut theirs, none, none.cast(), none.cast()) };
        assert_eq!(made, 0, "openpty: {}", io::Error::last_os_error());
        // SAFETY: openpty returned two new descriptors that nothing else owns.
        unsafe { (OwnedFd::from_raw_fd(ours), OwnedFd::from_raw_fd(theirs)) }
    }

    /// Which file `fd` is open on, or `None` when it is closed.
    fn identity(fd: RawFd) -> Option<(libc::dev_t, libc::ino_t, libc::dev_t)> {
        // SAFETY: stat is plain data, so all zeroes is a valid value for fstat to fill.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat accepts any descriptor and writes into a live stat.
        (unsafe { libc::fstat(fd, &mut st) } == 0).then_some((st.st_dev, st.st_ino, st.st_rdev))
    }

    #[test]
    fn a_terminal_numbered_above_fd_setsize_is_kept_and_watched() {
        let floor = libc::FD_SETSIZE as libc::c_int + 1;
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit and setrlimit read and write a live rlimit.
        unsafe {
            libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit);
            if limit.rlim_cur <= floor as libc::rlim_t {
                limit.rlim_cur = (2 * floor as libc::rlim_t).min(limit.rlim_max);
                libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
            }
        }
        let (ours, theirs) = pty();
        // SAFETY: F_DUPFD_CLOEXEC makes a new descriptor numbered `floor` or above.
        let high = unsafe { libc::fcntl(theirs.as_raw_fd(), libc::F_DUPFD_CLOEXEC, floor) };
        assert!(high >= floor, "dup: {}", io::Error::last_os_error());
        // SAFETY: fcntl returned a new descriptor that nothing else owns.
        let high = unsafe { OwnedFd::from_raw_fd(high) };
        let saved = Saved::of(Some(high)).expect("the modes are kept whatever the number");
        let fd = saved.raw_fd();

        assert!(!ready(fd, Dir::Read, Duration::ZERO).unwrap());
        // A whole line, since the new terminal is in canonical mode.
        // SAFETY: writes two bytes from a live buffer.
        unsafe { libc::write(ours.as_raw_fd(), b"x\n".as_ptr().cast(), 2) };
        assert!(ready(fd, Dir::Read, Duration::from_secs(1)).unwrap());

        // Raw mode through the shell's descriptor, then the saved modes back through ours.
        let mut raw = saved.cooked;
        // SAFETY: cfmakeraw and tcsetattr take a live termios and an open descriptor.
        unsafe {
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(theirs.as_raw_fd(), libc::TCSANOW, &raw);
        }
        saved.put_back();
        // SAFETY: as for `saved.cooked`, tcgetattr fills a zeroed termios.
        let mut now: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `theirs` is open, and `now` is a valid termios to write into.
        unsafe { libc::tcgetattr(theirs.as_raw_fd(), &mut now) };
        assert_ne!(now.c_lflag & libc::ICANON, 0, "cooked again");
    }

    #[test]
    fn a_terminal_that_is_not_kept_is_closed() {
        // tcgetattr fails on a file that is not a terminal.
        let file = OwnedFd::from(tempfile::tempfile().unwrap());
        let fd = file.as_raw_fd();
        let was = identity(fd);
        assert!(Saved::of(Some(file)).is_none());
        assert_ne!(identity(fd), was, "descriptor {fd} is still open");

        // A second save keeps the first terminal and closes the one it brought.
        let slot = OnceLock::new();
        let (_a, first) = pty();
        let (_b, second) = pty();
        let fd = second.as_raw_fd();
        let was = identity(fd);
        assert!(slot.set(Saved::of(Some(first)).unwrap()).is_ok());
        assert!(slot.set(Saved::of(Some(second)).unwrap()).is_err());
        assert_ne!(identity(fd), was, "descriptor {fd} is still open");
    }
}
