//! The terminal outside ratatui: the colour query at startup, flushing input before the
//! first key is read, and putting the terminal back from the signal thread.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::theme::Rgb;

/// Foreground (OSC 10), background (OSC 11), then primary device attributes (DA1).
const QUERY: &[u8] = b"\x1b]10;?\x07\x1b]11;?\x07\x1b[c";
const SLICE: Duration = Duration::from_millis(50);
const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;

struct Saved {
    fd: libc::c_int,
    termios: libc::termios,
}

static SAVED: OnceLock<Saved> = OnceLock::new();

/// Remembers the terminal's modes before Scry changes any, so `restore` can put them back
/// from anywhere. Uses the same terminal as crossterm: stdin when it is one, else /dev/tty.
pub fn save() {
    let fd = if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        libc::STDIN_FILENO
    } else {
        // Kept open for the life of the process.
        unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_NOCTTY) }
    };
    if fd < 0 {
        return;
    }
    if let Some(termios) = get(fd) {
        let _ = SAVED.set(Saved { fd, termios });
    }
}

/// Puts back the saved modes, leaves the alternate screen and shows the cursor. Safe to
/// call from the signal thread: plain syscalls, no locks, and on a hung-up terminal each
/// fails at once with EIO instead of blocking.
pub fn restore() {
    if let Some(s) = SAVED.get() {
        unsafe { libc::tcsetattr(s.fd, libc::TCSANOW, &s.termios) };
    }
    let seq = b"\x1b[?1049l\x1b[?25h";
    unsafe { libc::write(libc::STDOUT_FILENO, seq.as_ptr().cast(), seq.len()) };
}

/// Drops input that arrived but was not read yet, so nothing typed or answered during
/// startup reaches the event loop as keys.
pub fn flush_input() {
    if let Some(s) = SAVED.get() {
        unsafe { libc::tcflush(s.fd, libc::TCIFLUSH) };
    }
}

/// Asks the terminal for its default foreground and background.
///
/// Terminals answer in order, and every terminal answers DA1, so its reply marks the end:
/// whatever came before it is the whole answer, and nothing is left in the input for the
/// event loop to read as keys. Reading stops at that reply, at `wait`, or when `stop` is
/// set. Returns `(background, foreground)`.
pub fn query_colors(wait: Duration, stop: &AtomicBool) -> Option<(Rgb, Rgb)> {
    let s = SAVED.get()?;
    let mut raw = s.termios;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(s.fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    let reply = exchange(s.fd, wait, stop);
    unsafe { libc::tcsetattr(s.fd, libc::TCSANOW, &s.termios) };
    let reply = reply?;
    Some((reply.bg?, reply.fg?))
}

fn exchange(fd: libc::c_int, wait: Duration, stop: &AtomicBool) -> Option<Reply> {
    write_all(fd, QUERY)?;
    let deadline = Instant::now() + wait;
    let mut got = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let reply = Reply::parse(&got);
        let left = deadline.saturating_duration_since(Instant::now());
        if reply.done || left.is_zero() || stop.load(Ordering::Relaxed) {
            return Some(reply);
        }
        match readable(fd, left.min(SLICE)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Some(reply),
        }
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        match n {
            n if n > 0 => got.extend_from_slice(&chunk[..n as usize]),
            0 => return Some(reply),
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => return Some(reply),
        }
    }
}

fn write_all(fd: libc::c_int, mut bytes: &[u8]) -> Option<()> {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        bytes = &bytes[n as usize..];
    }
    Some(())
}

/// `select`, because macOS `poll` does not work on /dev/tty.
fn readable(fd: libc::c_int, timeout: Duration) -> std::io::Result<bool> {
    let mut set: libc::fd_set = unsafe { std::mem::zeroed() };
    unsafe {
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
    }
    let mut tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    let n = unsafe {
        libc::select(
            fd + 1,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        )
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n > 0)
}

fn get(fd: libc::c_int) -> Option<libc::termios> {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    (unsafe { libc::tcgetattr(fd, &mut t) } == 0).then_some(t)
}

/// What the terminal has answered so far.
#[derive(Debug, Default, PartialEq, Eq)]
struct Reply {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    /// The DA1 reply arrived, so nothing more is coming.
    done: bool,
}

impl Reply {
    fn parse(bytes: &[u8]) -> Reply {
        Reply {
            fg: osc_color(bytes, b"\x1b]10;"),
            bg: osc_color(bytes, b"\x1b]11;"),
            done: da1_end(bytes).is_some(),
        }
    }
}

/// The end of a `CSI ? … c` reply.
fn da1_end(bytes: &[u8]) -> Option<usize> {
    let start = find(bytes, b"\x1b[?")? + 3;
    let len = bytes[start..]
        .iter()
        .position(|b| !(b.is_ascii_digit() || *b == b';'))?;
    (bytes[start + len] == b'c').then_some(start + len + 1)
}

/// The colour in an `OSC 1x ; spec` reply ended by BEL or ST.
fn osc_color(bytes: &[u8], prefix: &[u8]) -> Option<Rgb> {
    let start = find(bytes, prefix)? + prefix.len();
    let len = bytes[start..].iter().position(|&b| b == BEL || b == ESC)?;
    parse_color(std::str::from_utf8(&bytes[start..start + len]).ok()?)
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

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_whole_only_once_da1_arrives() {
        let fg = b"\x1b]10;rgb:c5c5/c8c8/c6c6\x1b\\".as_slice();
        let bg = b"\x1b]11;rgb:1d1d/1f1f/2121\x07".as_slice();
        let da1 = b"\x1b[?62;22c".as_slice();
        let partial = Reply::parse(&[fg, &bg[..9]].concat());
        assert_eq!(partial.fg, Some(Rgb(0xc5, 0xc8, 0xc6)));
        assert_eq!((partial.bg, partial.done), (None, false));
        let whole = Reply::parse(&[fg, bg, da1].concat());
        assert_eq!(whole.bg, Some(Rgb(0x1d, 0x1f, 0x21)));
        assert!(whole.done);
        // A terminal without the colour query answers DA1 alone.
        assert_eq!(
            Reply::parse(da1),
            Reply {
                done: true,
                ..Reply::default()
            }
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
