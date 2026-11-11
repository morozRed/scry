//! Fuzzy matching for the `/` filter, on `nucleo-matcher`.

use nucleo_matcher::pattern::{Atom, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use unicode_segmentation::UnicodeSegmentation;

/// The `/` filter: fuzzy, smart case, as in fzf and Helix, matched field by field.
pub struct Fuzzy {
    matcher: Matcher,
    atoms: Vec<Atom>,
    buf: Vec<char>,
}

impl Fuzzy {
    /// `None` for an empty query, which matches everything.
    pub fn new(query: &str) -> Option<Fuzzy> {
        let atoms = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart).atoms;
        if atoms.is_empty() {
            return None;
        }
        Some(Fuzzy {
            matcher: Matcher::new(Config::DEFAULT),
            atoms,
            buf: Vec::new(),
        })
    }

    /// Matches one row, given as its fields. Each word of the query has to match inside a
    /// single field, so letters scattered across columns never make a match; different
    /// words may match different fields. Returns, per field, the grapheme indices to
    /// highlight, or `None` when the row does not match.
    pub fn row(&mut self, fields: &[&str]) -> Option<Vec<Vec<usize>>> {
        let mut hits = vec![Vec::new(); fields.len()];
        for atom in &self.atoms {
            if atom.negative {
                // A negative word scores on fields that do not contain it.
                for f in fields {
                    let h = haystack(f, &mut self.buf);
                    atom.score(h, &mut self.matcher)?;
                }
                continue;
            }
            let mut best: Option<(u16, usize)> = None;
            for (i, f) in fields.iter().enumerate() {
                let h = haystack(f, &mut self.buf);
                if let Some(score) = atom.score(h, &mut self.matcher)
                    && best.is_none_or(|(b, _)| score > b)
                {
                    best = Some((score, i));
                }
            }
            let (_, i) = best?;
            let mut at = Vec::new();
            let h = haystack(fields[i], &mut self.buf);
            atom.indices(h, &mut self.matcher, &mut at);
            hits[i].extend(at.into_iter().map(|a| a as usize));
        }
        for h in &mut hits {
            h.sort_unstable();
            h.dedup();
        }
        Some(hits)
    }
}

/// One character per grapheme, the first of it, as nucleo matches; so the indices it
/// returns are grapheme indices. Built here rather than with `Utf32Str::new`, which
/// switches to byte indices for text whose graphemes all start with ASCII, such as a
/// letter followed by a combining accent.
fn haystack<'a>(text: &'a str, buf: &'a mut Vec<char>) -> Utf32Str<'a> {
    if text.is_ascii() {
        return Utf32Str::Ascii(text.as_bytes());
    }
    buf.clear();
    buf.extend(
        text.graphemes(true)
            .map(|g| g.chars().next().unwrap_or(' ')),
    );
    Utf32Str::Unicode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(query: &str, fields: &[&str]) -> Option<Vec<Vec<usize>>> {
        Fuzzy::new(query).unwrap().row(fields)
    }

    #[test]
    fn letters_spread_across_fields_do_not_match() {
        let row = ["node mcp-server-github", "218M", "stamp", "leftover"];
        assert_eq!(hits("vite", &row), None);
        let vite = ["vite", "412M", "stamp", ""];
        assert_eq!(hits("vite", &vite).unwrap()[0], [0, 1, 2, 3]);
    }

    #[test]
    fn each_word_may_match_its_own_field() {
        let row = ["claude", "acme/wt-auth", "ended"];
        let got = hits("claude wt", &row).unwrap();
        assert_eq!(got[0], [0, 1, 2, 3, 4, 5]);
        assert_eq!(got[1], [5, 6]);
        assert!(got[2].is_empty());
        assert_eq!(hits("!acme claude", &row), None);
        assert!(hits("!scry claude", &row).is_some());
    }

    #[test]
    fn indices_count_graphemes() {
        // "e" + combining acute is one grapheme, as is the ZWJ emoji.
        let got = hits("cr", &["cafe\u{301} crème"]).unwrap();
        assert_eq!(got[0], [5, 6]);
        let got = hits("dev", &["👩‍💻 dev"]).unwrap();
        assert_eq!(got[0], [2, 3, 4]);
        let got = hits("dev", &["東京 dev"]).unwrap();
        assert_eq!(got[0], [3, 4, 5]);
    }
}
