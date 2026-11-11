//! Fuzzy matching for the `/` filter, on `nucleo-matcher`.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// The `/` filter: fuzzy, smart case, as in fzf and Helix.
pub struct Fuzzy {
    matcher: Matcher,
    pattern: Pattern,
    buf: Vec<char>,
}

impl Fuzzy {
    /// `None` for an empty query, which matches everything.
    pub fn new(query: &str) -> Option<Fuzzy> {
        if query.trim().is_empty() {
            return None;
        }
        Some(Fuzzy {
            matcher: Matcher::new(Config::DEFAULT),
            pattern: Pattern::parse(query, CaseMatching::Smart, Normalization::Smart),
            buf: Vec::new(),
        })
    }

    pub fn matches(&mut self, haystack: &str) -> bool {
        let h = Utf32Str::new(haystack, &mut self.buf);
        self.pattern.score(h, &mut self.matcher).is_some()
    }

    /// Character positions in `text` that the query matched, for highlighting.
    pub fn hits(&mut self, text: &str) -> Vec<u32> {
        let mut out = Vec::new();
        let h = Utf32Str::new(text, &mut self.buf);
        if self
            .pattern
            .indices(h, &mut self.matcher, &mut out)
            .is_none()
        {
            return Vec::new();
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}
