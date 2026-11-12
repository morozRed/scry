//! Reads every TCP and UDP socket on the machine, for all users and without root, from
//! `sysctl net.inet.{tcp,udp}.pcblist_n`.
//!
//! The buffer is a private xnu layout (`get_pcblist_n` in bsd/netinet/in_pcblist.c): an
//! `xinpgen` header, then for each socket a run of tagged items, then a closing `xinpgen`.
//! Every item opens with its length and kind, and each starts on an 8-byte boundary. The
//! structs are declared under `#pragma pack(4)`, so the offsets below are 4-byte packed.
//! Parsing walks by the tags and refuses any kind or length it was not written for.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result, bail};

use crate::model::{IpFamily, Proto, Socket, TcpState};

/// `struct xinpgen`: length, count, generation, socket generation.
const XINPGEN_LEN: usize = 24;
const XIG_COUNT: usize = 4;
const XIG_GEN: usize = 8;

// Item kinds from <sys/socketvar.h>.
const XSO_SOCKET: u32 = 0x001;
const XSO_RCVBUF: u32 = 0x002;
const XSO_SNDBUF: u32 = 0x004;
const XSO_STATS: u32 = 0x008;
const XSO_INPCB: u32 = 0x010;
const XSO_TCPCB: u32 = 0x020;

struct ItemKind {
    kind: u32,
    name: &'static str,
    /// The exact `*_len` this parser accepts. A different length is a different layout.
    len: usize,
}

/// Every item kind a TCP or UDP record can carry, as macOS 26 (xnu-12377) writes them.
const ITEM_KINDS: [ItemKind; 6] = [
    ItemKind {
        kind: XSO_INPCB,
        name: "xinpcb_n",
        len: 104,
    },
    ItemKind {
        kind: XSO_SOCKET,
        name: "xsocket_n",
        len: 104,
    },
    ItemKind {
        kind: XSO_RCVBUF,
        name: "xsockbuf_n (receive)",
        len: 32,
    },
    ItemKind {
        kind: XSO_SNDBUF,
        name: "xsockbuf_n (send)",
        len: 32,
    },
    ItemKind {
        kind: XSO_STATS,
        name: "xsockstat_n",
        len: 136,
    },
    ItemKind {
        kind: XSO_TCPCB,
        name: "xtcpcb_n",
        len: 204,
    },
];

const UDP_RECORD: u32 = XSO_INPCB | XSO_SOCKET | XSO_RCVBUF | XSO_SNDBUF | XSO_STATS;
const TCP_RECORD: u32 = UDP_RECORD | XSO_TCPCB;

// `struct xinpcb_n` fields.
const INP_FPORT: usize = 16;
const INP_LPORT: usize = 18;
const INP_GENCNT: usize = 28;
const INP_VFLAG: usize = 44;
/// `inp_dependfaddr`: an `in6_addr`, or for IPv4 three pad words and an `in_addr`.
const INP_FADDR: usize = 48;
const INP_LADDR: usize = 64;
const INP_IPV4: u8 = 0x1;
const INP_IPV6: u8 = 0x2;

// `struct xsocket_n` fields.
const SO_PROTOCOL: usize = 36;
const SO_UID: usize = 64;
const SO_LAST_PID: usize = 68;
const SO_E_PID: usize = 72;

// `struct xtcpcb_n` fields: `t_state` follows `t_segq`, `t_dupacks` and four timers.
const T_STATE: usize = 36;

#[derive(Debug)]
pub enum PcbError {
    /// The table changed while the kernel copied it out. Reading again usually succeeds.
    Changed { start: u64, end: u64 },
    /// The bytes do not match the layout this build knows.
    Layout(String),
}

impl fmt::Display for PcbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PcbError::Changed { start, end } => write!(
                f,
                "socket table changed during the read (generation {start} at the start, {end} at the end)"
            ),
            PcbError::Layout(why) => write!(
                f,
                "pcblist_n has a layout this build does not know: {why}. Run `scry doctor`."
            ),
        }
    }
}

impl std::error::Error for PcbError {}

fn layout(why: impl Into<String>) -> PcbError {
    PcbError::Layout(why.into())
}

fn u16_be(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

fn u32_ne(b: &[u8], o: usize) -> u32 {
    u32::from_ne_bytes(b[o..o + 4].try_into().unwrap())
}

fn i32_ne(b: &[u8], o: usize) -> i32 {
    i32::from_ne_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_ne(b: &[u8], o: usize) -> u64 {
    u64::from_ne_bytes(b[o..o + 8].try_into().unwrap())
}

fn round8(n: usize) -> usize {
    (n + 7) & !7
}

#[derive(Default)]
struct Record<'a> {
    seen: u32,
    inp: &'a [u8],
    so: &'a [u8],
    tcp: &'a [u8],
}

/// Walks one `pcblist_n` buffer into sockets.
pub fn parse(buf: &[u8], proto: Proto) -> Result<Vec<Socket>, PcbError> {
    if buf.len() < XINPGEN_LEN {
        return Err(layout(format!(
            "{} bytes is shorter than xinpgen",
            buf.len()
        )));
    }
    let head_len = u32_ne(buf, 0) as usize;
    if head_len != XINPGEN_LEN {
        return Err(layout(format!(
            "xinpgen is {head_len} bytes, not {XINPGEN_LEN}"
        )));
    }
    let head_count = u32_ne(buf, XIG_COUNT) as usize;
    let head_gen = u64_ne(buf, XIG_GEN);
    // With no sockets at all, xnu writes the opening `xinpgen` and stops.
    if buf.len() == XINPGEN_LEN && head_count == 0 {
        return Ok(Vec::new());
    }
    let (complete, expected_proto) = match proto {
        Proto::Tcp => (TCP_RECORD, libc::IPPROTO_TCP),
        Proto::Udp => (UDP_RECORD, libc::IPPROTO_UDP),
    };

    let mut sockets = Vec::new();
    let mut rec = Record::default();
    let mut off = round8(head_len);
    loop {
        if off + 8 > buf.len() {
            return Err(layout(format!(
                "no closing xinpgen before byte {}",
                buf.len()
            )));
        }
        let len = u32_ne(buf, off) as usize;
        if len <= XINPGEN_LEN {
            if len != XINPGEN_LEN || off + len != buf.len() {
                return Err(layout(format!(
                    "closing xinpgen at byte {off} is {len} bytes in a {}-byte buffer",
                    buf.len()
                )));
            }
            if rec.seen != 0 {
                return Err(layout(format!(
                    "the last record is missing kinds {:#x}",
                    complete & !rec.seen
                )));
            }
            let end_gen = u64_ne(buf, off + XIG_GEN);
            if end_gen != head_gen {
                return Err(PcbError::Changed {
                    start: head_gen,
                    end: end_gen,
                });
            }
            if sockets.len() > head_count {
                return Err(layout(format!(
                    "{} records but the header counted {head_count}",
                    sockets.len()
                )));
            }
            return Ok(sockets);
        }
        let kind = u32_ne(buf, off + 4);
        let Some(known) = ITEM_KINDS.iter().find(|k| k.kind == kind) else {
            return Err(layout(format!("unknown item kind {kind:#x} at byte {off}")));
        };
        if len != known.len {
            return Err(layout(format!(
                "{} is {len} bytes, not {}",
                known.name, known.len
            )));
        }
        if complete & kind == 0 {
            return Err(layout(format!(
                "{} in a {} table",
                known.name,
                proto.name()
            )));
        }
        if off + len > buf.len() {
            return Err(layout(format!("{} runs past the buffer end", known.name)));
        }
        if rec.seen & kind != 0 {
            return Err(layout(format!("{} twice in one record", known.name)));
        }
        let item = &buf[off..off + len];
        rec.seen |= kind;
        match kind {
            XSO_INPCB => rec.inp = item,
            XSO_SOCKET => rec.so = item,
            XSO_TCPCB => rec.tcp = item,
            _ => {}
        }
        if rec.seen == complete {
            if let Some(s) = socket(&rec, proto, expected_proto, head_gen)? {
                sockets.push(s);
            }
            rec = Record::default();
        }
        off += round8(len);
    }
}

fn socket(
    rec: &Record,
    proto: Proto,
    expected_proto: i32,
    head_gen: u64,
) -> Result<Option<Socket>, PcbError> {
    let (inp, so) = (rec.inp, rec.so);
    let protocol = i32_ne(so, SO_PROTOCOL);
    if protocol != expected_proto {
        return Err(layout(format!(
            "xso_protocol {protocol} in the {} table",
            proto.name()
        )));
    }
    // `xso_family` is not checked: a few live TCP records carry 0 there, and netstat, which
    // goes by `inp_vflag` alone, lists them.

    // A record newer than the table was freed and reused while the kernel copied the table
    // out. netstat skips these too.
    if u64_ne(inp, INP_GENCNT) > head_gen {
        return Ok(None);
    }
    let vflag = inp[INP_VFLAG];
    let family = match (vflag & INP_IPV4 != 0, vflag & INP_IPV6 != 0) {
        (true, false) => IpFamily::V4,
        (false, true) => IpFamily::V6,
        (true, true) => IpFamily::Dual,
        (false, false) => return Ok(None),
    };
    let addr = |at: usize| -> IpAddr {
        if vflag & INP_IPV4 != 0 {
            let b: [u8; 4] = inp[at + 12..at + 16].try_into().unwrap();
            IpAddr::V4(Ipv4Addr::from(b))
        } else {
            let b: [u8; 16] = inp[at..at + 16].try_into().unwrap();
            IpAddr::V6(Ipv6Addr::from(b))
        }
    };
    let state = match proto {
        Proto::Tcp => {
            let raw = i32_ne(rec.tcp, T_STATE);
            Some(TcpState::from_raw(raw).ok_or_else(|| layout(format!("t_state {raw}")))?)
        }
        Proto::Udp => None,
    };
    Ok(Some(Socket {
        proto,
        family,
        local_ip: addr(INP_LADDR),
        local_port: u16_be(inp, INP_LPORT),
        remote_ip: addr(INP_FADDR),
        remote_port: u16_be(inp, INP_FPORT),
        state,
        pid: i32_ne(so, SO_LAST_PID),
        epid: i32_ne(so, SO_E_PID),
        uid: u32_ne(so, SO_UID),
    }))
}

fn sysctl_name(proto: Proto) -> &'static str {
    match proto {
        Proto::Tcp => "net.inet.tcp.pcblist_n",
        Proto::Udp => "net.inet.udp.pcblist_n",
    }
}

/// The raw table for one protocol, as the kernel copies it out.
fn read_raw(proto: Proto) -> Result<Vec<u8>> {
    let name = std::ffi::CString::new(sysctl_name(proto)).unwrap();
    let mut buf: Vec<u8> = Vec::new();
    for _ in 0..4 {
        let mut len: usize = 0;
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("sysctl {} size", sysctl_name(proto)));
        }
        len += len / 8;
        buf.resize(len, 0);
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            buf.truncate(len);
            return Ok(buf);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("sysctl {}", sysctl_name(proto)));
        }
    }
    bail!("sysctl {} kept growing", sysctl_name(proto))
}

/// Reads one protocol's table, again if it changed during the copy.
pub fn read(proto: Proto) -> Result<Vec<Socket>> {
    const TRIES: usize = 8;
    let mut last = None;
    for _ in 0..TRIES {
        match parse(&read_raw(proto)?, proto) {
            Ok(s) => return Ok(s),
            Err(e @ PcbError::Changed { .. }) => last = Some(e),
            Err(e) => return Err(e.into()),
        }
    }
    Err(last.unwrap()).context(format!("{TRIES} reads in a row"))
}

/// Every TCP and UDP socket on the machine.
pub fn read_all() -> Result<Vec<Socket>> {
    let mut all = read(Proto::Tcp)?;
    all.extend(read(Proto::Udp)?);
    Ok(all)
}

const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
/// `sizeof(struct socket_fdinfo)` from <sys/proc_info.h>, with the offsets Scry reads.
const SOCKET_FDINFO_LEN: usize = 792;
const SOI_PROTOCOL: usize = 180;
const SOI_FAMILY: usize = 184;
const SOI_KIND: usize = 256;
const INSI_FPORT: usize = 264;
const INSI_LPORT: usize = 268;
/// `tcpsi_state`, after the 80-byte `in_sockinfo` in `tcp_sockinfo`.
const TCPSI_STATE: usize = 344;
const TSI_S_LISTEN: i32 = 1;
const SOCKINFO_IN: i32 = 1;
const SOCKINFO_TCP: i32 = 2;

/// Whether `pid` has a listening `proto` socket on `port` among its open files. This checks
/// the socket table's `so_last_pid`, which can name a process that has since closed the
/// socket. `None` when the process cannot be inspected, which is the case for other users.
pub fn pid_holds_port(pid: i32, proto: Proto, port: u16) -> Option<bool> {
    listening_ports(pid).map(|held| held.contains(&(proto, port)))
}

/// The ports `pid` listens on through its own open files: TCP sockets in `LISTEN`, and UDP
/// sockets bound to a port with no peer, as [`Socket::is_listening`] reads the table. `None`
/// when the process cannot be inspected.
pub fn listening_ports(pid: i32) -> Option<Vec<(Proto, u16)>> {
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    let need =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return None;
    }
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        need as usize / entry + 16
    ];
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (fds.len() * entry) as libc::c_int,
        )
    };
    if got <= 0 {
        return None;
    }
    fds.truncate(got as usize / entry);
    let mut held = Vec::new();
    let mut info = [0u8; SOCKET_FDINFO_LEN];
    for fd in fds
        .iter()
        .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
    {
        let n = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDSOCKETINFO,
                info.as_mut_ptr().cast(),
                SOCKET_FDINFO_LEN as libc::c_int,
            )
        };
        if n as usize != SOCKET_FDINFO_LEN {
            continue;
        }
        let kind = i32_ne(&info, SOI_KIND);
        let family = i32_ne(&info, SOI_FAMILY);
        if family != libc::AF_INET && family != libc::AF_INET6 {
            continue;
        }
        let listening = match (i32_ne(&info, SOI_PROTOCOL), kind) {
            (libc::IPPROTO_TCP, SOCKINFO_TCP) => {
                (i32_ne(&info, TCPSI_STATE) == TSI_S_LISTEN).then_some(Proto::Tcp)
            }
            (libc::IPPROTO_UDP, SOCKINFO_IN) => {
                (i32_ne(&info, INSI_FPORT) == 0).then_some(Proto::Udp)
            }
            _ => None,
        };
        // `insi_lport` holds the port in network byte order in its low 16 bits.
        let port = u16::from_be(u32_ne(&info, INSI_LPORT) as u16);
        if let Some(proto) = listening
            && port != 0
        {
            held.push((proto, port));
        }
    }
    Some(held)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &[u8] = include_bytes!("../../../../fixtures/pcblist/tcp-macos26.bin");
    const UDP: &[u8] = include_bytes!("../../../../fixtures/pcblist/udp-macos26.bin");
    const LISTENERS: &str = include_str!("../../../../fixtures/pcblist/listeners-macos26.txt");

    fn listing(sockets: &[Socket]) -> Vec<String> {
        let mut v: Vec<String> = sockets
            .iter()
            .filter(|s| s.is_listening())
            .map(|s| {
                let fam = match s.family {
                    IpFamily::V4 => "4",
                    IpFamily::V6 => "6",
                    IpFamily::Dual => "46",
                };
                format!("{}{fam} {} {}", s.proto.name(), s.local_port, s.pid)
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn recorded_tables_match_what_netstat_listed_at_the_same_moment() {
        let tcp = parse(TCP, Proto::Tcp).expect("tcp parses");
        let udp = parse(UDP, Proto::Udp).expect("udp parses");
        assert!(
            tcp.len() > 50 && udp.len() > 20,
            "{} {}",
            tcp.len(),
            udp.len()
        );
        let mut ours = listing(&tcp);
        ours.extend(listing(&udp));
        ours.sort();
        let mut theirs: Vec<String> = LISTENERS.lines().map(str::to_string).collect();
        theirs.sort();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn recorded_tables_carry_no_real_addresses() {
        let doc = |ip: &IpAddr| match ip {
            IpAddr::V4(a) => a.octets()[..3] == [192, 0, 2],
            IpAddr::V6(a) => a.segments()[..2] == [0x2001, 0xdb8],
        };
        let ok = |ip: &IpAddr| ip.is_loopback() || ip.is_unspecified() || doc(ip);
        for s in parse(TCP, Proto::Tcp)
            .unwrap()
            .iter()
            .chain(parse(UDP, Proto::Udp).unwrap().iter())
        {
            assert!(ok(&s.local_ip) && ok(&s.remote_ip), "{s:?}");
        }
    }

    #[test]
    fn recorded_tcp_table_has_states_and_both_families() {
        let tcp = parse(TCP, Proto::Tcp).unwrap();
        assert!(tcp.iter().any(|s| s.state == Some(TcpState::Established)));
        assert!(tcp.iter().any(|s| s.family == IpFamily::V4));
        assert!(tcp.iter().any(|s| s.family == IpFamily::V6));
        assert!(tcp.iter().any(|s| s.local_ip.is_loopback()));
        assert!(
            tcp.iter()
                .all(|s| s.pid < 100 || (1000..2000).contains(&s.pid))
        );
    }

    /// Byte offset of the first item of `kind` in a recorded buffer.
    fn first(buf: &[u8], kind: u32) -> usize {
        let mut off = XINPGEN_LEN;
        loop {
            if u32_ne(buf, off + 4) == kind {
                return off;
            }
            off += round8(u32_ne(buf, off) as usize);
        }
    }

    fn put_u32(buf: &mut [u8], at: usize, v: u32) {
        buf[at..at + 4].copy_from_slice(&v.to_ne_bytes());
    }

    #[test]
    fn a_changed_generation_is_retryable() {
        let mut b = TCP.to_vec();
        let end = b.len() - XINPGEN_LEN;
        let g = u64_ne(&b, end + XIG_GEN) + 1;
        b[end + XIG_GEN..end + XIG_GEN + 8].copy_from_slice(&g.to_ne_bytes());
        assert!(matches!(
            parse(&b, Proto::Tcp),
            Err(PcbError::Changed { .. })
        ));
    }

    #[test]
    fn an_unknown_kind_fails_loudly() {
        let mut b = TCP.to_vec();
        let at = first(&b, XSO_STATS);
        put_u32(&mut b, at + 4, 0x400);
        let e = parse(&b, Proto::Tcp).unwrap_err();
        assert!(e.to_string().contains("unknown item kind 0x400"), "{e}");
    }

    #[test]
    fn a_grown_struct_fails_loudly() {
        let mut b = TCP.to_vec();
        let at = first(&b, XSO_SOCKET);
        put_u32(&mut b, at, 112);
        let e = parse(&b, Proto::Tcp).unwrap_err();
        assert!(e.to_string().contains("xsocket_n is 112 bytes"), "{e}");
    }

    #[test]
    fn a_truncated_buffer_fails_loudly() {
        let e = parse(&TCP[..TCP.len() - XINPGEN_LEN], Proto::Tcp).unwrap_err();
        assert!(matches!(e, PcbError::Layout(_)), "{e}");
        let e = parse(&TCP[..TCP.len() / 2], Proto::Tcp).unwrap_err();
        assert!(matches!(e, PcbError::Layout(_)), "{e}");
    }

    #[test]
    fn a_table_with_no_sockets_is_empty_not_an_error() {
        let mut b = TCP[..XINPGEN_LEN].to_vec();
        put_u32(&mut b, XIG_COUNT, 0);
        assert!(parse(&b, Proto::Tcp).unwrap().is_empty());
        assert!(parse(&b, Proto::Udp).unwrap().is_empty());
        put_u32(&mut b, XIG_COUNT, 3);
        assert!(matches!(parse(&b, Proto::Tcp), Err(PcbError::Layout(_))));
    }

    #[test]
    fn a_tcp_item_in_the_udp_table_fails_loudly() {
        assert!(matches!(parse(TCP, Proto::Udp), Err(PcbError::Layout(_))));
    }

    /// Listening sockets as `netstat -anv` prints them, in the form `listing` writes.
    fn netstat_listing(proto: Proto) -> Vec<String> {
        let out = std::process::Command::new("netstat")
            .args(["-anv", "-p", proto.name()])
            .output()
            .expect("netstat");
        let mut v = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let t: Vec<&str> = line.split_whitespace().collect();
            if t.len() < 8 || !t[0].starts_with(proto.name()) {
                continue;
            }
            let listening = match proto {
                Proto::Tcp => t[5] == "LISTEN",
                Proto::Udp => t[4] == "*.*",
            };
            let Some((_, port)) = t[3].rsplit_once('.') else {
                continue;
            };
            if !listening || port == "*" {
                continue;
            }
            let pid = t[5..]
                .iter()
                .find_map(|w| w.rsplit_once(':').and_then(|(_, p)| p.parse::<i32>().ok()))
                .expect("process:pid column");
            v.push(format!("{} {port} {pid}", t[0]));
        }
        v.sort();
        v
    }

    /// A neutral pid for a recorded one. System pids below 100 keep their number.
    fn neutral_pid(pids: &mut std::collections::HashMap<i32, i32>, pid: i32) -> i32 {
        if pid < 100 {
            return pid;
        }
        let next = 1000 + pids.len() as i32;
        *pids.entry(pid).or_insert(next)
    }

    /// Replaces addresses that are not loopback or wildcard with documentation addresses,
    /// renumbers pids and zeroes kernel handles, so a recorded table says nothing about the
    /// machine or network it came from.
    fn scrub(buf: &mut [u8], pids: &mut std::collections::HashMap<i32, i32>) {
        let mut off = XINPGEN_LEN;
        let mut n = 1u8;
        while u32_ne(buf, off) as usize > XINPGEN_LEN {
            let len = u32_ne(buf, off) as usize;
            let kind = u32_ne(buf, off + 4);
            let item = &mut buf[off..off + len];
            match kind {
                XSO_INPCB => {
                    item[8..16].fill(0);
                    item[20..28].fill(0);
                    let v4 = item[INP_VFLAG] & INP_IPV4 != 0;
                    for at in [INP_FADDR, INP_LADDR] {
                        if v4 {
                            let ip = Ipv4Addr::from(
                                <[u8; 4]>::try_from(&item[at + 12..at + 16]).unwrap(),
                            );
                            if !ip.is_loopback() && !ip.is_unspecified() {
                                item[at + 12..at + 16].copy_from_slice(&[192, 0, 2, n]);
                            }
                        } else {
                            let ip =
                                Ipv6Addr::from(<[u8; 16]>::try_from(&item[at..at + 16]).unwrap());
                            if !ip.is_loopback() && !ip.is_unspecified() {
                                let doc: Ipv6Addr = format!("2001:db8::{n}").parse().unwrap();
                                item[at..at + 16].copy_from_slice(&doc.octets());
                            }
                        }
                        n = n.wrapping_add(1).max(1);
                    }
                }
                XSO_SOCKET => {
                    item[8..16].fill(0);
                    item[28..36].fill(0);
                    for at in [SO_LAST_PID, SO_E_PID] {
                        let pid = neutral_pid(pids, i32_ne(item, at));
                        item[at..at + 4].copy_from_slice(&pid.to_ne_bytes());
                    }
                }
                XSO_TCPCB => item[8..16].fill(0),
                _ => {}
            }
            off += round8(len);
        }
    }

    /// Records the fixtures above: `SCRY_RECORD_DIR=fixtures/pcblist cargo test -p scry-core
    /// record_pcblist -- --ignored`. It retries until netstat, run between two reads, lists
    /// exactly the listeners the parser found in both.
    #[test]
    #[ignore]
    fn record_pcblist() {
        let dir =
            std::path::PathBuf::from(std::env::var("SCRY_RECORD_DIR").expect("SCRY_RECORD_DIR"));
        for _ in 0..20 {
            let (tcp, udp) = (read_raw(Proto::Tcp).unwrap(), read_raw(Proto::Udp).unwrap());
            let mut theirs = netstat_listing(Proto::Tcp);
            theirs.extend(netstat_listing(Proto::Udp));
            theirs.sort();
            let (t, u) = match (parse(&tcp, Proto::Tcp), parse(&udp, Proto::Udp)) {
                (Ok(t), Ok(u)) => (t, u),
                (t, u) => {
                    eprintln!("parse failed, retrying: {:?} {:?}", t.err(), u.err());
                    continue;
                }
            };
            let mut ours = listing(&t);
            ours.extend(listing(&u));
            ours.sort();
            if ours != theirs {
                eprintln!(
                    "differs, retrying: {:?}",
                    ours.iter()
                        .filter(|l| !theirs.contains(l))
                        .collect::<Vec<_>>()
                );
                continue;
            }
            let (mut tcp, mut udp) = (tcp, udp);
            let mut pids = std::collections::HashMap::new();
            scrub(&mut tcp, &mut pids);
            scrub(&mut udp, &mut pids);
            let mut renamed: Vec<String> = theirs
                .iter()
                .map(|l| {
                    let (rest, pid) = l.rsplit_once(' ').unwrap();
                    format!("{rest} {}", neutral_pid(&mut pids, pid.parse().unwrap()))
                })
                .collect();
            renamed.sort();
            std::fs::write(dir.join("tcp-macos26.bin"), &tcp).unwrap();
            std::fs::write(dir.join("udp-macos26.bin"), &udp).unwrap();
            std::fs::write(dir.join("listeners-macos26.txt"), renamed.join("\n") + "\n").unwrap();
            eprintln!(
                "recorded {} tcp, {} udp sockets, {} listening",
                t.len(),
                u.len(),
                theirs.len()
            );
            return;
        }
        panic!("netstat and the parser never agreed");
    }

    #[test]
    fn live_tables_parse() {
        let all = read_all().expect("live read");
        assert!(all.iter().any(|s| s.proto == Proto::Tcp));
        assert!(all.iter().any(|s| s.proto == Proto::Udp));
    }

    #[test]
    fn a_listener_this_test_opens_is_found_with_this_pid() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let me = std::process::id() as i32;
        let all = read(Proto::Tcp).unwrap();
        let s = all
            .iter()
            .find(|s| s.local_port == port && s.is_listening())
            .expect("own listener in the table");
        assert_eq!(s.pid, me);
        assert_eq!(s.local_ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(s.uid, unsafe { libc::getuid() });
        assert_eq!(pid_holds_port(me, Proto::Tcp, port), Some(true));
        assert_eq!(pid_holds_port(me, Proto::Udp, port), Some(false));
    }

    #[test]
    fn only_listening_sockets_count_as_held() {
        let me = std::process::id() as i32;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (accepted, _) = l.accept().unwrap();
        assert!(listening_ports(me).unwrap().contains(&(Proto::Tcp, port)));
        // The accepted connection has the same local port, but it is not a listener.
        drop(l);
        assert_eq!(pid_holds_port(me, Proto::Tcp, port), Some(false));
        let bound = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let connected = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        connected.connect(bound.local_addr().unwrap()).unwrap();
        let held = listening_ports(me).unwrap();
        assert!(held.contains(&(Proto::Udp, bound.local_addr().unwrap().port())));
        assert!(!held.contains(&(Proto::Udp, connected.local_addr().unwrap().port())));
        drop((client, accepted));
    }
}
