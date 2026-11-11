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
//!   be that flag's value, as in `redis-cli -a --pass`, so a flag there is left out too;
//! - arguments that name an existing file or directory, as their last component, and names
//!   of programs on `PATH`;
//! - numbers equal to a port the process listens on;
//! - for Python, the module after `-m`. The code after `-c` or `-e` is always left out.
//!
//! A process can write a title over its arguments, as npm does with `npm run dev`. The title's
//! words are read as arguments, but a title has no argument boundaries, so a value with
//! spaces can hold words that look like flags: a title never shows a flag.
//!
//! A label shows at most [`MAX_SHOWN`] parts and reads at most [`MAX_ARGS`] arguments, each
//! once and without recursion, so building one takes time linear in the command line.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Stands for one or more arguments that were left out.
pub const OMITTED: &str = "…";
/// The most parts a label shows, the program included.
pub const MAX_SHOWN: usize = 8;
/// Arguments after this many are left out unread, which bounds the file lookups per process.
pub const MAX_ARGS: usize = 64;
/// A longer argument is never looked up as a file.
const MAX_ARG: usize = 1024;
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
    /// Shown as `~` when an argument names it, so a label never holds the account name.
    pub home: Option<&'a Path>,
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
            if interpreter == Some("python") && a == "-m" {
                interpreter = None;
                b.show(a.to_string());
                match args.next_if(|m| is_module(m)) {
                    Some(module) => {
                        b.show(module.to_string());
                        before = Before::Free;
                    }
                    None => before = Before::Flag,
                }
                continue;
            }
            before = if a == "--" {
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

    fn word(&mut self, a: &str, before: Before, f: &Facts) -> Before {
        if let Some(path) = existing(a, f) {
            self.show(file_name(&path, f.home));
        } else if is_port(a, f.ports)
            || (is_program(a) && f.files.on_path(a))
            || (before == Before::Free && WORDS.contains(&a))
        {
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
        return Some(printable(name));
    }
    (is_program(first) && f.files.on_path(first)).then(|| first.to_string())
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

/// The last component of an existing path, or `~` for the home directory. `..` is resolved
/// by name first, so `/Users/dev/acme/..` is home too and never shows the account name.
fn file_name(path: &Path, home: Option<&Path>) -> String {
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
    if home.is_some_and(|h| h == resolved) {
        return "~".into();
    }
    match resolved.file_name() {
        Some(n) => printable(&n.to_string_lossy()),
        None => "/".into(),
    }
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

/// The real file system, with programs looked up in this process's `PATH`.
pub struct LiveFiles {
    path: Vec<PathBuf>,
}

impl Default for LiveFiles {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveFiles {
    pub fn new() -> Self {
        let path = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .filter(|d| d.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        LiveFiles { path }
    }
}

impl Files for LiveFiles {
    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    fn on_path(&self, name: &str) -> bool {
        self.path.iter().any(|d| d.join(name).is_file())
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
    ]);

    fn label(exe: &str, argv: &[&str], ports: &[u16]) -> String {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let facts = Facts {
            exe: Some(exe),
            name: "fallback",
            cwd: Some(Path::new("/Users/dev/acme")),
            home: Some(Path::new("/Users/dev")),
            ports,
            files: &DISK,
        };
        CommandLabel::build(&argv, &facts).to_string()
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
