//! Removes secrets from command lines. Every command line Scry keeps or shows has passed
//! through [`argv`] first; raw arguments are never stored.
//!
//! The rules are data. `SECRET_PARTS` decides which names hold a secret, wherever the name
//! appears: a flag (`--password=x`, `--password x`), a variable (`MYSQL_PWD=x`), a header
//! (`Authorization: Bearer x`), a JSON or YAML key (`"password": "x"`) or a URL parameter
//! (`?access_token=x`). `SHORT_FLAGS` lists the one-letter flags that carry a password for
//! particular commands, `USER_PASSWORD_FLAGS` the flags that take `user:password`, and
//! `TOKEN_SHAPES` the token formats that are secret on sight. URL user info is masked too.
//!
//! An argument that holds spaces, such as the script in `sh -c '…'`, is split into words and
//! each word goes through the same rules. A word that looks like a path is never masked for
//! its shape alone, so `cargo run` and `go run` binaries keep their names.

use std::mem;
use std::ops::Range;

const MASK: &str = "***";
const QUOTES: [char; 2] = ['"', '\''];

/// How a secret word must sit in one part of a name.
#[derive(Clone, Copy)]
enum Fit {
    /// The part is the word.
    Whole,
    /// The part ends with the word: `requirepass`, `masterauth`, `apikey`.
    End,
    /// The part contains the word: `PGPASSWORD`, `keyStorePassword`.
    Within,
}

use Fit::{End, Whole, Within};

/// What makes a name secret. A name is split into lowercase parts at every character that is
/// not a letter or digit and at camelCase humps, so `MYSQL_PWD` is `mysql pwd` and `apiKey`
/// is `api key`. One matching part makes the name secret.
const SECRET_PARTS: [(&str, Fit); 17] = [
    ("password", Within),
    ("passwd", Within),
    ("passphrase", Within),
    ("secret", Within),
    ("credential", Within),
    ("cookie", Within),
    ("authorization", Within),
    // `--pass`, `--requirepass`; not `--passive`.
    ("pass", End),
    ("pwd", End),
    ("pw", Whole),
    // `access_token`, `authtoken`; not `--tokenizer` or `--max-tokens`.
    ("token", End),
    // `--api-key`, `apikey`, `AWS_SECRET_ACCESS_KEY`; not `--keyboard`.
    ("key", End),
    // `--auth`, `masterauth`; not `--author`.
    ("auth", End),
    ("cred", Whole),
    ("creds", Whole),
    ("sig", Whole),
    ("signature", Whole),
];

/// Parts that end like a secret word but are not secrets.
const NOT_SECRET_PARTS: [&str; 3] = ["askpass", "bypass", "compass"];

/// A last part that makes the value a place or a setting, not the secret itself:
/// `--key-file`, `SSH_AUTH_SOCK`, `--token-type`.
const ABOUT_PARTS: [&str; 9] = [
    "file", "path", "dir", "sock", "socket", "type", "id", "name", "size",
];

/// A first part that turns a flag off. It takes no value: `--no-password`.
const NEGATIONS: [&str; 4] = ["no", "without", "skip", "disable"];

/// A one-letter flag that carries a password for the commands listed.
struct ShortFlag {
    flag: &'static str,
    /// `-pSECRET`.
    attached: bool,
    /// `-p SECRET`.
    separate: bool,
    commands: &'static [&'static str],
}

const SHORT_FLAGS: [ShortFlag; 6] = [
    // A bare `-p` makes mysql prompt, so the word after it is not the password.
    ShortFlag {
        flag: "-p",
        attached: true,
        separate: false,
        commands: &[
            "mysql",
            "mysqldump",
            "mysqladmin",
            "mysqlimport",
            "mysqlcheck",
            "mysqlshow",
            "mariadb",
            "mariadb-dump",
            "mariadb-admin",
        ],
    },
    ShortFlag {
        flag: "-p",
        attached: true,
        separate: true,
        commands: &["sshpass"],
    },
    ShortFlag {
        flag: "-a",
        attached: false,
        separate: true,
        commands: &["redis-cli", "valkey-cli"],
    },
    ShortFlag {
        flag: "-p",
        attached: false,
        separate: true,
        commands: &[
            "mongo",
            "mongosh",
            "mongodump",
            "mongorestore",
            "mongoexport",
            "mongoimport",
        ],
    },
    ShortFlag {
        flag: "-P",
        attached: false,
        separate: true,
        commands: &["zip", "unzip"],
    },
    ShortFlag {
        flag: "-p",
        attached: true,
        separate: false,
        commands: &["7z", "7za", "7zz"],
    },
];

/// Flags whose value is `user:password`, as in `curl -u alice:secret`. The user stays.
const USER_PASSWORD_FLAGS: [&str; 4] = ["-u", "--user", "-U", "--proxy-user"];

/// Words that open an `Authorization` value. The credential is the word after.
const AUTH_SCHEMES: [&str; 5] = ["basic", "bearer", "bot", "digest", "token"];

/// Token formats that are secret by shape alone, as a prefix and the shortest whole token.
/// `?` in a prefix stands for any letter.
const TOKEN_SHAPES: [(&str, usize); 15] = [
    ("gh?_", 20),        // GitHub
    ("github_pat_", 30), // GitHub fine-grained
    ("glpat-", 20),      // GitLab
    ("sk-", 20),         // OpenAI, Anthropic
    ("sk_live_", 20),    // Stripe
    ("sk_test_", 20),    // Stripe
    ("rk_live_", 20),    // Stripe
    ("xox?-", 15),       // Slack
    ("xapp-", 15),       // Slack
    ("AKIA", 20),        // AWS access key
    ("ASIA", 20),        // AWS temporary access key
    ("AIza", 35),        // Google API key
    ("npm_", 36),        // npm
    ("hf_", 30),         // Hugging Face
    ("pypi-", 50),       // PyPI
];

/// Words that end a command inside a script, so the next word names a new one.
const SEPARATORS: [&str; 14] = [
    ";", "&&", "||", "|", "&", "|&", "!", "if", "then", "else", "elif", "do", "while", "until",
];

/// Commands that run the command after them.
const WRAPPERS: [&str; 8] = [
    "sudo", "doas", "env", "exec", "nohup", "nice", "time", "command",
];

/// Redacts an argument vector.
pub fn argv(args: &[String]) -> Vec<String> {
    let mut words = Words::new();
    args.iter().map(|a| words.word(a, true)).collect()
}

/// One line for display, quoting arguments that hold spaces. Line breaks and tabs inside an
/// argument are written as `\n`, `\r` and `\t`.
pub fn join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains(char::is_whitespace) {
                let a = a
                    .replace('\'', "'\\''")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t");
                format!("'{a}'")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the previous word says about the next one.
#[derive(Clone, Copy, Default, PartialEq)]
enum Expect {
    #[default]
    Nothing,
    /// After `--password`, `"password":` or `Bearer`.
    Secret,
    /// After `-u`.
    UserPassword,
}

/// Reads one command line, or one script, word by word.
struct Words {
    /// The command the current words belong to, such as `mysql`.
    command: String,
    /// The next word names a command: the first word, or one after `;`, `&&` or `|`.
    at_command: bool,
    expect: Expect,
}

impl Words {
    fn new() -> Words {
        Words {
            command: String::new(),
            at_command: true,
            expect: Expect::Nothing,
        }
    }

    /// `may_be_script` is set for whole arguments and quoted script words, whose spaces
    /// separate words of their own.
    fn word(&mut self, w: &str, may_be_script: bool) -> String {
        let w = &json_pairs(w);
        if SEPARATORS.contains(&w.as_str()) {
            self.expect = Expect::Nothing;
            self.at_command = true;
            return w.clone();
        }
        match mem::take(&mut self.expect) {
            Expect::Secret if takes_value(w) => {
                if is_auth_scheme(w) {
                    self.expect = Expect::Secret;
                    return w.clone();
                }
                return mask_value(w);
            }
            Expect::UserPassword if takes_value(w) => return user_password(w),
            _ => {}
        }
        self.note_command(w);
        if let Some(r) = self.flag(w) {
            return r;
        }
        if let Some(r) = assignment(w) {
            return r;
        }
        if let Some((r, expect)) = colon_name(w) {
            self.expect = expect;
            return r;
        }
        if may_be_script && w.contains(char::is_whitespace) {
            return script(w);
        }
        if w.eq_ignore_ascii_case("bearer") {
            self.expect = Expect::Secret;
            return w.clone();
        }
        shaped(w)
    }

    fn note_command(&mut self, w: &str) {
        if !self.at_command || w.starts_with('-') || WRAPPERS.contains(&w) {
            return;
        }
        if w.split_once('=').is_some_and(|(name, _)| is_name(name)) {
            return;
        }
        self.command = w.rsplit('/').next().unwrap_or(w).to_string();
        self.at_command = false;
    }

    /// A word that starts with `-`. `None` for any other word.
    fn flag(&mut self, w: &str) -> Option<String> {
        if !w.starts_with('-') || w.len() < 2 {
            return None;
        }
        for f in SHORT_FLAGS
            .iter()
            .filter(|f| f.commands.contains(&self.command.as_str()))
        {
            if f.separate && w == f.flag {
                self.expect = Expect::Secret;
                return Some(w.to_string());
            }
            if f.attached
                && let Some(v) = w.strip_prefix(f.flag)
                && !v.is_empty()
            {
                return Some(format!("{}{}", f.flag, mask_value(v)));
            }
        }
        if USER_PASSWORD_FLAGS.contains(&w) {
            self.expect = Expect::UserPassword;
            return Some(w.to_string());
        }
        let (name, value) = match w.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (w, None),
        };
        if let Some(v) = value
            && USER_PASSWORD_FLAGS.contains(&name)
        {
            return Some(format!("{name}={}", user_password(v)));
        }
        // `-ualice:secret`.
        if let Some(short @ ("-u" | "-U")) = w.get(..2)
            && value.is_none()
            && w.contains(':')
        {
            return Some(format!("{short}{}", user_password(&w[2..])));
        }
        let bare = name.trim_start_matches('-');
        let secret = is_name(bare) && secret_name(bare);
        Some(match value {
            None if secret => {
                self.expect = Expect::Secret;
                w.to_string()
            }
            None => w.to_string(),
            Some(v) if secret => format!("{name}={}", mask_secret(v)),
            Some(v) => format!("{name}={}", value_of(v)),
        })
    }
}

/// Redacts a script word by word, keeping its spacing and quotes.
fn script(text: &str) -> String {
    let mut words = Words::new();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for span in split_words(text) {
        out.push_str(&text[last..span.start]);
        let token = json_pairs(&text[span.clone()]);
        let (lead, core, trail, quoted) = peel(&token);
        if lead.contains(['(', '`', '{']) {
            words.at_command = true;
        }
        out.push_str(lead);
        out.push_str(&words.word(core, quoted));
        out.push_str(trail);
        if trail.contains([';', '&', '|']) {
            words.expect = Expect::Nothing;
            words.at_command = true;
        }
        last = span.end;
    }
    out.push_str(&text[last..]);
    out
}

/// The words of a script, split at whitespace outside quotes. With a quote left open, the
/// text splits at every space instead.
fn split_words(s: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => escaped = true,
            Some(_) => {}
            None if c.is_whitespace() => {
                if let Some(st) = start.take() {
                    spans.push(st..i);
                }
            }
            None => {
                start.get_or_insert(i);
                match c {
                    '"' | '\'' => quote = Some(c),
                    '\\' => escaped = true,
                    _ => {}
                }
            }
        }
    }
    if quote.is_some() {
        let base = s.as_ptr() as usize;
        return s
            .split_whitespace()
            .map(|w| {
                let at = w.as_ptr() as usize - base;
                at..at + w.len()
            })
            .collect();
    }
    if let Some(st) = start {
        spans.push(st..s.len());
    }
    spans
}

/// Splits shell punctuation off a script word: `"pw"},` is `"`, `pw` and `"},`. The flag is
/// set when the word is one quoted string, whose spaces then separate words of their own.
fn peel(t: &str) -> (&str, &str, &str, bool) {
    const LEAD: [char; 5] = ['(', '{', '[', '`', '$'];
    const TRAIL: [char; 8] = [')', '}', ']', ';', ',', '&', '|', '`'];
    let open = t.find(|c| !LEAD.contains(&c)).unwrap_or(t.len());
    let rest = &t[open..];
    if let Some(q) = rest.chars().next().filter(|c| QUOTES.contains(c))
        && let Some(close) = closing_quote(rest, q)
        && rest[close + 1..].chars().all(|c| TRAIL.contains(&c))
    {
        return (&t[..open + 1], &rest[1..close], &rest[close..], true);
    }
    let start = t
        .find(|c| !LEAD.contains(&c) && !QUOTES.contains(&c))
        .unwrap_or(t.len());
    let end = t[start..]
        .char_indices()
        .rev()
        .find(|&(_, c)| !TRAIL.contains(&c) && !QUOTES.contains(&c))
        .map_or(start, |(i, c)| start + i + c.len_utf8());
    (&t[..start], &t[start..end], &t[end..], false)
}

/// Where the quote that opens `s` closes.
fn closing_quote(s: &str, q: char) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in s.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if c == '\\' && q == '"' {
            escaped = true;
        } else if c == q {
            return Some(i);
        }
    }
    None
}

/// The value of a flag or variable whose name is not secret. It may still hold a script, a
/// URL, another assignment or a token.
fn value_of(v: &str) -> String {
    if v.contains(char::is_whitespace) {
        return script(v);
    }
    let v = json_pairs(v);
    if let Some(r) = assignment(&v) {
        return r;
    }
    if let Some((r, _)) = colon_name(&v) {
        return r;
    }
    shaped(&v)
}

/// `NAME=value`, or a secret name at the end of other text, as in a form body
/// `a=1&password=x` or `//registry.npmjs.org/:_authToken=x`.
fn assignment(w: &str) -> Option<String> {
    let (name, v) = w.split_once('=')?;
    if name.starts_with('-') {
        return None;
    }
    if is_name(name) {
        return Some(if secret_name(name) {
            format!("{name}={}", mask_secret(v))
        } else {
            format!("{name}={}", value_of(v))
        });
    }
    if w.contains("://") || name.contains(char::is_whitespace) {
        return None;
    }
    let key = &name[name.trim_end_matches(is_name_char).len()..];
    (!key.is_empty() && secret_name(key)).then(|| format!("{name}={}", mask_secret(v)))
}

/// `password:`, `password:x`, `"password":` or `Authorization: Bearer x`, with what the next
/// word must be.
fn colon_name(w: &str) -> Option<(String, Expect)> {
    let (name, v) = w.split_once(':')?;
    let bare = name.trim_matches(QUOTES);
    if !is_name(bare) || !secret_name(bare) {
        return None;
    }
    let text = v.trim_start();
    let space = &v[..v.len() - text.len()];
    let (first, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    Some(if text.trim_matches(QUOTES).is_empty() {
        (w.to_string(), Expect::Secret)
    } else if is_masked(text) {
        (w.to_string(), Expect::Nothing)
    } else if is_auth_scheme(first) && rest.trim().is_empty() {
        (w.to_string(), Expect::Secret)
    } else if is_auth_scheme(first) {
        (format!("{name}:{space}{first} {MASK}"), Expect::Nothing)
    } else {
        (
            format!("{name}:{space}{}", mask_secret(text)),
            Expect::Nothing,
        )
    })
}

/// URL user info and query parameters, `user:password@host` without a scheme, and tokens
/// known by their shape.
fn shaped(w: &str) -> String {
    let w = url(w).unwrap_or_else(|| w.to_string());
    let w = bare_userinfo(&w).unwrap_or(w);
    if path_like(&w) {
        return w;
    }
    if token_shaped(&w) {
        return MASK.to_string();
    }
    match w.rsplit_once(':') {
        Some((head, tail)) if token_shaped(tail) => format!("{head}:{MASK}"),
        _ => w,
    }
}

/// `scheme://user:password@host/path?token=x`.
fn url(w: &str) -> Option<String> {
    let start = w.find("://")? + 3;
    let rest = &w[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = match rest[..end].rfind('@') {
        Some(at) => format!("{}{}", userinfo(&rest[..at]), &rest[at..end]),
        None => rest[..end].to_string(),
    };
    let out = format!("{}{authority}{}", &w[..start], query(&rest[end..]));
    (out != w).then_some(out)
}

/// The path, query and fragment of a URL, with secret query parameters masked.
fn query(tail: &str) -> String {
    let Some(q) = tail.find('?') else {
        return tail.to_string();
    };
    let (path, rest) = tail.split_at(q + 1);
    let (params, fragment) = rest.split_at(rest.find('#').unwrap_or(rest.len()));
    let params: Vec<String> = params
        .split('&')
        .map(|p| match p.split_once('=') {
            Some((k, v)) if !v.is_empty() && secret_name(k) => format!("{k}={MASK}"),
            _ => p.to_string(),
        })
        .collect();
    format!("{path}{}{fragment}", params.join("&"))
}

/// The part of a URL before `@`. The password goes. The user stays when it reads as an
/// account name; a token in its place, as in `https://<token>:x-oauth-basic@host`, goes too.
fn userinfo(info: &str) -> String {
    let (user, password) = match info.split_once(':') {
        Some((u, p)) => (u, Some(p)),
        None => (info, None),
    };
    let user = if account_name(user) { user } else { MASK };
    match password {
        Some(_) => format!("{user}:{MASK}"),
        None => user.to_string(),
    }
}

fn account_name(u: &str) -> bool {
    u.len() <= 16
        && u.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        && !token_shaped(u)
}

/// `user:password@host` with no scheme, as in Go's MySQL DSN `root:pw@tcp(db:3306)/app`.
fn bare_userinfo(w: &str) -> Option<String> {
    if w.contains("://") {
        return None;
    }
    let at = w.find('@')?;
    let info = &w[..at];
    let (_, password) = info.split_once(':')?;
    if password.is_empty() || password == MASK || info.contains(['/', '=']) {
        return None;
    }
    Some(format!("{}{}", userinfo(info), &w[at..]))
}

/// `user:password` as `-u` takes it. `uid:gid` pairs such as `1000:1000` stay, and a URL
/// goes through the URL rules.
fn user_password(v: &str) -> String {
    if v.contains("://") {
        return shaped(v);
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match v.split_once(':') {
        Some((user, password))
            if !password.is_empty() && password != MASK && !(digits(user) && digits(password)) =>
        {
            format!("{user}:{MASK}")
        }
        _ => v.to_string(),
    }
}

/// Masks `"key": "value"` and `'key': value` pairs with secret keys inside one word or
/// argument.
fn json_pairs(s: &str) -> String {
    if !s.contains(':') || !s.contains(QUOTES) {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        let q = b[i];
        if q != b'"' && q != b'\'' {
            i += 1;
            continue;
        }
        let Some(len) = s[i + 1..].find(q as char) else {
            break;
        };
        let name_end = i + 1 + len;
        let name = &s[i + 1..name_end];
        let mut j = name_end + 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() || b[j] != b':' || !is_name(name) || !secret_name(name) {
            i = name_end + 1;
            continue;
        }
        j += 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let (start, end) = if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
            let mut k = j + 1;
            while k < b.len() && b[k] != b[j] {
                k += if b[k] == b'\\' { 2 } else { 1 };
            }
            (j + 1, k.min(b.len()))
        } else {
            let k = s[j..]
                .find(|c: char| c == ',' || c == '}' || c == ']' || c.is_whitespace())
                .map_or(b.len(), |n| j + n);
            (j, k)
        };
        if end > start && &s[start..end] != MASK {
            out.push_str(&s[copied..start]);
            out.push_str(MASK);
            copied = end;
        }
        i = end.max(name_end + 1);
    }
    out.push_str(&s[copied..]);
    out
}

/// Whether a flag, variable, header, key or query parameter name holds a secret.
fn secret_name(name: &str) -> bool {
    let parts = name_parts(name);
    let (Some(first), Some(last)) = (parts.first(), parts.last()) else {
        return false;
    };
    if NEGATIONS.contains(&first.as_str()) || ABOUT_PARTS.contains(&last.as_str()) {
        return false;
    }
    parts.iter().any(|p| {
        !NOT_SECRET_PARTS.contains(&p.as_str())
            && SECRET_PARTS.iter().any(|&(word, fit)| match fit {
                Whole => p == word,
                End => p.ends_with(word),
                Within => p.contains(word),
            })
    })
}

/// `X-Amz-Security-Token` is `x amz security token`; `keyStorePassword` is
/// `key store password`.
fn name_parts(name: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut after_lower = false;
    for c in name.chars() {
        let hump = c.is_ascii_uppercase() && after_lower;
        if (!c.is_ascii_alphanumeric() || hump) && !part.is_empty() {
            parts.push(mem::take(&mut part));
        }
        if c.is_ascii_alphanumeric() {
            part.push(c.to_ascii_lowercase());
        }
        after_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_name_char)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_-.".contains(c)
}

fn is_auth_scheme(w: &str) -> bool {
    AUTH_SCHEMES.iter().any(|s| w.eq_ignore_ascii_case(s))
}

/// Whether a word can be the value a secret flag announced. A flag cannot, and neither can
/// a place: `--key ~/.ssh/id` shows the path.
fn takes_value(w: &str) -> bool {
    !w.is_empty() && !(w.starts_with('-') && w.len() > 1) && !is_location(w)
}

/// A value that starts with `/`, `~` or `.` names a file, as in `SSH_AUTH_SOCK=/private/…`.
fn is_location(v: &str) -> bool {
    v.trim_start_matches(QUOTES).starts_with(['/', '~', '.'])
}

/// A word that contains `/` or starts with `~` or `.` is a path, never a token.
fn path_like(w: &str) -> bool {
    w.contains('/') || w.starts_with(['~', '.'])
}

fn is_masked(v: &str) -> bool {
    v.trim_matches(QUOTES) == MASK
}

/// The value of a secret name, unless it is empty, a place or already masked.
fn mask_secret(v: &str) -> String {
    if v.trim_matches(QUOTES).is_empty() || is_location(v) || is_masked(v) {
        v.to_string()
    } else {
        mask_value(v)
    }
}

/// Replaces a value and keeps its quotes: `"abc"` becomes `"***"`.
fn mask_value(v: &str) -> String {
    let start = v.len() - v.trim_start_matches(QUOTES).len();
    let end = v.trim_end_matches(QUOTES).len();
    if end <= start {
        return v.to_string();
    }
    format!("{}{MASK}{}", &v[..start], &v[end..])
}

fn token_shaped(w: &str) -> bool {
    let tokenish = w
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let prefixed = TOKEN_SHAPES
        .iter()
        .any(|&(prefix, min)| w.len() >= min && starts_like(w, prefix));
    (tokenish && prefixed) || jwt(w) || random(w)
}

fn starts_like(w: &str, prefix: &str) -> bool {
    w.len() >= prefix.len()
        && w.bytes()
            .zip(prefix.bytes())
            .all(|(c, p)| c == p || (p == b'?' && c.is_ascii_alphabetic()))
}

/// A JSON Web Token: three base64url parts, the first a JSON object.
fn jwt(w: &str) -> bool {
    let parts: Vec<&str> = w.split('.').collect();
    w.starts_with("eyJ")
        && w.len() >= 30
        && parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

/// 32 or more base64 characters that mix upper case, lower case and digits. Hex digests and
/// UUIDs are one case, and kebab-case names hold `-`, so they stay readable.
fn random(w: &str) -> bool {
    let body = w.trim_end_matches('=');
    body.len() >= 32
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'_')
        && body.bytes().any(|b| b.is_ascii_uppercase())
        && body.bytes().any(|b| b.is_ascii_lowercase())
        && body.bytes().any(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rule above, as an argument vector and what Scry keeps of it. Tokens that look
    /// real are split with `concat!` so `scripts/check-public.sh` does not flag this file.
    const CASES: &[(&[&str], &[&str])] = &[
        // Flags with secret names, with `=` and with a separate value.
        (
            &["psql", "--password=hunter2", "--port=5432"],
            &["psql", "--password=***", "--port=5432"],
        ),
        (
            &["gh", "--token", "abc123", "--repo", "x"],
            &["gh", "--token", "***", "--repo", "x"],
        ),
        (
            &["x", "--api-key=k", "--client-secret", "s"],
            &["x", "--api-key=***", "--client-secret", "***"],
        ),
        (&["x", "--pass=hunter2"], &["x", "--pass=***"]),
        (&["x", "--auth", "admin:pw"], &["x", "--auth", "***"]),
        (
            &["redis-server", "--requirepass", "pw"],
            &["redis-server", "--requirepass", "***"],
        ),
        (
            &["redis-server", "--masterauth=pw"],
            &["redis-server", "--masterauth=***"],
        ),
        (&["x", "--password", "a b c"], &["x", "--password", "***"]),
        (&["x", "--password=a b"], &["x", "--password=***"]),
        (
            &["java", "-Dspring.datasource.password=pw"],
            &["java", "-Dspring.datasource.password=***"],
        ),
        (
            &["server", "-password", "pw"],
            &["server", "-password", "***"],
        ),
        (&["x", "--accessToken=abc"], &["x", "--accessToken=***"]),
        // One-letter flags, for the commands that use them for passwords.
        (
            &["mysql", "-uroot", "-pHunter2pw", "app"],
            &["mysql", "-uroot", "-p***", "app"],
        ),
        (
            &["/opt/homebrew/bin/mysql", "-p3306"],
            &["/opt/homebrew/bin/mysql", "-p***"],
        ),
        (
            &["sshpass", "-p", "pw", "ssh", "host"],
            &["sshpass", "-p", "***", "ssh", "host"],
        ),
        (
            &["redis-cli", "-a", "pw", "ping"],
            &["redis-cli", "-a", "***", "ping"],
        ),
        (
            &["mongosh", "-u", "admin", "-p", "pw"],
            &["mongosh", "-u", "admin", "-p", "***"],
        ),
        (
            &["zip", "-P", "pw", "out.zip"],
            &["zip", "-P", "***", "out.zip"],
        ),
        // `user:password` flags.
        (
            &["curl", "-u", "alice:hunter2", "https://example.com"],
            &["curl", "-u", "alice:***", "https://example.com"],
        ),
        (
            &["curl", "--user=alice:hunter2"],
            &["curl", "--user=alice:***"],
        ),
        (&["curl", "-ualice:hunter2"], &["curl", "-ualice:***"]),
        (
            &["curl", "--proxy-user", "bob:pw"],
            &["curl", "--proxy-user", "bob:***"],
        ),
        // Variables.
        (
            &[
                "env",
                "GITHUB_TOKEN=abc",
                "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                "PORT=3000",
            ],
            &[
                "env",
                "GITHUB_TOKEN=***",
                "AWS_SECRET_ACCESS_KEY=***",
                "PORT=3000",
            ],
        ),
        (&["MYSQL_PWD=hunter2"], &["MYSQL_PWD=***"]),
        (&["PGPASSWORD=pw"], &["PGPASSWORD=***"]),
        (&["DB_PASSWD=p"], &["DB_PASSWD=***"]),
        (
            &["docker", "run", "-e", "API_KEY=k", "img"],
            &["docker", "run", "-e", "API_KEY=***", "img"],
        ),
        (
            &["docker", "run", "--env=NPM_TOKEN=t", "img"],
            &["docker", "run", "--env=NPM_TOKEN=***", "img"],
        ),
        (
            &[
                "npm",
                "config",
                "set",
                "//registry.npmjs.org/:_authToken=abc",
            ],
            &[
                "npm",
                "config",
                "set",
                "//registry.npmjs.org/:_authToken=***",
            ],
        ),
        (
            &["curl", "-d", "user=a&password=pw"],
            &["curl", "-d", "user=a&password=***"],
        ),
        (
            &[
                "git",
                "-c",
                "http.extraHeader=Authorization: Basic abc",
                "pull",
            ],
            &[
                "git",
                "-c",
                "http.extraHeader=Authorization: Basic ***",
                "pull",
            ],
        ),
        // URL user info and query parameters.
        (
            &["psql", "postgres://dev:s3cret@example.com:5432/app"],
            &["psql", "postgres://dev:***@example.com:5432/app"],
        ),
        (
            &[
                "git",
                "clone",
                concat!(
                    "https://ghp",
                    "_0123456789abcdefABCDEF:x-oauth-basic@example.com/a/b.git"
                ),
            ],
            &["git", "clone", "https://***:***@example.com/a/b.git"],
        ),
        (
            &[
                "git",
                "clone",
                "https://0123456789abcdef0123456789abcdef01234567@example.com/a/b.git",
            ],
            &["git", "clone", "https://***@example.com/a/b.git"],
        ),
        (
            &[
                "git",
                "push",
                "https://x-access-token:abc@example.com/a/b.git",
            ],
            &[
                "git",
                "push",
                "https://x-access-token:***@example.com/a/b.git",
            ],
        ),
        (
            &["redis-cli", "-u", "redis://:pw@cache:6379"],
            &["redis-cli", "-u", "redis://:***@cache:6379"],
        ),
        (
            &[
                "curl",
                "https://example.com/cb?access_token=hunter2&page=2#top",
            ],
            &["curl", "https://example.com/cb?access_token=***&page=2#top"],
        ),
        (
            &[
                "curl",
                "https://example.com/f?sv=2024&sig=abc&X-Amz-Signature=def",
            ],
            &[
                "curl",
                "https://example.com/f?sv=2024&sig=***&X-Amz-Signature=***",
            ],
        ),
        (
            &["app", "DATABASE_URL=postgres://app:pw@db:5432/app"],
            &["app", "DATABASE_URL=postgres://app:***@db:5432/app"],
        ),
        (
            &["app", "--dsn=root:pw@tcp(db:3306)/app"],
            &["app", "--dsn=root:***@tcp(db:3306)/app"],
        ),
        // Headers.
        (
            &["curl", "-H", "Authorization: Bearer abc.def"],
            &["curl", "-H", "Authorization: Bearer ***"],
        ),
        (
            &["curl", "-H", "X-Api-Key: abc"],
            &["curl", "-H", "X-Api-Key: ***"],
        ),
        (
            &["curl", "-H", "Cookie:session=abc"],
            &["curl", "-H", "Cookie:***"],
        ),
        // JSON and YAML.
        (
            &["curl", "-d", r#"{"password":"pw"}"#],
            &["curl", "-d", r#"{"password":"***"}"#],
        ),
        (
            &["curl", "-d", r#"{"user": "a", "password": "a b", "n": 1}"#],
            &["curl", "-d", r#"{"user": "a", "password": "***", "n": 1}"#],
        ),
        (&["x", "{'api_key': 'k'}"], &["x", "{'api_key': '***'}"]),
        (
            &["x", "--set", "password: pw"],
            &["x", "--set", "password: ***"],
        ),
        // Known token shapes.
        (
            &["tool", concat!("ghp", "_0123456789abcdefABCDEF")],
            &["tool", "***"],
        ),
        (
            &["tool", concat!("github", "_pat_11ABCDEFG0123456789abcd")],
            &["tool", "***"],
        ),
        (
            &["tool", concat!("sk", "-ant-api03-abcdefg")],
            &["tool", "***"],
        ),
        (
            &["tool", concat!("AKIA", "IOSFODNN7EXAMPLE")],
            &["tool", "***"],
        ),
        (
            &["tool", concat!("xoxb", "-0123456789-abcdef")],
            &["tool", "***"],
        ),
        (
            &["t", "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl"],
            &["t", "***"],
        ),
        (&["t", "Zx8Qp2Lm9Rt4Vb7Nc1Kd6Hf3Gj5Ws0Ya"], &["t", "***"]),
        (&["t", "Bearer", "abc"], &["t", "Bearer", "***"]),
        (
            &["t", concat!("--github=ghp", "_0123456789abcdefABCDEF")],
            &["t", "--github=***"],
        ),
        // Scripts: every word of an argument with spaces goes through the same rules.
        (
            &["sh", "-c", "mysql --password=Hunter2pw -h db"],
            &["sh", "-c", "mysql --password=*** -h db"],
        ),
        (
            &["sh", "-c", "sleep 600; mysql --password=Hunter2pw"],
            &["sh", "-c", "sleep 600; mysql --password=***"],
        ),
        (
            &[
                "/bin/zsh",
                "-c",
                "cd app && PGPASSWORD=pw psql -h db; mysql -pHunter2pw app",
            ],
            &[
                "/bin/zsh",
                "-c",
                "cd app && PGPASSWORD=*** psql -h db; mysql -p*** app",
            ],
        ),
        (
            &["sh", "-c", "bash -c \"mysql -pHunter2pw -e 'select 1'\""],
            &["sh", "-c", "bash -c \"mysql -p*** -e 'select 1'\""],
        ),
        (
            &["sh", "-c", "mysql --password=\"a b\" app"],
            &["sh", "-c", "mysql --password=\"***\" app"],
        ),
        (
            &[
                "sh",
                "-c",
                "curl -u alice:hunter2 -H 'Authorization: Bearer abc' -d '{\"token\":\"t\"}' x",
            ],
            &[
                "sh",
                "-c",
                "curl -u alice:*** -H 'Authorization: Bearer ***' -d '{\"token\":\"***\"}' x",
            ],
        ),
        (
            &["sh", "-c", "echo 'unclosed --token abc"],
            &["sh", "-c", "echo 'unclosed --token ***"],
        ),
        // Paths are never masked for their shape, even long mixed-case ones.
        (
            &[
                "/Users/dev/acme/target/debug/devServer0123456789ABCDEFghij",
                "--port",
                "3000",
            ],
            &[
                "/Users/dev/acme/target/debug/devServer0123456789ABCDEFghij",
                "--port",
                "3000",
            ],
        ),
        (
            &["/var/folders/ab/Xyz0123456789AbCdEfGhIjKlMn/T/go-build1234567890/b001/exe/main"],
            &["/var/folders/ab/Xyz0123456789AbCdEfGhIjKlMn/T/go-build1234567890/b001/exe/main"],
        ),
        (
            &["x", "./Zx8Qp2Lm9Rt4Vb7Nc1Kd6Hf3Gj5Ws0Ya"],
            &["x", "./Zx8Qp2Lm9Rt4Vb7Nc1Kd6Hf3Gj5Ws0Ya"],
        ),
        (
            &["x", "--key", "~/.ssh/id_ed25519"],
            &["x", "--key", "~/.ssh/id_ed25519"],
        ),
        (
            &["x", "--key-file", "/etc/k.pem"],
            &["x", "--key-file", "/etc/k.pem"],
        ),
        (
            &["x", "--password-file=/run/secrets/db"],
            &["x", "--password-file=/run/secrets/db"],
        ),
        (
            &["SSH_AUTH_SOCK=/private/tmp/agent/Listeners"],
            &["SSH_AUTH_SOCK=/private/tmp/agent/Listeners"],
        ),
        (
            &["GIT_ASKPASS=/usr/bin/true", "PWD=/Users/dev/acme"],
            &["GIT_ASKPASS=/usr/bin/true", "PWD=/Users/dev/acme"],
        ),
        // Flags that turn a secret off, and flags that only look like secrets.
        (
            &["mysql", "--no-password", "app"],
            &["mysql", "--no-password", "app"],
        ),
        (&["x", "--skip-auth", "app"], &["x", "--skip-auth", "app"]),
        (
            &["git", "log", "--author=dev"],
            &["git", "log", "--author=dev"],
        ),
        (
            &["llm", "--max-tokens", "4096", "--tokenizer", "bert"],
            &["llm", "--max-tokens", "4096", "--tokenizer", "bert"],
        ),
        (
            &["x", "--keyboard", "us", "--passive", "on"],
            &["x", "--keyboard", "us", "--passive", "on"],
        ),
        (
            &["x", "--token-type", "bearer"],
            &["x", "--token-type", "bearer"],
        ),
        // Ports, users and ids.
        (
            &["node", "server.js", "-p", "3000", "-p3001"],
            &["node", "server.js", "-p", "3000", "-p3001"],
        ),
        (
            &["mysql", "-p", "app", "-P", "3306"],
            &["mysql", "-p", "app", "-P", "3306"],
        ),
        (
            &["python3", "-u", "server.py"],
            &["python3", "-u", "server.py"],
        ),
        (
            &["docker", "run", "-u", "1000:1000", "-p", "8080:80"],
            &["docker", "run", "-u", "1000:1000", "-p", "8080:80"],
        ),
        (
            &["psql", "-U", "postgres", "app"],
            &["psql", "-U", "postgres", "app"],
        ),
        (
            &[
                "x",
                "7f3a9c2e-1b4d-4e8a-9c3f-2a1b0c9d8e7f",
                "0123456789abcdef0123456789abcdef01234567",
            ],
            &[
                "x",
                "7f3a9c2e-1b4d-4e8a-9c3f-2a1b0c9d8e7f",
                "0123456789abcdef0123456789abcdef01234567",
            ],
        ),
        // URLs and titles without secrets.
        (
            &["git", "clone", "git@example.com:owner/repo.git"],
            &["git", "clone", "git@example.com:owner/repo.git"],
        ),
        (
            &["psql", "postgres://postgres@localhost/app"],
            &["psql", "postgres://postgres@localhost/app"],
        ),
        (
            &["curl", "https://example.com/a@b?page=2"],
            &["curl", "https://example.com/a@b?page=2"],
        ),
        (
            &["python3", "-m", "http.server", "8000"],
            &["python3", "-m", "http.server", "8000"],
        ),
        (
            &["env", "PORT=3000", "NODE_ENV=production"],
            &["env", "PORT=3000", "NODE_ENV=production"],
        ),
        (
            &["nginx: master process /opt/homebrew/bin/nginx"],
            &["nginx: master process /opt/homebrew/bin/nginx"],
        ),
        (
            &["puma 6.4.0 (tcp://localhost:3002) [backend]"],
            &["puma 6.4.0 (tcp://localhost:3002) [backend]"],
        ),
        (
            &["sh", "-c", "sleep 600; echo done"],
            &["sh", "-c", "sleep 600; echo done"],
        ),
        (
            &["cargo", "run", "--release"],
            &["cargo", "run", "--release"],
        ),
        (&["-zsh"], &["-zsh"]),
    ];

    #[test]
    fn every_case_keeps_what_the_table_says() {
        let failures: Vec<String> = CASES
            .iter()
            .filter_map(|(input, want)| {
                let got = argv(&input.iter().map(|s| s.to_string()).collect::<Vec<_>>());
                (got != *want).then(|| format!("{input:?}\n  got  {got:?}\n  want {want:?}"))
            })
            .collect();
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    #[test]
    fn join_quotes_arguments_with_spaces_on_one_line() {
        let a: Vec<String> = ["echo", "a b", "it's", "x\ny\tz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(join(&a), r"echo 'a b' it's 'x\ny\tz'");
    }
}
