//! Command labels: what Scry shows for a command line, such as `next dev --port 3000`.
//!
//! No rule can find every secret in an arbitrary command line, so a label never copies one.
//! It is built only from parts known to be safe, with `…` wherever arguments were left out:
//!
//! - the program: the name of the executable the kernel runs, not `argv[0]`, which a process
//!   can rewrite. For an interpreter or shell, a launcher script in a `bin` directory takes
//!   its place, so `node …/.bin/next dev` reads `next dev`;
//! - words from [`WORDS`], unless they follow a flag, which may take them as its value;
//! - flag names, never their values. An argument right after a flag, shown or left out, may
//!   be that flag's value, as in `redis-cli -a --pass`, so a flag there is left out too.
//!   Python's own options are read as Python reads them, so `-W ignore` is a flag and its
//!   value, and `--` ends them;
//! - arguments that name an existing file or directory on a local volume, as their last
//!   component, and names of programs in the first [`MAX_PATH_DIRS`] directories of `PATH`;
//! - numbers equal to a port the process listens on;
//! - for Python, the module after `-m`. The code after `-c` or `-e` is always left out.
//!
//! A process can write a title over its arguments, as npm does with `npm run dev`. The title's
//! words are read as arguments, but a title has no argument boundaries, so a value with
//! spaces can hold words that look like flags: a title never shows a flag.
//!
//! A label shows at most [`MAX_SHOWN`] parts and reads at most [`MAX_ARGS`] arguments, each
//! once and without recursion, so building one takes time linear in the command line. Table
//! words and ports are checked before the file system, which is consulted only for the rest.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::home::Home;

/// Stands for one or more arguments that were left out.
pub const OMITTED: &str = "…";
/// The most parts a label shows, the program included.
pub const MAX_SHOWN: usize = 8;
/// Arguments after this many are left out unread, which bounds the file lookups per process.
pub const MAX_ARGS: usize = 64;
/// A longer argument is never looked up as a file.
const MAX_ARG: usize = 1024;
/// Programs are looked up in at most this many `PATH` directories.
pub const MAX_PATH_DIRS: usize = 64;
/// Symlinks followed in one lookup, as the kernel's own limit.
const MAX_LINKS: usize = 32;
/// Longer flag, program and module names are left out.
const MAX_NAME: usize = 64;

/// Subcommand words a label shows. Each is a common verb of a dev tool, and none takes a
/// secret as the argument after it. Add a word only after checking both.
pub const WORDS: [&str; 25] = [
    "agent",
    "app-server",
    "build",
    "compose",
    "console",
    "daemon",
    "dev",
    "develop",
    "exec",
    "http",
    "lsp",
    "mcp",
    "mcp-server",
    "preview",
    "run",
    "runserver",
    "serve",
    "server",
    "sse",
    "start",
    "stdio",
    "test",
    "up",
    "watch",
    "worker",
];

/// Programs whose first argument is a script. Versions such as `python3.12` match too.
const INTERPRETERS: [&str; 11] = [
    "bash", "bun", "deno", "java", "node", "perl", "php", "python", "ruby", "sh", "zsh",
];

/// Interpreter flags whose next argument is code.
const CODE_FLAGS: [&str; 5] = ["-c", "-e", "--eval", "-p", "--print"];

/// Python's one-letter options that take no value. `-c`, `-m`, `-W` and `-X` take one.
const PYTHON_FLAGS: &str = "bBdEhiIOPqRsSuvVx?";

/// Launcher scripts lose these extensions: `vite/bin/vite.js` reads `vite`.
const SCRIPT_EXTENSIONS: [&str; 3] = [".js", ".mjs", ".cjs"];

/// A command line reduced to safe parts. Serialized as its text, such as
/// `"docker exec … mysql …"`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandLabel {
    parts: Vec<String>,
}

/// The file system as labels see it. Tests stand in for it.
pub trait Files {
    /// Whether `path` names an existing file or directory.
    fn exists(&self, path: &Path) -> bool;
    /// Whether `name` is a program in a `PATH` directory.
    fn on_path(&self, name: &str) -> bool;
}

/// What a label may consult besides the arguments.
pub struct Facts<'a> {
    /// The executable the kernel runs, from `proc_pidpath`.
    pub exe: Option<&'a str>,
    /// The kernel's name for the process, used when the executable is unknown.
    pub name: &'a str,
    /// The working directory, which relative paths resolve against.
    pub cwd: Option<&'a Path>,
    /// An argument that names the home directory, by any name [`Home::is_home`] accepts,
    /// shows as `~`. An existing file, a launcher script, the program and a program on
    /// `PATH` whose name is the account name, in any case, are left out. The account name
    /// can still show as a table word, a flag name or a Python module.
    pub home: Option<&'a Home>,
    /// The ports the process listens on.
    pub ports: &'a [u16],
    pub files: &'a dyn Files,
}

/// What the argument before the current one says about it.
#[derive(Clone, Copy, PartialEq)]
enum Before {
    /// The program, an argument that is not a flag, `--`, or a flag with its value after `=`.
    Free,
    /// A flag, shown or left out, with no value after `=`: it may take the current argument
    /// as its value. `-xY` may be `-x` with `Y` attached or the flags `-x -Y`, so a left-out
    /// one counts as well.
    Flag,
    /// An interpreter's code flag: the current argument is code.
    Code,
}

impl CommandLabel {
    /// The label for a raw argument vector. Nothing from `argv` is copied into it except
    /// parts that pass the rules above.
    pub fn build(argv: &[String], f: &Facts) -> CommandLabel {
        let Arguments {
            words,
            unread,
            title,
        } = arguments(argv);
        let program = program_name(f);
        // Set while the arguments are the interpreter's own, up to its script, module or code.
        let mut interpreter = interpreter(&program);
        let mut b = Builder::default();
        let mut args = words.into_iter().peekable();
        match args
            .peek()
            .and_then(|first| interpreter.and_then(|_| launcher(first, f)))
        {
            Some(tool) => {
                interpreter = None;
                b.show(tool);
                args.next();
            }
            None if f.home.is_some_and(|h| h.is_account(&program)) => b.omit(),
            None => b.show(program),
        }
        let mut before = Before::Free;
        while let Some(a) = args.next() {
            if b.shown >= MAX_SHOWN {
                b.omit();
                break;
            }
            if before == Before::Code {
                interpreter = None;
                b.omit();
                before = Before::Free;
                continue;
            }
            if title && a.starts_with('-') {
                b.omit();
                before = Before::Flag;
                continue;
            }
            if interpreter == Some("python")
                && let Some(option) = python_option(a)
            {
                // Python's grammar is known, so what follows a known option is never taken
                // for its value by mistake.
                before = Before::Free;
                // A lone `-x` or a long name is shown; a cluster or an attached value is not.
                let named = a.len() == 2 || a == "--check-hash-based-pycs";
                if named {
                    b.show(a.to_string());
                } else {
                    b.omit();
                }
                match option {
                    Py::Flags => {}
                    Py::End => interpreter = None,
                    Py::Takes { value, attached } => {
                        if value != PyValue::Option {
                            interpreter = None;
                        }
                        if attached {
                            continue;
                        }
                        match args.next() {
                            Some(m) if value == PyValue::Module && named && is_module(m) => {
                                b.show(m.to_string())
                            }
                            Some(_) => b.omit(),
                            None => {}
                        }
                    }
                }
                continue;
            }
            before = if a == "--" {
                interpreter = None;
                b.show(a.to_string());
                Before::Free
            } else if a.starts_with('-') && a.len() > 1 {
                b.flag(a, before, f.ports, interpreter.is_some())
            } else {
                interpreter = None;
                b.word(a, before, f)
            };
        }
        if unread {
            b.omit();
        }
        b.label
    }

    /// The parts in order, with [`OMITTED`] where arguments were left out.
    pub fn parts(&self) -> &[String] {
        &self.parts
    }
}

#[derive(Default)]
struct Builder {
    label: CommandLabel,
    shown: usize,
}

impl Builder {
    fn show(&mut self, part: String) {
        if self.shown >= MAX_SHOWN {
            return self.omit();
        }
        self.label.parts.push(part);
        self.shown += 1;
    }

    fn omit(&mut self) {
        if self.label.parts.last().is_none_or(|p| p != OMITTED) {
            self.label.parts.push(OMITTED.to_string());
        }
    }

    /// `--name`, `--name=value`, `-x` or `-xvalue`. Only a name is ever shown.
    fn flag(&mut self, a: &str, before: Before, ports: &[u16], interpreter: bool) -> Before {
        if before == Before::Flag {
            self.omit();
            return Before::Flag;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            if !is_name(name) {
                self.omit();
                return Before::Flag;
            }
            return match value {
                None => {
                    self.show(a.to_string());
                    code_or_flag(a, interpreter)
                }
                Some(v) if is_port(v, ports) => {
                    self.show(a.to_string());
                    Before::Free
                }
                Some(_) => {
                    self.show(format!("--{name}"));
                    self.omit();
                    Before::Free
                }
            };
        }
        // `-pX` may be `-p` with the value `X` attached, so only a lone `-x` is a name.
        if a.len() == 2 && a.as_bytes()[1].is_ascii_alphanumeric() {
            self.show(a.to_string());
            return code_or_flag(a, interpreter);
        }
        self.omit();
        Before::Flag
    }

    /// Ports and table words first: they need no lookup, and a lookup can be slow.
    fn word(&mut self, a: &str, before: Before, f: &Facts) -> Before {
        let account = |name: &str| f.home.is_some_and(|h| h.is_account(name));
        if is_port(a, f.ports) || (before == Before::Free && WORDS.contains(&a)) {
            self.show(a.to_string());
        } else if let Some(path) = existing(a, f) {
            match file_name(&path, f.home) {
                Some(name) => self.show(name),
                None => self.omit(),
            }
        } else if is_program(a) && !account(a) && f.files.on_path(a) {
            self.show(a.to_string());
        } else {
            self.omit();
        }
        Before::Free
    }
}

fn code_or_flag(a: &str, interpreter: bool) -> Before {
    if interpreter && CODE_FLAGS.contains(&a) {
        Before::Code
    } else {
        Before::Flag
    }
}

/// What a label reads from an argument vector.
struct Arguments<'a> {
    /// Up to [`MAX_ARGS`] arguments after `argv[0]`, or words of a title.
    words: Vec<&'a str>,
    /// More were left unread.
    unread: bool,
    /// The words come from a title written over the arguments.
    title: bool,
}

fn arguments(argv: &[String]) -> Arguments<'_> {
    fn first<'a>(mut words: impl Iterator<Item = &'a str>, title: bool) -> Arguments<'a> {
        Arguments {
            words: words.by_ref().take(MAX_ARGS).collect(),
            unread: words.next().is_some(),
            title,
        }
    }
    match argv {
        [title] if title.contains(char::is_whitespace) && !title.starts_with('/') => {
            first(title.split_whitespace(), true)
        }
        [_, rest @ ..] => first(rest.iter().map(String::as_str), false),
        [] => first(std::iter::empty(), false),
    }
}

fn program_name(f: &Facts) -> String {
    let exe = f.exe.map(base).filter(|b| !b.is_empty()).unwrap_or(f.name);
    // Framework builds run `python3` as `….framework/Versions/3.x/…/Python.app/…/Python`.
    if exe == "Python" && f.exe.is_some_and(|e| e.contains(".framework/Versions/3.")) {
        return "python3".into();
    }
    printable(exe)
}

fn interpreter(program: &str) -> Option<&'static str> {
    let bare = program
        .to_ascii_lowercase()
        .trim_end_matches(|c: char| c.is_ascii_digit() || c == '.')
        .to_string();
    INTERPRETERS.iter().copied().find(|i| *i == bare)
}

/// A script that stands for the interpreter running it: a file in a `bin` or `.bin`
/// directory, or a program on `PATH` named by a title.
fn launcher(first: &str, f: &Facts) -> Option<String> {
    if first.starts_with('-') {
        return None;
    }
    let account = |name: &str| f.home.is_some_and(|h| h.is_account(name));
    if let Some(path) = existing(first, f) {
        let dir = path.parent()?.file_name()?;
        if dir != "bin" && dir != ".bin" {
            return None;
        }
        let name = path.file_name()?.to_string_lossy();
        let name = SCRIPT_EXTENSIONS
            .iter()
            .find_map(|ext| name.strip_suffix(ext))
            .unwrap_or(&name);
        return (!account(name)).then(|| printable(name));
    }
    (is_program(first) && !account(first) && f.files.on_path(first)).then(|| first.to_string())
}

/// Python's options as Python reads them, or `None` for an argument that is not one.
fn python_option(a: &str) -> Option<Py> {
    if a == "--" {
        return Some(Py::End);
    }
    if let Some(long) = a.strip_prefix("--") {
        let attached = match long.split_once('=') {
            Some((name, _)) => (name == "check-hash-based-pycs").then_some(true)?,
            None => (long == "check-hash-based-pycs").then_some(false)?,
        };
        return Some(Py::Takes {
            value: PyValue::Option,
            attached,
        });
    }
    let cluster = a.strip_prefix('-')?;
    for (i, c) in cluster.char_indices() {
        let value = match c {
            'c' => PyValue::Code,
            'm' => PyValue::Module,
            'W' | 'X' => PyValue::Option,
            c if PYTHON_FLAGS.contains(c) => continue,
            _ => return None,
        };
        return Some(Py::Takes {
            value,
            attached: i + 1 < cluster.len(),
        });
    }
    (!cluster.is_empty()).then_some(Py::Flags)
}

/// What one of Python's own options does with what follows it.
#[derive(Clone, Copy, PartialEq)]
enum Py {
    /// One or more options that take no value, as `-u` or `-uB`.
    Flags,
    /// `--`: the script comes next.
    End,
    /// An option with a value, attached as in `-Wignore` or in the next argument.
    Takes { value: PyValue, attached: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum PyValue {
    /// `-c`: the rest of the arguments belong to the code.
    Code,
    /// `-m`: the rest belong to the module.
    Module,
    /// `-W`, `-X` or `--check-hash-based-pycs`: more of Python's own options may follow.
    Option,
}

/// Whether `argv` is a Python multiprocessing child. The `spawn` start method runs
/// `python3 [flags] -c 'from multiprocessing.spawn import spawn_main; …' --multiprocessing-fork`,
/// and `forkserver` runs `-c 'from multiprocessing.forkserver import main; …'`, whose forks
/// keep those arguments. A `fork` child keeps its parent's arguments and so its label.
pub fn multiprocessing_worker(argv: &[String]) -> bool {
    let argv = &argv[..argv.len().min(MAX_ARGS + 1)];
    argv.iter().any(|a| a == "--multiprocessing-fork")
        || argv.windows(2).any(|w| {
            w[0] == "-c"
                && ["spawn import", "forkserver import"]
                    .iter()
                    .any(|m| w[1].starts_with(&format!("from multiprocessing.{m} ")))
        })
}

/// The path `a` names, when it exists. Relative paths resolve against the working directory.
fn existing(a: &str, f: &Facts) -> Option<PathBuf> {
    if a.is_empty() || a.len() > MAX_ARG {
        return None;
    }
    let p = Path::new(a);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        f.cwd?.join(p)
    };
    f.files.exists(&full).then_some(full)
}

/// The last component of an existing path, `~` for the home directory, or `None` when it is
/// the account name. `..` is resolved by name first, so `/Users/dev/acme/..` is home too.
fn file_name(path: &Path, home: Option<&Home>) -> Option<String> {
    if home.is_some_and(|h| h.is_home(path)) {
        return Some("~".into());
    }
    let mut resolved = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            c => resolved.push(c),
        }
    }
    let Some(name) = resolved.file_name() else {
        return Some("/".into());
    };
    let name = name.to_string_lossy();
    (!home.is_some_and(|h| h.is_account(&name))).then(|| printable(&name))
}

/// A port the process listens on, written as the port number exactly, so `03000` is not.
fn is_port(a: &str, ports: &[u16]) -> bool {
    a.len() <= 5
        && a.bytes().all(|b| b.is_ascii_digit())
        && a.parse::<u16>()
            .is_ok_and(|p| ports.contains(&p) && p.to_string() == a)
}

/// A flag name after its dashes: letters, digits, `-`, `_` and `.`.
fn is_name(s: &str) -> bool {
    s.len() <= MAX_NAME
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A word that could name a program on `PATH`.
fn is_program(s: &str) -> bool {
    s.len() <= MAX_NAME
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
}

/// A dotted Python module name, such as `http.server`.
fn is_module(s: &str) -> bool {
    s.len() <= MAX_NAME
        && s.split('.').all(|part| {
            part.bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `s` with every character that could drive a terminal or hide or reorder text replaced
/// by `?`. Text output passes everything read from the system through it.
pub fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if unprintable(c) { '?' } else { c })
        .collect()
}

/// A control character such as ESC, or an invisible or text-direction one such as U+202E.
pub fn unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
        )
}

impl fmt::Display for CommandLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.parts.join(" "))
    }
}

impl Serialize for CommandLabel {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// Reads a recorded label back. The text survives, made printable as a built label is; a
/// part that held a space, such as the file name `Code Helper`, comes back as two parts.
impl<'de> Deserialize<'de> for CommandLabel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Ok(CommandLabel {
            parts: text
                .split(' ')
                .filter(|p| !p.is_empty())
                .map(printable)
                .collect(),
        })
    }
}

/// The real file system, with programs looked up in the first [`MAX_PATH_DIRS`] directories
/// of this process's `PATH`. Only local volumes are consulted: a path is resolved one
/// component at a time, following symlinks, and counts as missing once it reaches a volume
/// the mount table does not mark local. Looking at a network or automounted volume can
/// block for as long as its server does not answer.
pub struct LiveFiles {
    path: Vec<PathBuf>,
    /// Mount points of volumes that are not local.
    remote: Vec<PathBuf>,
    /// What each path looked up so far is. The keys hold no symlinks.
    seen: RefCell<HashMap<PathBuf, Seen>>,
}

#[derive(Clone, Debug, PartialEq)]
enum Seen {
    Missing,
    Remote,
    Link(PathBuf),
    Is(Kind),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Dir,
    File,
    Other,
}

impl Default for LiveFiles {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveFiles {
    pub fn new() -> Self {
        LiveFiles::with(std::env::var_os("PATH"), remote_mounts())
    }

    fn with(path: Option<OsString>, remote: Vec<PathBuf>) -> Self {
        let path = path
            .map(|p| {
                std::env::split_paths(&p)
                    .filter(|d| d.is_absolute())
                    .take(MAX_PATH_DIRS)
                    .collect()
            })
            .unwrap_or_default();
        LiveFiles {
            path,
            remote,
            seen: RefCell::default(),
        }
    }

    /// Reads the mount table again and forgets what was looked up, as a new snapshot needs.
    pub fn refresh(&mut self) {
        self.remote = remote_mounts();
        self.seen.get_mut().clear();
    }

    /// The path of the file `path` names after every symlink and `..` in it, when it is an
    /// existing file on a local volume.
    pub fn real_file(&self, path: &Path) -> Option<PathBuf> {
        match self.resolve(path, true)? {
            (Kind::File, real) => Some(real),
            _ => None,
        }
    }

    /// What `path` is after its symlinks, or only those before its last component when
    /// `follow` is false. `None` when it is missing or reaches a volume that is not local.
    fn kind(&self, path: &Path, follow: bool) -> Option<Kind> {
        self.resolve(path, follow).map(|(kind, _)| kind)
    }

    /// [`LiveFiles::kind`], with the path it reached.
    fn resolve(&self, path: &Path, follow: bool) -> Option<(Kind, PathBuf)> {
        if !path.is_absolute() {
            return None;
        }
        let mut todo = Vec::new();
        push_components(&mut todo, path);
        let mut real = PathBuf::from("/");
        let mut kind = Kind::Dir;
        let mut links = 0;
        while let Some(name) = todo.pop() {
            if kind != Kind::Dir {
                return None;
            }
            if name == ".." {
                real.pop();
                continue;
            }
            let next = real.join(&name);
            match self.look(&next) {
                Seen::Missing | Seen::Remote => return None,
                Seen::Link(target) if follow || !todo.is_empty() => {
                    links += 1;
                    if links > MAX_LINKS {
                        return None;
                    }
                    if target.is_absolute() {
                        real = PathBuf::from("/");
                    }
                    push_components(&mut todo, &target);
                }
                Seen::Link(_) => return Some((Kind::Other, next)),
                Seen::Is(k) => {
                    real = next;
                    kind = k;
                }
            }
        }
        Some((kind, real))
    }

    /// What one component is, looked up once. `path`'s parent holds no symlinks.
    fn look(&self, path: &Path) -> Seen {
        if let Some(s) = self.seen.borrow().get(path) {
            return s.clone();
        }
        let s = if self.remote(path) {
            Seen::Remote
        } else {
            match std::fs::symlink_metadata(path) {
                Err(_) => Seen::Missing,
                Ok(m) if m.file_type().is_symlink() => {
                    std::fs::read_link(path).map_or(Seen::Missing, Seen::Link)
                }
                Ok(m) if m.is_dir() => Seen::Is(Kind::Dir),
                Ok(m) if m.is_file() => Seen::Is(Kind::File),
                Ok(_) => Seen::Is(Kind::Other),
            }
        };
        self.seen.borrow_mut().insert(path.to_path_buf(), s.clone());
        s
    }

    /// Whether `path` is on a volume that is not local, also through the data volume's
    /// firmlinks, as `/home` leads to `/System/Volumes/Data/home`.
    fn remote(&self, path: &Path) -> bool {
        let data = Path::new("/System/Volumes/Data").join(path.strip_prefix("/").unwrap_or(path));
        self.remote
            .iter()
            .any(|m| path.starts_with(m) || data.starts_with(m))
    }
}

/// `path`'s components, last first, so popping reads them in order.
fn push_components(todo: &mut Vec<OsString>, path: &Path) {
    for c in path.components().rev() {
        match c {
            Component::Normal(n) => todo.push(n.to_os_string()),
            Component::ParentDir => todo.push("..".into()),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
}

/// Mount points without `MNT_LOCAL`, read without waiting on any of them. If the table
/// cannot be read, every volume counts as local, as before Scry checked.
fn remote_mounts() -> Vec<PathBuf> {
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return Vec::new();
    }
    // Room for mounts added between the two calls.
    let cap = n as usize + 16;
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(cap);
    let size = (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), size, libc::MNT_NOWAIT) };
    if got <= 0 {
        return Vec::new();
    }
    unsafe { buf.set_len(got as usize) };
    buf.iter()
        .filter(|s| s.f_flags & libc::MNT_LOCAL as u32 == 0)
        .map(|s| {
            let name = unsafe { CStr::from_ptr(s.f_mntonname.as_ptr()) };
            PathBuf::from(name.to_string_lossy().into_owned())
        })
        .collect()
}

impl Files for LiveFiles {
    fn exists(&self, path: &Path) -> bool {
        self.kind(path, false).is_some()
    }

    fn on_path(&self, name: &str) -> bool {
        self.path
            .iter()
            .any(|d| self.kind(&d.join(name), true) == Some(Kind::File))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Disk(&'static [&'static str]);

    impl Files for Disk {
        fn exists(&self, path: &Path) -> bool {
            self.0.iter().any(|p| Path::new(p) == path)
        }

        fn on_path(&self, name: &str) -> bool {
            ["mysql", "npm"].contains(&name)
        }
    }

    const DISK: Disk = Disk(&[
        "/Users/dev",
        "/Users/dev/acme",
        "/Users/dev/acme/server.js",
        "/Users/dev/acme/bin/rails",
        "/Users/dev/acme/..",
        "/System/Volumes/Data/Users/dev",
        "/USERS/DEV",
        "/Volumes/Backup/USERS/Dev",
        "/Users/dev/acme/bin/dev",
    ]);

    fn dev() -> Home {
        Home::new(Path::new("/Users/dev")).unwrap()
    }

    fn label_on(files: &dyn Files, exe: &str, argv: &[&str], ports: &[u16]) -> String {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let home = dev();
        let facts = Facts {
            exe: Some(exe),
            name: "fallback",
            cwd: Some(Path::new("/Users/dev/acme")),
            home: Some(&home),
            ports,
            files,
        };
        CommandLabel::build(&argv, &facts).to_string()
    }

    fn label(exe: &str, argv: &[&str], ports: &[u16]) -> String {
        label_on(&DISK, exe, argv, ports)
    }

    #[test]
    fn home_by_any_name_is_tilde_and_the_account_name_never_shows() {
        let l = |argv: &[&str]| label("/bin/app", argv, &[]);
        assert_eq!(l(&["app", "/System/Volumes/Data/Users/dev"]), "app ~");
        assert_eq!(l(&["app", "/USERS/DEV"]), "app ~");
        assert_eq!(l(&["app", "/Volumes/Backup/USERS/Dev"]), "app …");
        // A launcher script and a program named like the account are left out too.
        assert_eq!(
            label("/usr/bin/ruby", &["ruby", "bin/dev"], &[]),
            "ruby …",
            "not the launcher `dev`"
        );
        assert_eq!(label("/Users/dev/bin/DEV", &["DEV", "x"], &[]), "…");
        assert_eq!(l(&["app", "dev"]), "app dev", "a table word is not a path");
    }

    #[test]
    fn python_reads_its_own_options_as_python_does() {
        let python = "/Library/Frameworks/Python.framework/Versions/3.12/Resources/Python.app/Contents/MacOS/Python";
        let l = |argv: &[&str]| label(python, argv, &[]);
        // `--` ends Python's options: what follows is a script and its arguments.
        assert_eq!(
            l(&["python3", "--", "-m", "http.server"]),
            "python3 -- -m …"
        );
        // `-W` and `-X` take a value and more options may follow.
        assert_eq!(
            l(&["python3", "-W", "ignore", "-m", "http.server"]),
            "python3 -W … -m http.server"
        );
        assert_eq!(
            l(&["python3", "-X", "dev", "-u", "-m", "http.server"]),
            "python3 -X … -u -m http.server"
        );
        assert_eq!(
            l(&["python3", "-Wignore", "-m", "http.server"]),
            "python3 … -m http.server"
        );
        assert_eq!(
            l(&[
                "python3",
                "--check-hash-based-pycs",
                "never",
                "-m",
                "http.server"
            ]),
            "python3 --check-hash-based-pycs … -m http.server"
        );
        // Code is left out however it is given, and ends Python's options.
        assert_eq!(l(&["python3", "-u", "-c", "3000"]), "python3 -u -c …");
        assert_eq!(l(&["python3", "-uc", "x", "dev"]), "python3 … dev");
        assert_eq!(l(&["python3", "-mhttp.server"]), "python3 …");
        // An option Python does not have is read as any flag is.
        assert_eq!(l(&["python3", "-p", "dev"]), "python3 -p …");
    }

    /// Answers like `DISK` and counts every lookup.
    struct Counted(std::cell::Cell<usize>);

    impl Files for Counted {
        fn exists(&self, path: &Path) -> bool {
            self.0.set(self.0.get() + 1);
            DISK.exists(path)
        }

        fn on_path(&self, name: &str) -> bool {
            self.0.set(self.0.get() + 1);
            DISK.on_path(name)
        }
    }

    #[test]
    fn table_words_and_ports_need_no_lookup() {
        let files = Counted(Default::default());
        let got = label_on(
            &files,
            "/bin/app",
            &["app", "run", "dev", "--port", "3000", "--watch"],
            &[3000],
        );
        assert_eq!(got, "app run dev --port 3000 --watch");
        assert_eq!(files.0.get(), 0);
        label_on(&files, "/bin/app", &["app", "server.js"], &[]);
        assert_eq!(files.0.get(), 1);
    }

    #[test]
    fn multiprocessing_children_are_workers() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(multiprocessing_worker(&argv(&[
            "python3",
            "-B",
            "-c",
            "from multiprocessing.spawn import spawn_main; spawn_main(tracker_fd=5, pipe_handle=7)",
            "--multiprocessing-fork",
        ])));
        assert!(multiprocessing_worker(&argv(&[
            "python3",
            "-c",
            "from multiprocessing.forkserver import main; main(3, 4, ['__main__'], **{})",
        ])));
        assert!(!multiprocessing_worker(&argv(&[
            "python3",
            "-c",
            "from multiprocessing.resource_tracker import main;main(5)",
        ])));
        assert!(!multiprocessing_worker(&argv(&[
            "python3", "-m", "uvicorn", "app:app"
        ])));
    }

    /// A directory tree for `LiveFiles`, removed afterwards.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let p = std::env::temp_dir().join(format!("scry-label-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tree(p.canonicalize().unwrap())
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_volume_that_is_not_local_is_never_looked_into() {
        let t = Tree::new("remote");
        let (local, nas) = (t.0.join("local"), t.0.join("nas"));
        for d in [&local, &nas] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("tool"), "").unwrap();
        }
        std::os::unix::fs::symlink(&nas, local.join("link")).unwrap();
        std::os::unix::fs::symlink("../nas/tool", local.join("rel")).unwrap();
        let path = std::env::join_paths([&nas, &local]).unwrap();
        // The mount table says `nas` is a network volume.
        let files = LiveFiles::with(Some(path.clone()), vec![nas.clone()]);
        assert!(files.exists(&local.join("tool")));
        assert!(!files.exists(&nas.join("tool")));
        assert!(!files.exists(&local.join("link/tool")), "through a symlink");
        assert!(!files.exists(&local.join("link/../nas/tool")));
        assert!(files.exists(&local.join("rel")), "the link itself is local");
        assert!(!files.exists(&local.join("rel/..")));
        assert!(files.on_path("tool"), "found in the local directory");
        let files = LiveFiles::with(Some(path), vec![nas.clone(), local.clone()]);
        assert!(!files.on_path("tool"));
        // With every volume local, the same paths are found.
        let files = LiveFiles::with(None, vec![]);
        assert!(files.exists(&local.join("link/tool")));
        assert!(files.exists(&local.join("./link/../local/tool")));
        assert!(
            !files.exists(&local.join("tool/x")),
            "a file has no children"
        );
        assert!(!files.exists(Path::new("relative")));
    }

    #[test]
    fn path_is_read_up_to_its_cap() {
        let dirs: Vec<String> = (0..100).map(|i| format!("/opt/d{i}")).collect();
        let files = LiveFiles::with(Some(dirs.join(":").into()), vec![]);
        assert_eq!(files.path.len(), MAX_PATH_DIRS);
        assert_eq!(files.path[0], Path::new("/opt/d0"));
    }

    #[test]
    fn the_mount_table_marks_automounts_as_not_local() {
        let remote = remote_mounts();
        assert!(!remote.iter().any(|m| m == Path::new("/")));
        assert!(
            !remote
                .iter()
                .any(|m| m == Path::new("/System/Volumes/Data"))
        );
        let files = LiveFiles::new();
        assert!(files.exists(Path::new("/usr/bin/true")));
        // `/home` leads into the `auto_home` automount macOS sets up.
        if remote
            .iter()
            .any(|m| m == Path::new("/System/Volumes/Data/home"))
        {
            assert!(!files.exists(Path::new("/home/dev")));
            assert!(!files.exists(Path::new("/System/Volumes/Data/home/dev")));
        }
    }

    #[test]
    fn a_number_shows_only_as_a_listening_port_written_exactly() {
        let l = |argv: &[&str]| label("/bin/app", argv, &[3000]);
        assert_eq!(l(&["app", "--port", "3000"]), "app --port 3000");
        assert_eq!(l(&["app", "--port=3000"]), "app --port=3000");
        assert_eq!(l(&["app", "--port", "3001"]), "app --port …");
        assert_eq!(l(&["app", "--port=3001"]), "app --port …");
        assert_eq!(l(&["app", "-p3000"]), "app …");
        assert_eq!(l(&["app", "03000"]), "app …");
        assert_eq!(l(&["app", "+3000"]), "app …");
    }

    #[test]
    fn files_show_their_last_component_and_home_shows_as_tilde() {
        let l = |argv: &[&str]| label("/bin/app", argv, &[]);
        assert_eq!(l(&["app", "server.js"]), "app server.js");
        assert_eq!(l(&["app", "/Users/dev/acme/server.js"]), "app server.js");
        assert_eq!(l(&["app", "/Users/dev"]), "app ~");
        assert_eq!(l(&["app", "/Users/dev/"]), "app ~");
        assert_eq!(l(&["app", "."]), "app acme");
        assert_eq!(l(&["app", ".."]), "app ~", "never the account name");
        assert_eq!(l(&["app", "missing.js"]), "app …");
    }

    #[test]
    fn omitted_runs_collapse_and_long_command_lines_stop_at_the_cap() {
        let l = |argv: &[&str]| label("/bin/app", argv, &[]);
        assert_eq!(l(&["app", "a", "b", "c", "dev", "d"]), "app … dev …");
        let many = [
            "app", "run", "dev", "start", "serve", "build", "watch", "test", "up", "exec",
        ];
        assert_eq!(l(&many), "app run dev start serve build watch test …");
        let python = "/Library/Frameworks/Python.framework/Versions/3.12/Resources/Python.app/Contents/MacOS/Python";
        let seven = [
            "python3", "--a=1", "--b=1", "--c=1", "--d=1", "--e=1", "--f=1", "-m", "mod", "dev",
        ];
        assert_eq!(
            label(python, &seven, &[]),
            "python3 --a … --b … --c … --d … --e … --f … -m …"
        );
        let mut module = vec!["python3", "-m", "mod"];
        module.extend(std::iter::repeat_n("x", MAX_ARGS));
        assert_eq!(label(python, &module, &[]), "python3 -m mod …");
        let mut long = vec!["app"];
        long.extend(std::iter::repeat_n("x", MAX_ARGS + 10));
        long.push("dev");
        assert_eq!(l(&long), "app …", "arguments past the limit are not read");
    }

    #[test]
    fn a_word_or_flag_right_after_a_flag_may_be_its_value() {
        let l = |argv: &[&str]| label("/bin/app", argv, &[]);
        assert_eq!(l(&["app", "-a", "dev"]), "app -a …");
        assert_eq!(l(&["app", "-a", "--pass"]), "app -a …");
        assert_eq!(l(&["app", "--sort-key", "--monkey"]), "app --sort-key …");
        assert_eq!(
            l(&["app", "--port", "--password", "--pass"]),
            "app --port …"
        );
        assert_eq!(l(&["app", "-p", "-p", "--pass", "dev"]), "app -p …");
        assert_eq!(l(&["app", "-vp", "--pass", "x", "dev"]), "app … dev");
        assert_eq!(l(&["app", "--monkey", "dev"]), "app --monkey …");
        assert_eq!(
            l(&["app", "--x=1", "dev", "--sort-key"]),
            "app --x … dev --sort-key"
        );
        assert_eq!(
            l(&["app", "a", "--port", "--", "dev"]),
            "app … --port -- dev"
        );
    }

    #[test]
    fn interpreters_name_the_script_module_or_code_flag() {
        let node = "/opt/homebrew/bin/node";
        assert_eq!(label(node, &["node", "server.js"], &[]), "node server.js");
        assert_eq!(label(node, &["node", "-e", "server.js"], &[]), "node -e …");
        assert_eq!(label(node, &["node", "-e", "3000"], &[3000]), "node -e …");
        assert_eq!(label(node, &["npm run dev"], &[]), "npm run dev");
        let ruby = "/usr/bin/ruby";
        assert_eq!(
            label(
                ruby,
                &["ruby", "bin/rails", "server", "-p", "3002"],
                &[3002]
            ),
            "rails server -p 3002"
        );
        let python = "/Library/Frameworks/Python.framework/Versions/3.12/Resources/Python.app/Contents/MacOS/Python";
        assert_eq!(
            label(python, &["python3", "-u", "-m", "http.server"], &[]),
            "python3 -u -m http.server"
        );
        assert_eq!(
            label(python, &["python3", "-m", "x;y"], &[]),
            "python3 -m …"
        );
        assert_eq!(
            label(python, &["python3", "-c", "dev"], &[]),
            "python3 -c …"
        );
        assert_eq!(
            label(
                python,
                &[
                    "/Library/Frameworks/Python.framework/Versions/3.12/Resources/Python.app/Contents/MacOS/Python"
                ],
                &[]
            ),
            "python3"
        );
        assert_eq!(label("/opt/app/Python", &["Python"], &[]), "Python");
    }

    #[test]
    fn the_program_is_the_executable_not_a_rewritten_argv0() {
        assert_eq!(label("/bin/zsh", &["-zsh"], &[]), "zsh");
        assert_eq!(label("/bin/app", &["hunter2-app"], &[]), "app");
        assert_eq!(label("/bin/app", &["app.secret"], &[]), "app");
        assert_eq!(label("/bin/app", &["app.98127634"], &[]), "app");
        assert_eq!(
            label(
                "/opt/homebrew/bin/nginx",
                &["nginx: master process nginx"],
                &[]
            ),
            "nginx …"
        );
    }

    #[test]
    fn control_characters_never_reach_a_terminal() {
        let argv = ["app".to_string()];
        let facts = Facts {
            exe: Some("/tmp/a\u{1b}[2Jb"),
            name: "x",
            cwd: None,
            home: None,
            ports: &[],
            files: &DISK,
        };
        assert_eq!(CommandLabel::build(&argv, &facts).to_string(), "a?[2Jb");
        assert_eq!(printable("srv\u{202E}sj.x\u{7}"), "srv?sj.x?");
        let back: CommandLabel = serde_json::from_str(r#""next\u001b]0;x\u0007 dev""#).unwrap();
        assert_eq!(back.to_string(), "next?]0;x? dev");
    }

    #[test]
    fn the_text_survives_a_round_trip_through_json() {
        let l = CommandLabel {
            parts: ["Code Helper", "--type", OMITTED]
                .map(String::from)
                .to_vec(),
        };
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(json, r#""Code Helper --type …""#);
        let back: CommandLabel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.to_string(), l.to_string());
    }

    #[test]
    fn without_an_executable_the_kernel_name_is_the_program() {
        let facts = Facts {
            exe: None,
            name: "launchd",
            cwd: None,
            home: None,
            ports: &[],
            files: &DISK,
        };
        assert_eq!(CommandLabel::build(&[], &facts).to_string(), "launchd");
    }
}
