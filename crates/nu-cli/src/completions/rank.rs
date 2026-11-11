//! The default fuzzy ranking: a match, scored by terms that can be read one by one.
//!
//! A candidate that matches the typed text gets a [`Score`] whose fields say why it ranks where
//! it does: nucleo's alignment score, a bonus for a prefix, for an exact name, for a match in a
//! path's file name, a penalty for a dotfile the user did not ask for, and three inputs that a
//! caller supplies (how often the candidate was used, its git status, whether a command takes
//! what is piped into it). The terms are summed by
//! [`Score::total`]; ties are broken by [`tie_break`].
//!
//! # What matches
//!
//! * One word of needle matches a candidate when its characters appear in the candidate in order
//!   (nucleo), case-insensitively unless the options say otherwise. For a path (a candidate with a
//!   separator in it) a character after a separator counts as the start of a word.
//! * A needle with whitespace is a multi-word name (`git ch`, `str tit`): it is split on
//!   whitespace and matched word by word, in order, against the candidate's words. Every needle
//!   word but the last must equal or be a prefix of its candidate word (case-folded); the last,
//!   still-typed word may match fuzzily. So `cd cr` does not match `commandline get-cursor`,
//!   though all its characters appear there in order. A needle that ends in whitespace has no
//!   still-typed word, so all its words are prefixes, and a separator must follow the last one
//!   (`git ` matches `git checkout`, not `git`).
//! * Only when nothing matches strongly (see [`Ranker::is_strong`]), a needle of four or more
//!   characters also matches a candidate that starts like it with one typo (two for eight or more characters): a
//!   transposed, missing, extra or wrong letter. Three letters match only a whole name with
//!   two neighbours swapped (`gti` for `git`). The caller decides "nothing" (see
//!   `NuMatcher`); [`Ranker::typo`] only judges one candidate. A typo match highlights nothing:
//!   it is a replacement, not a completion of what was typed.
//!
//! # The constants
//!
//! nucleo scores about 16 per matched character plus 8 per word start and a few points for
//! consecutive characters, so a four-character match scores 80 to 110. The bonuses are in those
//! units; [`PREFIX`] and [`EXACT`] are larger than the spread of nucleo's score for any needle a
//! person types, so a prefix match always ranks above a fuzzy-only match and an exact name above
//! both.

use nu_utils::IgnoreCaseExt;
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{Atom, AtomKind, CaseMatching, Normalization},
};
use std::cmp::Ordering;
use unicode_segmentation::UnicodeSegmentation;

/// A candidate (or a path's file name) that starts with the needle.
pub const PREFIX: i32 = 400;
/// A path whose last component, but not the whole path, starts with the needle.
pub const COMPONENT_PREFIX: i32 = 300;
/// A candidate (or a path's file name) that is the needle.
pub const EXACT: i32 = 1000;
/// A path whose matched characters are all in its file name.
pub const BASENAME: i32 = 10;
/// A dotfile when the needle does not start with a dot.
pub const HIDDEN: i32 = -60;
/// A command that takes the type piped into it, by [`Score::fits`]: more than the spread of the
/// alignment between candidates that match the same prefix, less than a cap's worth of history.
pub const FITS: i32 = 30;
/// A candidate that is only a typo away from the needle; one more per extra edit.
pub const TYPO: i32 = -2000;
/// Alignment points per typed character a match needs to count as strong:
/// nucleo's score for a matched character.
pub const STRONG: i32 = 16;
/// Points per typed character for a whole name one typo away: more than a
/// compact match that is not a prefix, less than any prefix.
pub const WHOLE_TYPO: i32 = 32;
/// Points per typed character for a start of a name one typo away, which
/// only competes when no match is strong: above scattered letters.
pub const PART_TYPO: i32 = 20;
/// Per edit beyond the first.
pub const TYPO_STEP: i32 = -200;
/// Per matched character that has the needle's exact case.
pub const CASE: i32 = 2;
/// A needle word that is a prefix of a candidate word (all but the last word of a needle).
const WORD_PREFIX: i32 = 20;

/// Why a candidate ranks where it does. The fields sum to [`Score::total`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Score {
    /// nucleo's alignment score, plus [`CASE`] per matched character with the needle's case.
    pub matched: i32,
    /// [`PREFIX`] when the candidate starts with the needle, [`COMPONENT_PREFIX`] when only a
    /// path's last component does.
    pub prefix: i32,
    /// [`EXACT`] when the needle is the whole name.
    pub exact: i32,
    /// [`BASENAME`] for a path whose matched characters are all in its last component.
    pub basename: i32,
    /// [`HIDDEN`] for a dotfile when the needle does not start with `.`.
    pub hidden: i32,
    /// For a candidate that matches only with a typo: [`WHOLE_TYPO`] or
    /// [`PART_TYPO`] per typed character, less [`TYPO_STEP`] for each edit past one.
    pub typo: i32,
    /// Supplied by the caller: how much the user has used this candidate.
    pub used: i32,
    /// Supplied by the caller: how much git says this candidate matters.
    pub git: i32,
    /// Supplied by the caller: [`FITS`] for a command that takes what is piped into it.
    pub fits: i32,
}

impl Score {
    /// The sum of every term.
    pub fn total(&self) -> i32 {
        self.matched
            + self.prefix
            + self.exact
            + self.basename
            + self.hidden
            + self.typo
            + self.used
            + self.git
            + self.fits
    }
}

/// What the caller knows about a candidate besides its text. Both default to nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Boost {
    /// Goes to [`Score::used`].
    pub used: i32,
    /// Goes to [`Score::git`].
    pub git: i32,
}

/// A candidate that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    pub score: Score,
    /// The matched graphemes of the candidate. Empty for a typo, and for an empty needle.
    pub indices: Vec<usize>,
    /// The candidate matched only with a typo: show it as a replacement.
    pub typo: bool,
    /// The typo is in the whole name (`gti` for `git`), not in a start of it:
    /// such a correction competes with the matches on its score.
    pub whole: bool,
}

/// One whitespace-separated word of the needle.
struct Word {
    graphemes: Vec<String>,
    atom: Atom,
}

/// A needle, ready to rank candidates against.
pub struct Ranker {
    needle: String,
    case_sensitive: bool,
    /// The needle's graphemes, as typed.
    graphemes: Vec<String>,
    /// The needle's words: one, unless the needle has whitespace in it.
    words: Vec<Word>,
    /// The needle ends in whitespace after its words: a separator must follow the last word.
    blank_tail: bool,
    multiword: bool,
    matcher: Matcher,
}

fn config(path: bool) -> Config {
    let mut config = if path {
        Config::DEFAULT.match_paths()
    } else {
        Config::DEFAULT
    };
    // A match near the start is better than one further in.
    config.prefer_prefix = true;
    config
}

fn is_separator(grapheme: &str) -> bool {
    grapheme == "/" || (cfg!(windows) && grapheme == "\\")
}

fn is_blank(grapheme: &str) -> bool {
    grapheme.chars().all(char::is_whitespace)
}

impl Ranker {
    /// A ranker for `needle`. `case_sensitive` makes a case mismatch a non-match instead of a
    /// smaller score.
    pub fn new(needle: &str, case_sensitive: bool) -> Self {
        let case = if case_sensitive {
            CaseMatching::Respect
        } else {
            CaseMatching::Ignore
        };
        let atom = |text: &str| Atom::new(text, case, Normalization::Smart, AtomKind::Fuzzy, false);
        let graphemes_of = |text: &str| text.graphemes(true).map(str::to_owned).collect();
        let trimmed = needle.trim_start();
        let multiword = trimmed.split_whitespace().nth(1).is_some()
            || (trimmed.ends_with(char::is_whitespace) && !trimmed.trim_end().is_empty());
        let words = if multiword {
            trimmed
                .split_whitespace()
                .map(|word| Word {
                    graphemes: graphemes_of(word),
                    atom: atom(word),
                })
                .collect()
        } else {
            vec![Word {
                graphemes: graphemes_of(needle),
                atom: atom(needle),
            }]
        };
        Self {
            needle: needle.to_owned(),
            case_sensitive,
            graphemes: graphemes_of(needle),
            words,
            blank_tail: multiword && trimmed.ends_with(char::is_whitespace),
            multiword,
            matcher: Matcher::new(config(false)),
        }
    }

    /// Whether the needle says nothing about order: it is empty, or it is only dashes (the start
    /// of a flag, which every flag has). The candidates then keep the alphabetical order they
    /// have always had instead of being ordered by length.
    pub fn is_unranked(&self) -> bool {
        self.needle.chars().all(|c| c == '-')
    }

    /// The text being matched.
    pub fn needle(&self) -> &str {
        &self.needle
    }

    fn same(&self, hay: &str, needle: &str) -> bool {
        if self.case_sensitive || hay == needle {
            hay == needle
        } else if hay.is_ascii() && needle.is_ascii() {
            hay.eq_ignore_ascii_case(needle)
        } else {
            hay.to_folded_case() == needle.to_folded_case()
        }
    }

    /// Whether `hay` starts with `needle`, grapheme by grapheme, case-folded unless the ranker is
    /// case-sensitive.
    fn starts_with(&self, hay: &[&str], needle: &[String]) -> bool {
        hay.len() >= needle.len() && hay.iter().zip(needle).all(|(h, n)| self.same(h, n))
    }

    /// Match `text` against one needle word with nucleo: its score and the matched graphemes.
    fn fuzzy(&mut self, word: usize, text: &str, path: bool) -> Option<(i32, Vec<usize>)> {
        self.matcher.config = config(path);
        let mut buffer = Vec::new();
        // nucleo 0.3's constructor can incorrectly reuse UTF-8 bytes when every grapheme starts
        // with ASCII, such as `e` followed by a combining accent. CRLF is also a single
        // grapheme, despite containing two ASCII bytes.
        let haystack = if text.is_ascii() && !text.contains('\r') {
            Utf32Str::Ascii(text.as_bytes())
        } else {
            buffer.extend(nucleo_matcher::chars::graphemes(text));
            Utf32Str::Unicode(&buffer)
        };
        let mut indices = Vec::new();
        let score = self.words[word]
            .atom
            .indices(haystack, &mut self.matcher, &mut indices)?;
        let indices = indices
            .into_iter()
            .map(|i| usize::try_from(i).unwrap_or(usize::MAX))
            .collect();
        Some((i32::from(score), indices))
    }

    /// Points for the matched graphemes whose case is exactly the needle word's.
    fn case_bonus(&self, word: &[String], hay: &[&str], indices: &[usize]) -> i32 {
        let cased = |g: &str| g.to_lowercase() != g.to_uppercase();
        CASE * i32::try_from(
            word.iter()
                .zip(indices)
                .filter(|(n, i)| hay.get(**i).is_some_and(|h| h == n && cased(h)))
                .count(),
        )
        .unwrap_or(0)
    }

    /// Whether a match scored `score` reads as meant rather than as letters
    /// that happen to occur in order: a prefix, the whole name, or an
    /// alignment worth a full character for each typed one whose letters lie
    /// within twice the typed length of each other. Typo corrections
    /// are offered when no match is strong, so scattered letters (`gti` in
    /// `generate completions`) do not hide `git`.
    pub fn is_strong(&self, score: &Score, indices: &[usize]) -> bool {
        let typed = self.graphemes.len();
        let compact = match (indices.first(), indices.last()) {
            (Some(first), Some(last)) => last - first < typed.saturating_mul(2),
            _ => false,
        };
        let points = i32::try_from(typed).unwrap_or(i32::MAX);
        score.prefix > 0
            || score.exact > 0
            || (compact && score.matched >= STRONG.saturating_mul(points))
    }

    /// Rank `haystack`: `None` if it does not match the needle.
    pub fn rank(&mut self, haystack: &str, boost: Boost) -> Option<Ranked> {
        let graphemes: Vec<&str> = haystack.graphemes(true).collect();
        let path = graphemes.iter().any(|g| is_separator(g));
        let (matched, indices) = if self.needle.is_empty() {
            (0, Vec::new())
        } else if self.needle.chars().all(char::is_whitespace) {
            // Only blanks typed, as in `open " `: the fuzzy matcher takes them for nothing, and
            // it is the names that hold them that match.
            let at = haystack.find(&self.needle)?;
            let indices = (haystack.grapheme_indices(true).enumerate())
                .filter(|(_, (byte, _))| (at..at + self.needle.len()).contains(byte))
                .map(|(index, _)| index)
                .collect();
            (0, indices)
        } else if self.multiword {
            self.match_words(haystack, &graphemes)?
        } else {
            let (score, indices) = self.fuzzy(0, haystack, path)?;
            let bonus = self.case_bonus(&self.words[0].graphemes, &graphemes, &indices);
            (score + bonus, indices)
        };
        let score = self.score(&graphemes, path, matched, &indices, boost);
        Some(Ranked {
            score,
            indices,
            typo: false,
            whole: false,
        })
    }

    /// Match a needle with whitespace word by word. See the module documentation.
    fn match_words(&mut self, haystack: &str, graphemes: &[&str]) -> Option<(i32, Vec<usize>)> {
        // The candidate's words, as grapheme ranges.
        let mut words: Vec<(usize, usize)> = Vec::new();
        let mut start = None;
        for (i, g) in graphemes.iter().enumerate() {
            match (is_blank(g), start) {
                (false, None) => start = Some(i),
                (true, Some(s)) => {
                    words.push((s, i));
                    start = None;
                }
                _ => {}
            }
        }
        words.extend(start.map(|s| (s, graphemes.len())));

        // Where each candidate word starts and ends, in bytes.
        let offsets: Vec<usize> = haystack
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain(std::iter::once(haystack.len()))
            .collect();

        let last = self.words.len() - 1;
        let mut indices = Vec::new();
        let mut matched = 0;
        let mut next = 0;
        let mut previous_end = None;
        for k in 0..self.words.len() {
            // The last needle word is fuzzy, unless whitespace after it says it is finished.
            let prefix_only = k < last || self.blank_tail;
            let candidates = words.get(next..)?;
            // The separator before a word that is not the first is part of the match.
            let separator = |start: usize| start.checked_sub(1).filter(|_| k > 0);
            if prefix_only {
                let needle = &self.words[k].graphemes;
                let at = candidates
                    .iter()
                    .position(|&(s, e)| self.starts_with(&graphemes[s..e], needle))?;
                let (s, e) = candidates[at];
                indices.extend(separator(s));
                indices.extend(s..s + needle.len());
                matched += WORD_PREFIX + 16 * i32::try_from(needle.len()).unwrap_or(0);
                next += at + 1;
                previous_end = Some(e);
            } else {
                let mut best: Option<(i32, usize, Vec<usize>)> = None;
                for &(s, e) in candidates {
                    let text = &haystack[offsets[s]..offsets[e]];
                    if let Some((score, local)) = self.fuzzy(k, text, false) {
                        let global: Vec<usize> = local.iter().map(|i| s + i).collect();
                        let score =
                            score + self.case_bonus(&self.words[k].graphemes, graphemes, &global);
                        if best.as_ref().is_none_or(|(b, _, _)| score > *b) {
                            best = Some((score, s, global));
                        }
                    }
                }
                let (score, s, global) = best?;
                indices.extend(separator(s));
                matched += score;
                indices.extend(global);
            }
        }
        // Whitespace after the last word asks for a separator after the last matched word.
        if self.blank_tail {
            let after = previous_end?;
            if !graphemes.get(after).is_some_and(|g| is_blank(g)) {
                return None;
            }
            indices.push(after);
        }
        indices.sort_unstable();
        indices.dedup();
        Some((matched, indices))
    }

    /// The part of `graphemes` after the last separator (ignoring a trailing one), and where it
    /// starts.
    fn last_component<'a, 'b>(&self, graphemes: &'a [&'b str]) -> (usize, &'a [&'b str]) {
        let end = graphemes.len() - usize::from(graphemes.last().is_some_and(|g| is_separator(g)));
        let start = graphemes[..end]
            .iter()
            .rposition(|g| is_separator(g))
            .map_or(0, |i| i + 1);
        (start, &graphemes[start..end])
    }

    /// Everything but the nucleo score and the typo.
    fn score(
        &self,
        graphemes: &[&str],
        path: bool,
        matched: i32,
        indices: &[usize],
        boost: Boost,
    ) -> Score {
        let mut score = Score {
            matched,
            used: boost.used,
            git: boost.git,
            ..Score::default()
        };
        if self.needle.is_empty() {
            score.hidden = self.hidden(graphemes);
            return score;
        }
        let (start, component) = self.last_component(graphemes);
        let needle = &self.graphemes;
        let whole = match graphemes.split_last() {
            Some((last, rest)) if is_separator(last) => rest,
            _ => graphemes,
        };
        if self.starts_with(whole, needle) {
            score.prefix = PREFIX;
        } else if self.starts_with(component, needle) {
            score.prefix = COMPONENT_PREFIX;
        }
        let equals = |hay: &[&str]| hay.len() == needle.len() && self.starts_with(hay, needle);
        if equals(whole) || equals(component) {
            score.exact = EXACT;
        }
        if path && !indices.is_empty() && indices.iter().all(|i| *i >= start) {
            score.basename = BASENAME;
        }
        score.hidden = self.hidden(graphemes);
        score
    }

    /// [`HIDDEN`] for a dotfile the needle does not ask for.
    fn hidden(&self, graphemes: &[&str]) -> i32 {
        let (_, component) = self.last_component(graphemes);
        let dotfile = component.first() == Some(&".")
            && !matches!(component, ["."] | [".", "."])
            && component.len() > 1;
        let asked = self
            .needle
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|last| last.starts_with('.'))
            || self.needle.starts_with('.');
        if dotfile && !asked { HIDDEN } else { 0 }
    }

    /// Rank `haystack` as a typo of the needle, for when nothing matches: see the module
    /// documentation.
    pub fn typo(&self, haystack: &str, boost: Boost) -> Option<Ranked> {
        let n = self.graphemes.len();
        // Three letters admit one slip, and only two swapped neighbours
        // (`gti` for `git`): anything looser matches half of every list.
        if self.multiword || n < 3 {
            return None;
        }
        let swap_only = n == 3;
        let allowed = if n >= 8 { 2 } else { 1 };
        let graphemes: Vec<&str> = haystack.graphemes(true).collect();
        let (_, component) = self.last_component(&graphemes);
        let folded = |g: &str| {
            if self.case_sensitive {
                g.to_owned()
            } else {
                g.to_folded_case()
            }
        };
        let needle: Vec<String> = self.graphemes.iter().map(|g| folded(g)).collect();
        let longest = (n + allowed).min(component.len());
        let hay: Vec<String> = component[..longest].iter().map(|g| folded(g)).collect();
        if swap_only {
            let swapped = hay.len() == n
                && (0..n - 1).any(|i| {
                    needle[i] == hay[i + 1]
                        && needle[i + 1] == hay[i]
                        && needle[i] != needle[i + 1]
                        && needle
                            .iter()
                            .zip(&hay)
                            .enumerate()
                            .all(|(j, (a, b))| j == i || j == i + 1 || a == b)
                });
            if !swapped {
                return None;
            }
        }
        let distance = (n.saturating_sub(allowed)..=longest)
            .filter(|len| *len > 0 && (!swap_only || *len == n))
            .map(|len| damerau_levenshtein(&needle, &hay[..len]))
            .min()?;
        if distance == 0 || distance > allowed {
            return None;
        }
        // The whole name, not a start of it, is the typo's.
        let whole = component.len() <= longest
            && damerau_levenshtein(&needle, &hay[..component.len()]) == distance;
        let typed = i32::try_from(n).unwrap_or(i32::MAX);
        let mut score = Score {
            // A whole name one slip away reads as meant: it ranks with a
            // compact match of what was typed, below every prefix.
            typo: if whole {
                WHOLE_TYPO.saturating_mul(typed)
            } else {
                PART_TYPO.saturating_mul(typed)
                    + TYPO_STEP * (i32::try_from(distance).unwrap_or(1) - 1)
            },
            used: boost.used,
            git: boost.git,
            ..Score::default()
        };
        score.hidden = self.hidden(&graphemes);
        Some(Ranked {
            score,
            indices: Vec::new(),
            typo: true,
            whole,
        })
    }
}

/// The optimal string alignment distance: insertions, deletions, substitutions and transpositions
/// of neighbours, each one edit.
pub(crate) fn damerau_levenshtein(a: &[String], b: &[String]) -> usize {
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}

/// Compare two names so that `file2` comes before `file10`: runs of digits compare as numbers,
/// the rest by case-folded text, then as written.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    fn chunks(s: &str) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut start = 0;
        let mut digits = None;
        for (i, c) in s.char_indices() {
            let is_digit = c.is_ascii_digit();
            if digits.is_some_and(|d| d != is_digit) {
                out.push((digits.unwrap_or(false), &s[start..i]));
                start = i;
            }
            digits = Some(is_digit);
        }
        if start < s.len() {
            out.push((digits.unwrap_or(false), &s[start..]));
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for ((a_digits, x), (b_digits, y)) in ca.iter().zip(&cb) {
        let order = if *a_digits && *b_digits {
            // Without leading zeros, a longer run is a bigger number.
            let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
            x.len().cmp(&y.len()).then_with(|| x.cmp(y))
        } else {
            x.to_folded_case().cmp(&y.to_folded_case())
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    ca.len().cmp(&cb.len()).then_with(|| a.cmp(b))
}

/// How candidates of equal total are ordered: the shorter one, then natural order. (Candidates
/// that are still equal keep the order they were offered in; the caller sorts stably.)
pub fn tie_break(a: &str, b: &str) -> Ordering {
    a.graphemes(true)
        .count()
        .cmp(&b.graphemes(true).count())
        .then_with(|| natural_cmp(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(needle: &str, haystack: &str) -> Option<Ranked> {
        Ranker::new(needle, false).rank(haystack, Boost::default())
    }

    fn total(needle: &str, haystack: &str) -> i32 {
        rank(needle, haystack)
            .map(|r| r.score.total())
            .unwrap_or(i32::MIN)
    }

    #[test]
    fn the_breakdown_sums_to_the_total() {
        let ranked = rank("src", "src/main.rs").expect("matches");
        let s = ranked.score;
        assert_eq!(
            s.total(),
            s.matched + s.prefix + s.exact + s.basename + s.hidden + s.typo + s.used + s.git
        );
        assert_eq!(s.prefix, PREFIX);
        assert_eq!(s.exact, 0);
    }

    #[test]
    fn an_exact_name_beats_a_prefix_and_a_prefix_beats_a_fuzzy_match() {
        let exact = total("cargo", "cargo");
        let prefix = total("cargo", "cargo-nextest");
        let fuzzy = total("cargo", "c_a_r_g_o_x");
        assert!(exact > prefix, "{exact} {prefix}");
        assert!(prefix > fuzzy, "{prefix} {fuzzy}");
    }

    #[test]
    fn a_path_matches_in_its_file_name() {
        // Only the file name starts with the needle: a smaller prefix bonus than a path that starts
        // with it, and the file name holds every matched character.
        let r = rank("main", "src/main.rs").expect("matches");
        assert_eq!(r.score.prefix, COMPONENT_PREFIX);
        assert_eq!(r.score.basename, BASENAME);
        assert_eq!(
            rank("src/ma", "src/main.rs").expect("m").score.prefix,
            PREFIX
        );
        // ... and a file named like the needle beats one that only contains it scattered.
        assert!(total("main", "src/main.rs") > total("main", "src/m_a_i_n.rs"));
        let exact_dir = rank("src", "crates/src/").expect("matches");
        assert_eq!(exact_dir.score.exact, EXACT);
    }

    #[test]
    fn dotfiles_rank_below_unless_asked_for() {
        assert_eq!(rank("git", ".gitignore").expect("m").score.hidden, HIDDEN);
        assert_eq!(rank(".git", ".gitignore").expect("m").score.hidden, 0);
        assert_eq!(
            rank("git", "src/.gitignore").expect("m").score.hidden,
            HIDDEN
        );
        assert_eq!(rank("", ".config").expect("m").score.hidden, HIDDEN);
        assert_eq!(rank("", "..").expect("m").score.hidden, 0);
    }

    #[test]
    fn case_that_agrees_scores_a_little_more() {
        assert!(total("Do", "Docs") > total("Do", "docs"));
        assert!(rank("do", "DOCS").is_some());
        assert!(
            Ranker::new("do", true)
                .rank("DOCS", Boost::default())
                .is_none()
        );
    }

    #[test]
    fn indices_are_graphemes_of_the_candidate() {
        assert_eq!(rank("z", "e\u{301}z").expect("m").indices, vec![1]);
        assert_eq!(rank("fob", "foo bar").expect("m").indices, vec![0, 1, 4]);
    }

    #[test]
    fn the_boost_is_added() {
        let boost = Boost { used: 30, git: 5 };
        let ranked = Ranker::new("b", false).rank("build", boost).expect("m");
        assert_eq!((ranked.score.used, ranked.score.git), (30, 5));
        let plain = Ranker::new("b", false)
            .rank("bench", Boost::default())
            .expect("m");
        assert!(ranked.score.total() > plain.score.total());
    }

    #[test]
    fn a_needle_with_whitespace_matches_word_by_word() {
        // Every character of `cd cr` appears in order, but `cd` is no word of the command.
        assert!(rank("cd cr", "commandline get-cursor").is_none());
        assert!(rank("cd cr", "commandline set-cursor").is_none());
        assert!(rank("str tit", "str title-case").is_some());
        assert!(rank("git ch", "git checkout").is_some());
        // The earlier words are prefixes of their words, case-folded.
        assert!(rank("g ch", "git checkout").is_some());
        assert!(rank("GIT ch", "git checkout").is_some());
        assert!(rank("gt ch", "git checkout").is_none());
        // The last word is fuzzy.
        assert!(rank("git cko", "git checkout").is_some());
        assert!(rank("git zzz", "git checkout").is_none());
        // In order, and not necessarily adjacent words.
        assert!(rank("git ch", "ch git").is_none());
        assert!(rank("a c", "a b c").is_some());
        // A trailing space needs a separator after the last word.
        assert!(rank("git ", "git checkout").is_some());
        assert!(rank("git ", "git").is_none());
        assert!(rank("t ", "to csv").is_some());
    }

    #[test]
    fn multi_word_indices_cover_the_words_and_their_separators() {
        assert_eq!(
            rank("love spaces' ", "i love spaces' so much")
                .expect("m")
                .indices,
            (2..15).collect::<Vec<_>>()
        );
        assert_eq!(
            rank("git ch", "git checkout").expect("m").indices,
            vec![0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn a_typo_matches_only_a_candidate_one_edit_away() {
        let typo = |needle: &str, haystack: &str| {
            Ranker::new(needle, false).typo(haystack, Boost::default())
        };
        // Swapped, missing, doubled and wrong letters, against the candidate's own prefix.
        assert!(typo("gruop", "group-by").is_some());
        assert!(typo("grup", "group").is_some());
        assert!(typo("grroup", "group").is_some());
        assert!(typo("gtoup", "group").is_some());
        let found = typo("gruop", "group-by").expect("typo");
        // A correction highlights nothing; its weight is in `typo`, and a
        // start of a name weighs less than a whole one.
        assert!(found.typo && found.indices.is_empty() && !found.whole);
        let whole = typo("gruop", "group").expect("typo");
        assert!(whole.whole && whole.score.typo > found.score.typo);
        // Too short, too far, or nothing wrong.
        assert!(typo("gru", "group").is_none());
        assert!(typo("grxxp", "group").is_none());
        assert!(typo("group", "group").is_none());
        // Two edits are fine from eight characters.
        assert!(typo("configrutaion", "configuration").is_some());
        assert!(typo("grooup", "grp").is_none());
    }

    #[test]
    fn natural_order_puts_file2_before_file10() {
        assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("file10", "file2"), Ordering::Greater);
        assert_eq!(natural_cmp("a01", "a1"), Ordering::Less);
        assert_eq!(natural_cmp("File1", "file1"), Ordering::Less);
        assert_eq!(tie_break("ab", "abc"), Ordering::Less);
        assert_eq!(tie_break("f2", "f10"), Ordering::Less);
    }

    mod laws {
        use super::super::*;
        use crate::completions::completion_options::{
            CompletionOptions, MatchAlgorithm, NuMatcher,
        };
        use proptest::prelude::*;

        fn options() -> CompletionOptions {
            CompletionOptions {
                case_sensitive: false,
                match_algorithm: MatchAlgorithm::Fuzzy,
                ..CompletionOptions::default()
            }
        }

        /// What `NuMatcher` offers for `needle` among `names`, best first, and whether the answer is
        /// a typo correction.
        fn offered(needle: &str, names: &[String]) -> (Vec<String>, bool) {
            let options = options();
            let mut matcher = NuMatcher::new(needle, &options, true);
            for name in names {
                matcher.add(name, name.clone());
            }
            let found = matcher.results_ranked();
            let typo = found
                .iter()
                .any(|found| found.score.is_some_and(|score| score.typo != 0));
            (found.into_iter().map(|found| found.item).collect(), typo)
        }

        fn names_and_needle() -> impl Strategy<Value = (Vec<String>, String)> {
            prop::collection::hash_set("[a-d][a-d_.-]{1,6}", 2..9).prop_flat_map(|set| {
                let mut names: Vec<String> = set.into_iter().collect();
                names.sort();
                let own = names.clone();
                let needle = prop_oneof![
                    1 => "[a-d][a-d_.-]{0,3}".boxed(),
                    2 => (0..own.len(), 1usize..8)
                        .prop_map(move |(i, k)| own[i].chars().take(k).collect::<String>())
                        .boxed(),
                ];
                (Just(names), needle)
            })
        }

        proptest! {
            /// A name that is the needle is offered first.
            #[test]
            fn an_exact_match_ranks_first((names, needle) in names_and_needle()) {
                let (offered, _) = offered(&needle, &names);
                if names.contains(&needle) {
                    prop_assert_eq!(offered.first(), Some(&needle));
                }
            }

            /// Every name that starts with the needle is offered before every other match.
            #[test]
            fn a_prefix_match_ranks_above_a_fuzzy_only_match((names, needle) in names_and_needle()) {
                let (offered, typo) = offered(&needle, &names);
                prop_assume!(!typo);
                let first_other = offered.iter().position(|n| !n.starts_with(&needle));
                let last_prefix = offered.iter().rposition(|n| n.starts_with(&needle));
                if let (Some(other), Some(prefix)) = (first_other, last_prefix) {
                    prop_assert!(prefix < other, "{offered:?} for {needle:?}");
                }
            }

            /// The terms of every score add up to its total, and the answer is the same every time.
            #[test]
            fn the_breakdown_sums_and_ranking_is_deterministic((names, needle) in names_and_needle()) {
                for name in &names {
                    let mut ranker = Ranker::new(&needle, false);
                    if let Some(ranked) = ranker.rank(name, Boost { used: 3, git: 4 }) {
                        let s = ranked.score;
                        prop_assert_eq!(
                            s.total(),
                            s.matched + s.prefix + s.exact + s.basename + s.hidden + s.typo + s.used + s.git
                        );
                        prop_assert_eq!((s.used, s.git), (3, 4));
                        // The indices are graphemes of the name, in order.
                        let count = name.chars().count();
                        prop_assert!(ranked.indices.windows(2).all(|w| w[0] < w[1]));
                        prop_assert!(ranked.indices.iter().all(|i| *i < count));
                        prop_assert_eq!(ranker.rank(name, Boost { used: 3, git: 4 }), Some(ranked));
                    }
                }
                prop_assert_eq!(offered(&needle, &names), offered(&needle, &names));
            }
        }
    }
}
