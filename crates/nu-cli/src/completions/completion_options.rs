use nu_protocol::{CompletionAlgorithm, CompletionSort};
use nu_utils::IgnoreCaseExt;
use std::{borrow::Cow, collections::HashSet, fmt::Display, path::PathBuf, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    Context, SemanticSuggestion, Trace,
    quote::QUOTES,
    rank::{Boost, FITS, Ranked, Ranker, Score, tie_break},
    usage::{Asking, UsageIndex, Used, unix_now},
};

/// Describes how suggestions should be matched.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MatchAlgorithm {
    /// Only show suggestions which begin with the given input
    ///
    /// Example:
    /// "git switch" is matched by "git sw"
    Prefix,

    /// Only show suggestions which contain the input chars at any place, best first: see
    /// [`super::rank`]. The deprecated `substring` algorithm of the configuration is this one.
    ///
    /// Example:
    /// "git checkout" is matched by "gco"
    Fuzzy,
}

/// Gives a candidate's [`Boost`] from its text.
type BoostFn<'a> = Box<dyn Fn(&str) -> Boost + 'a>;

pub struct NuMatcher<'a, T> {
    options: &'a CompletionOptions,
    should_sort: bool,
    needle: String,
    state: State<T>,
    /// What the caller knows about a candidate besides its text: see [`NuMatcher::with_boost`].
    boost: Option<BoostFn<'a>>,
    /// The candidates that take what is piped into them: see [`NuMatcher::with_fits`].
    fits: Option<&'a HashSet<String>>,
    /// Whether the order the candidates were offered in means something: see
    /// [`NuMatcher::keeping_order`].
    keep_order: bool,
    /// Whether a candidate that matches only with a slip is one: see [`NuMatcher::with_typos`].
    typos: bool,
}

enum State<T> {
    Unscored(Vec<UnscoredMatch<T>>),
    Fuzzy {
        ranker: Box<Ranker>,
        matches: Vec<FuzzyMatch<T>>,
        /// Candidates that match only with a typo. They are results only if nothing else matched.
        typos: Vec<FuzzyMatch<T>>,
    },
}

struct UnscoredMatch<T> {
    item: T,
    haystack: String,
    match_indices: Vec<usize>,
}

struct FuzzyMatch<T> {
    item: T,
    haystack: String,
    score: Score,
    match_indices: Vec<usize>,
    typo: bool,
}

/// A candidate that matched, as [`NuMatcher::results_ranked`] offers it.
pub struct Matched<T> {
    pub item: T,
    /// The graphemes of the candidate that matched. Empty for a typo correction: a candidate
    /// that matched only because it is one edit away from the needle, offered when nothing
    /// matched, which is shown as a replacement and whose [`Score::typo`] is not zero.
    pub indices: Vec<usize>,
    /// The [`Score`] it ranked by. `None` when the algorithm does not score, as `prefix` does not.
    pub score: Option<Score>,
}

/// Filters and sorts suggestions
impl<'a, T> NuMatcher<'a, T> {
    /// # Arguments
    ///
    /// * `needle` - The text to search for
    /// * `should_sort` - Should results be sorted?
    pub fn new(
        needle: impl AsRef<str>,
        options: &CompletionOptions,
        should_sort: bool,
    ) -> NuMatcher<'_, T> {
        // NOTE: Should match `'bar baz'` when completing `foo "b<tab>`
        // https://github.com/nushell/nushell/issues/16860#issuecomment-3402016955
        let needle = needle.as_ref().trim_matches(QUOTES);
        match options.match_algorithm {
            MatchAlgorithm::Prefix => {
                let lowercase_needle = if options.case_sensitive {
                    needle.to_owned()
                } else {
                    needle.to_folded_case()
                };
                NuMatcher {
                    options,
                    should_sort,
                    needle: lowercase_needle,
                    state: State::Unscored(Vec::new()),
                    boost: None,
                    fits: None,
                    keep_order: false,
                    typos: true,
                }
            }
            MatchAlgorithm::Fuzzy => NuMatcher {
                options,
                should_sort,
                needle: needle.to_owned(),
                state: State::Fuzzy {
                    ranker: Box::new(Ranker::new(needle, options.case_sensitive)),
                    matches: Vec::new(),
                    typos: Vec::new(),
                },
                boost: None,
                fits: None,
                keep_order: false,
                typos: true,
            },
        }
    }

    /// Whether a candidate that matches only with a slip is offered; see [`Candidates::typos`].
    pub(crate) fn with_typos(mut self, typos: bool) -> Self {
        self.typos = typos;
        self
    }

    /// Keep the order the candidates were offered in where nothing is typed to rank them by, rather
    /// than make it alphabetical: a completer's own list, recent branches first, means something.
    /// What the history says still lifts a candidate.
    pub(crate) fn keeping_order(mut self, keep: bool) -> Self {
        self.keep_order = keep;
        self
    }

    /// Rank candidates with what the caller knows about them: `boost` gives a candidate's
    /// [`Boost`] from its text. Without it, no candidate gets one.
    pub fn with_boost(mut self, boost: impl Fn(&str) -> Boost + 'a) -> Self {
        self.boost = Some(Box::new(boost));
        self
    }

    /// Rank the candidates named in `fits` ahead of others that match as well: the commands
    /// that take what is piped into them. Whether one matches does not depend on it.
    pub(crate) fn with_fits(mut self, fits: &'a HashSet<String>) -> Self {
        self.fits = Some(fits);
        self
    }

    /// Rank candidates of `kind` by how much, and how lately, the history says the user has used
    /// them for what `asking` says, if it says anything. The boost only orders candidates:
    /// whether one matches does not depend on it.
    pub(crate) fn with_usage(
        self,
        usage: Option<&'a Arc<UsageIndex>>,
        kind: Used,
        asking: Asking<'a>,
    ) -> Self {
        match usage {
            Some(usage) => self.with_boost(move |name| usage.boost(kind, name, &asking)),
            None => self,
        }
    }

    /// Returns whether or not the haystack matches the needle. If it does, `item` is added
    /// to the list of matches (if given).
    ///
    /// Helper to avoid code duplication between [NuMatcher::add] and [NuMatcher::matches].
    fn matches_aux(&mut self, orig_haystack: &str, item: Option<T>) -> Option<Vec<usize>> {
        let haystack = orig_haystack.trim_start_matches(QUOTES);
        let offset = orig_haystack.len() - haystack.len();
        // A combining mark can share a displayed grapheme with a stripped quote.
        let offset = if offset == 0 || orig_haystack.is_ascii() {
            offset
        } else {
            orig_haystack
                .grapheme_indices(true)
                .take_while(|(start, _)| *start <= offset)
                .count()
                .saturating_sub(1)
        };
        let haystack = haystack.trim_end_matches(QUOTES);
        match &mut self.state {
            State::Unscored(matches) => {
                let haystack_folded = if self.options.case_sensitive {
                    Cow::Borrowed(haystack)
                } else {
                    Cow::Owned(haystack.to_folded_case())
                };
                let match_start = haystack_folded
                    .starts_with(self.needle.as_str())
                    .then_some(0);
                match_start.map(|byte_start| {
                    let byte_end = byte_start + self.needle.len();
                    let match_indices: Vec<usize> =
                        if haystack.is_ascii() && !haystack.contains('\r') {
                            (offset + byte_start..offset + byte_end).collect()
                        } else {
                            // Folding can expand a displayed grapheme (`ß` becomes `ss`). Map
                            // overlapping folded byte ranges back to the original graphemes.
                            let mut indices = Vec::new();
                            let mut end = 0;
                            for (index, grapheme) in haystack.graphemes(true).enumerate() {
                                if end >= byte_end {
                                    break;
                                }
                                // Reuse the folded text when the byte length is unchanged,
                                // including expansions such as `ß` -> `ss`. Only folds that
                                // change byte length need another allocation (`İ` -> `i̇`).
                                let same_folded_byte_len = self.options.case_sensitive
                                    || grapheme.is_ascii()
                                    || haystack_folded
                                        .get(end..end + grapheme.len())
                                        .is_some_and(|folded| grapheme.eq_ignore_case(folded));
                                end += if same_folded_byte_len {
                                    grapheme.len()
                                } else {
                                    grapheme.to_folded_case().len()
                                };
                                if byte_start < end {
                                    indices.push(offset + index);
                                }
                            }
                            indices
                        };
                    if let Some(item) = item {
                        matches.push(UnscoredMatch {
                            item,
                            haystack: haystack.to_string(),
                            match_indices: match_indices.clone(),
                        });
                    }
                    match_indices
                })
            }
            State::Fuzzy {
                ranker,
                matches,
                typos,
            } => {
                let boost = self.boost.as_ref().map(|f| f(haystack)).unwrap_or_default();
                // A typo is a match too, as far as the caller can tell: whether it is a result is
                // decided when the results are taken.
                let Ranked {
                    mut score,
                    indices,
                    typo,
                    whole,
                } = ranker
                    .rank(haystack, boost)
                    .or_else(|| ranker.typo(haystack, boost).filter(|_| self.typos))?;
                if self.fits.is_some_and(|fits| fits.contains(haystack)) {
                    score.fits = FITS;
                }
                let indices: Vec<usize> = indices.iter().map(|i| offset + i).collect();
                if let Some(item) = item {
                    let found = FuzzyMatch {
                        item,
                        haystack: haystack.to_string(),
                        score,
                        match_indices: indices.clone(),
                        typo,
                    };
                    // A whole-name correction competes with the matches;
                    // one in a start of a name only stands in for them.
                    if typo && !whole { typos } else { matches }.push(found);
                }
                Some(indices)
            }
        }
    }

    /// Add the given item if the given haystack matches the needle.
    ///
    /// Returns whether the item was added.
    pub fn add(&mut self, haystack: impl AsRef<str>, item: T) -> bool {
        self.matches_aux(haystack.as_ref(), Some(item)).is_some()
    }

    /// Check if the given haystack matches the needle without adding it as a result.
    ///
    /// Returns match indices if it matched, None if it didn't.
    pub fn check_match(&mut self, haystack: &str) -> Option<Vec<usize>> {
        self.matches_aux(haystack, None)
    }

    fn sort(&mut self) {
        let keep_order = self.keep_order;
        match &mut self.state {
            State::Unscored(_) if keep_order && self.needle.is_empty() => {}
            State::Unscored(matches) => {
                matches.sort_by(|a, b| {
                    let cmp_sensitive = a.haystack.cmp(&b.haystack);
                    if self.options.case_sensitive {
                        cmp_sensitive
                    } else {
                        a.haystack
                            .to_folded_case()
                            .cmp(&b.haystack.to_folded_case())
                            .then(cmp_sensitive)
                    }
                });
            }
            State::Fuzzy {
                ranker,
                matches,
                typos,
            } => {
                // Equal candidates keep the order they were offered in: the sort is stable. With
                // nothing to rank by (nothing typed, or only the dashes of a flag): alphabetical,
                // as before, unless that order means something.
                let nothing_typed = ranker.is_unranked();
                for found in [matches, typos] {
                    match self.options.sort {
                        CompletionSort::Alphabetical => {
                            found.sort_by(|a, b| a.haystack.cmp(&b.haystack));
                        }
                        CompletionSort::Smart => found.sort_by(|a, b| {
                            b.score.total().cmp(&a.score.total()).then_with(|| {
                                if nothing_typed && keep_order {
                                    std::cmp::Ordering::Equal
                                } else if nothing_typed {
                                    a.haystack.cmp(&b.haystack)
                                } else {
                                    tie_break(&a.haystack, &b.haystack)
                                }
                            })
                        }),
                    }
                }
            }
        }
    }

    /// Sort and return all the matches, along with their match indices
    #[cfg(test)]
    pub fn results(self) -> Vec<(T, Vec<usize>)> {
        self.results_ranked()
            .into_iter()
            .map(|found| (found.item, found.indices))
            .collect()
    }

    /// The matches in the order they are offered, each with how it ranked.
    #[cfg(test)]
    pub fn results_ranked(self) -> Vec<Matched<T>> {
        self.finish().0
    }

    /// [`Self::results_ranked`], and the text of the typo corrections that are left out because a
    /// better match exists.
    fn finish(mut self) -> (Vec<Matched<T>>, Vec<String>) {
        if self.should_sort {
            self.sort();
        }
        match self.state {
            State::Unscored(matches) => (
                matches
                    .into_iter()
                    .map(|mat| Matched {
                        item: mat.item,
                        indices: mat.match_indices,
                        score: None,
                    })
                    .collect::<Vec<_>>(),
                Vec::new(),
            ),
            State::Fuzzy {
                matches,
                typos,
                ranker,
            } => {
                // A name that only scatters the typed letters through itself
                // (`swift-package-collection` for `selec`) is noise. Whether it does depends on
                // it and what is typed alone, and typing on only scatters them more, so a name
                // left out never comes back.
                let needle = ranker.needle();
                let (matches, scattered): (Vec<_>, Vec<_>) = matches.into_iter().partition(|mat| {
                    ranker.is_unranked() || mat.typo || jumps(&mat.haystack, needle) <= 1
                });
                let mut left_out: Vec<_> = scattered.into_iter().map(|mat| mat.haystack).collect();
                // Typo corrections go ahead of matches that only scatter the
                // typed letters, and are not offered beside one that reads as
                // meant.
                // With nothing to rank by (nothing typed, or only a flag's
                // dashes) the order is alphabetical and no correction applies.
                let strong = ranker.is_unranked()
                    || matches
                        .iter()
                        .any(|mat| !mat.typo && ranker.is_strong(&mat.score, &mat.match_indices));
                // Without one, every reading competes on its score: a whole
                // name one slip away, then a start of a name one slip away,
                // then letters that only occur in order.
                let shown: Vec<_> = if strong {
                    left_out.extend(typos.into_iter().map(|typo| typo.haystack));
                    matches
                } else {
                    let mut all: Vec<_> = typos.into_iter().chain(matches).collect();
                    all.sort_by(|a, b| {
                        b.score
                            .total()
                            .cmp(&a.score.total())
                            .then_with(|| tie_break(&a.haystack, &b.haystack))
                    });
                    all
                };
                let shown = shown
                    .into_iter()
                    .map(|mat| Matched {
                        item: mat.item,
                        indices: mat.match_indices,
                        score: Some(mat.score),
                    })
                    .collect::<Vec<_>>();
                (shown, left_out)
            }
        }
    }
}

/// How many of the letters of `needle` land in the middle of a word of `haystack` after a skip,
/// in the reading of it that has the fewest: none for `sel` in `select` or `gcm` in
/// `git-commit-msg`, one for `selec` in `scselect`, three in `swift-package-collection`. A letter
/// that follows the one before it, or starts a word, is no jump. Spaces in the needle count
/// nothing, and case is not told apart. Adding letters to the needle never makes the count smaller.
pub(crate) fn jumps(haystack: &str, needle: &str) -> usize {
    // A needle of blanks alone has no letters to scatter.
    if needle.chars().all(char::is_whitespace) {
        return 0;
    }
    let hay: Vec<char> = haystack.chars().collect();
    let starts = |at: usize| match at.checked_sub(1).map(|before| hay[before]) {
        None => true,
        Some(before) => {
            !before.is_alphanumeric() || (before.is_lowercase() && hay[at].is_uppercase())
        }
    };
    let same = |a: char, b: char| a.to_lowercase().eq(b.to_lowercase());
    // `best[i]`: the fewest jumps with the last letter so far on `hay[i]`.
    let mut best: Vec<Option<usize>> = Vec::new();
    for (index, letter) in needle.chars().filter(|c| !c.is_whitespace()).enumerate() {
        let mut next = vec![None; hay.len()];
        // The fewest jumps with the last letter on any of `hay[..at - 1]`.
        let mut before: Option<usize> = None;
        for at in 0..hay.len() {
            if index > 0 && at >= 2 {
                before = match (before, best[at - 2]) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
            }
            if !same(hay[at], letter) {
                continue;
            }
            let jump = usize::from(!starts(at));
            next[at] = if index == 0 {
                Some(jump)
            } else {
                let adjacent = at.checked_sub(1).and_then(|before| best[before]);
                let skipped = before.map(|cost| cost + jump);
                match (adjacent, skipped) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            };
        }
        best = next;
    }
    best.into_iter().flatten().min().unwrap_or(usize::MAX)
}

/// What a source offers for the token, before ranking: each candidate with the text it is
/// matched by. [`Candidates::suggestions`] is the one place that narrows and orders them by the
/// options, the [`Ranker`](super::rank::Ranker) and what the history says about the candidates.
pub(crate) struct Candidates<T = SemanticSuggestion> {
    pub(crate) items: Vec<(String, T)>,
    /// The text to match, when that is not the token.
    pub(crate) needle: Option<String>,
    /// The options a completer asked for, in place of the configured ones.
    pub(crate) options: Option<CompletionOptions>,
    /// Which history to rank by.
    pub(crate) used: Option<Used>,
    /// The directory the candidates are entries of, for [`Used::Path`].
    pub(crate) beneath: Option<PathBuf>,
    /// Whether the candidates are narrowed against the needle and ranked. Without it they are
    /// offered as they are: a completer that narrowed its own list, or an answer already ranked.
    pub(crate) filter: bool,
    /// Whether the matches are ordered, or kept in the order they were offered.
    pub(crate) sort: bool,
    /// Whether the next source may add to these, rather than this one being the answer.
    pub(crate) partial: bool,
    /// Whether the order offered means something, as a completer's own list does: with nothing
    /// typed it is kept rather than made alphabetical.
    pub(crate) ordered: bool,
    /// Whether a candidate one slip from what is typed is offered as what was meant. Names are
    /// misspelt; a value read from data (`160b`) is meant as it is typed, and one close to it is
    /// another value.
    pub(crate) typos: bool,
    /// The texts of the candidates that take what is piped into them, which rank ahead of the
    /// rest: see [`NuMatcher::with_fits`].
    pub(crate) fits: HashSet<String>,
}

impl<T> Default for Candidates<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            needle: None,
            options: None,
            used: None,
            beneath: None,
            filter: true,
            sort: true,
            partial: false,
            ordered: false,
            typos: true,
            fits: HashSet::new(),
        }
    }
}

impl<T> Candidates<T> {
    /// Offer `item`, matched by `haystack`.
    pub(crate) fn push_as(&mut self, haystack: impl Into<String>, item: T) {
        self.items.push((haystack.into(), item));
    }

    /// The candidates that match, best first, each with the graphemes of its text that matched.
    /// A candidate whose description matches stands in when `match_description` is on and its
    /// text does not; `describe` reads the description. A candidate that is removed is reported
    /// to `trace`, with the reason.
    fn matched(
        self,
        ctx: &Context,
        describe: impl Fn(&T) -> Option<String>,
        trace: Option<&Trace>,
    ) -> Vec<Matched<T>> {
        let Self {
            items,
            needle,
            options,
            used,
            beneath,
            sort,
            fits,
            ordered,
            typos,
            ..
        } = self;
        // Whether the completer chose how its candidates are matched.
        let own_options = options.as_ref().is_some_and(|own| {
            own.case_sensitive != ctx.options.case_sensitive
                || own.match_algorithm != ctx.options.match_algorithm
                || own.match_description != ctx.options.match_description
        });
        let options = options.as_ref().unwrap_or(ctx.options);
        let needle = needle.unwrap_or_else(|| ctx.prefix_str().into_owned());
        // Looked up here, once per request, rather than for each candidate.
        let ranked_by_use = used.is_some() && ctx.usage.is_some();
        let head = ranked_by_use.then(|| ctx.command_head()).flatten();
        let cwd = ranked_by_use
            .then(|| {
                (ctx.working_set.permanent_state)
                    .cwd_as_string(Some(ctx.stack))
                    .ok()
            })
            .flatten();
        let mut matcher = NuMatcher::new(&needle, options, sort)
            .keeping_order(ordered)
            .with_typos(typos);
        if let Some(kind) = used {
            let asking = Asking {
                head: head.as_deref(),
                cwd: cwd.as_deref(),
                beneath: beneath.as_deref(),
                nothing_typed: needle.trim_matches(QUOTES).is_empty(),
                now: unix_now(),
            };
            matcher = matcher.with_usage(ctx.usage, kind, asking);
        }
        if !fits.is_empty() {
            matcher = matcher.with_fits(&fits);
        }

        for (haystack, item) in items {
            let text = trace.map(|_| haystack.clone());
            let added = if !options.match_description || matcher.check_match(&haystack).is_some() {
                matcher.add(haystack, item)
            } else if let Some(description) = describe(&item)
                && matcher.check_match(&description).is_some()
            {
                matcher.add(description, item)
            } else {
                false
            };
            if let (false, Some(trace), Some(text)) = (added, trace, text) {
                trace.dropped(
                    None,
                    text,
                    if own_options {
                        "filtered by the completer's options".to_owned()
                    } else {
                        format!("no match for {:?}", needle.trim_matches(QUOTES))
                    },
                );
            }
        }
        let (matched, left_out) = matcher.finish();
        if let Some(trace) = trace {
            for text in left_out {
                trace.dropped(
                    None,
                    text,
                    "typo correction not shown: a better match exists",
                );
            }
        }
        matched
    }

    /// [`Self::matched`], for candidates with no description to match.
    pub(crate) fn ranked(self, ctx: &Context) -> Vec<Matched<T>> {
        self.matched(ctx, |_| None, None)
    }
}

impl Candidates<SemanticSuggestion> {
    /// Offer `suggestion`, matched by the text it displays.
    pub(crate) fn push(&mut self, suggestion: SemanticSuggestion) {
        self.push_as(suggestion.suggestion.display_value().to_owned(), suggestion);
    }

    /// Offer `suggestions` as they are: neither narrowed nor ordered.
    pub(crate) fn kept(suggestions: Vec<SemanticSuggestion>) -> Self {
        let mut kept = Self {
            filter: false,
            ..Self::default()
        };
        kept.items = suggestions
            .into_iter()
            .map(|suggestion| (String::new(), suggestion))
            .collect();
        kept
    }

    /// The suggestions that match, in the order they are offered, each with the graphemes that
    /// matched and the [`Score`] it ranked by.
    pub(crate) fn suggestions(self, ctx: &Context) -> Vec<SemanticSuggestion> {
        if !self.filter {
            return self.items.into_iter().map(|(_, item)| item).collect();
        }
        let needle = (self.needle.clone()).unwrap_or_else(|| ctx.prefix_str().into_owned());
        let split = self.split(&needle);
        let mut found: Vec<SemanticSuggestion> = self
            .matched(ctx, |item| item.suggestion.description.clone(), ctx.trace)
            .into_iter()
            .map(|found| {
                let mut suggestion = found.item;
                suggestion.suggestion.match_indices = Some(found.indices);
                // Matched only with a slip, it is what the word typed was meant to be.
                if found.score.is_some_and(|score| score.typo != 0) {
                    suggestion.fixes = Some(needle.clone());
                }
                suggestion.score = found.score;
                suggestion
            })
            .collect();
        // A forgotten space reads first when nothing starts the way the word does.
        let meant = |item: &SemanticSuggestion| {
            item.score
                .is_some_and(|score| score.prefix > 0 || score.exact > 0)
        };
        if let Some(split) = split.filter(|_| !found.iter().any(meant)) {
            found.insert(0, split);
        }
        found
    }

    /// The word `needle` read as a candidate and what was meant to follow it after a space,
    /// when it runs one on into the next: `cdcrates/` for `cd crates/`, `checkoutmain` for
    /// `checkout main`. The longest candidate the word starts with is the one meant.
    fn split(&self, needle: &str) -> Option<SemanticSuggestion> {
        let (haystack, item) = (self.items.iter())
            .filter(|(haystack, item)| {
                haystack.chars().count() >= 2
                    && item.suggestion.value == *haystack
                    && needle.len() > haystack.len()
                    && needle.starts_with(haystack.as_str())
            })
            .max_by_key(|(haystack, _)| haystack.len())?;
        let rest = &needle[haystack.len()..];
        if !rest.starts_with(|c: char| c.is_alphanumeric() || matches!(c, '.' | '/' | '~' | '-')) {
            return None;
        }
        let both = format!("{haystack} {rest}");
        Some(SemanticSuggestion {
            suggestion: reedline::Suggestion {
                value: both.clone(),
                display_override: Some(both),
                description: Some(format!("`{haystack}`, then `{rest}`: a space between")),
                match_indices: Some(Vec::new()),
                ..item.suggestion.clone()
            },
            kind: item.kind.clone(),
            source: item.source.clone(),
            help_at: item.help_at.clone(),
            fixes: Some(needle.to_owned()),
            origin: item.origin,
            ..SemanticSuggestion::default()
        })
    }
}

impl From<CompletionAlgorithm> for MatchAlgorithm {
    fn from(value: CompletionAlgorithm) -> Self {
        match value {
            CompletionAlgorithm::Prefix => MatchAlgorithm::Prefix,
            // Deprecated: a substring is a fuzzy match.
            CompletionAlgorithm::Substring | CompletionAlgorithm::Fuzzy => MatchAlgorithm::Fuzzy,
        }
    }
}

impl TryFrom<String> for MatchAlgorithm {
    type Error = InvalidMatchAlgorithm;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "prefix" => Ok(Self::Prefix),
            // Deprecated: a substring is a fuzzy match.
            "substring" | "fuzzy" => Ok(Self::Fuzzy),
            _ => Err(InvalidMatchAlgorithm::Unknown),
        }
    }
}

#[derive(Debug)]
pub enum InvalidMatchAlgorithm {
    Unknown,
}

impl Display for InvalidMatchAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            InvalidMatchAlgorithm::Unknown => write!(f, "unknown match algorithm"),
        }
    }
}

impl std::error::Error for InvalidMatchAlgorithm {}

#[derive(Clone)]
pub struct CompletionOptions {
    pub case_sensitive: bool,
    pub match_algorithm: MatchAlgorithm,
    pub sort: CompletionSort,
    pub match_description: bool,
}

impl Default for CompletionOptions {
    fn default() -> Self {
        Self {
            case_sensitive: true,
            match_algorithm: MatchAlgorithm::Fuzzy,
            sort: Default::default(),
            match_description: false,
        }
    }
}

#[cfg(test)]
mod test {
    use rstest::rstest;

    use super::{Boost, CompletionOptions, MatchAlgorithm, Matched, NuMatcher};

    #[rstest]
    #[case(MatchAlgorithm::Prefix, "example text", "", true)]
    #[case(MatchAlgorithm::Prefix, "example text", "examp", true)]
    #[case(MatchAlgorithm::Prefix, "example text", "text", false)]
    #[case(MatchAlgorithm::Fuzzy, "example text", "", true)]
    #[case(MatchAlgorithm::Fuzzy, "example text", "examp", true)]
    #[case(MatchAlgorithm::Fuzzy, "example text", "ext", true)]
    // Letters scattered through the words, as these are, do not read as meant.
    #[case(MatchAlgorithm::Fuzzy, "example text", "mplxt", false)]
    #[case(MatchAlgorithm::Fuzzy, "example text", "mpp", false)]
    fn match_algorithm_simple(
        #[case] match_algorithm: MatchAlgorithm,
        #[case] haystack: &str,
        #[case] needle: &str,
        #[case] should_match: bool,
    ) {
        let options = CompletionOptions {
            match_algorithm,
            ..Default::default()
        };
        let mut matcher = NuMatcher::new(needle, &options, true);
        matcher.add(haystack, haystack);
        let results: Vec<_> = matcher.results().iter().map(|r| r.0).collect();
        if should_match {
            assert_eq!(vec![haystack], results);
        } else {
            assert_ne!(vec![haystack], results);
        }
    }

    #[test]
    fn match_algorithm_fuzzy_sort_score() {
        let options = CompletionOptions {
            match_algorithm: MatchAlgorithm::Fuzzy,
            ..Default::default()
        };
        let mut matcher = NuMatcher::new("fob", &options, true);
        for item in ["foo/bar", "fob", "foo bar"] {
            matcher.add(item, item);
        }
        // Sort by score, then in alphabetical order
        assert_eq!(
            vec![
                ("fob", vec![0, 1, 2]),
                ("foo bar", vec![0, 1, 4]),
                ("foo/bar", vec![0, 1, 4])
            ],
            matcher.results()
        );
    }

    #[test]
    fn match_algorithm_fuzzy_sort_strip() {
        let options = CompletionOptions {
            match_algorithm: MatchAlgorithm::Fuzzy,
            ..Default::default()
        };
        let mut matcher = NuMatcher::new("'love spaces' ", &options, true);
        for item in [
            "'i love spaces'",
            "'i love spaces' so much",
            "'lovespaces' ",
        ] {
            matcher.add(item, item);
        }
        // Make sure the spaces are respected
        assert_eq!(
            vec![(
                "'i love spaces' so much",
                vec![3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
            )],
            matcher.results()
        );
    }

    #[rstest]
    #[case(MatchAlgorithm::Prefix, false, "STRASSE", "Straße", vec![0, 1, 2, 3, 4, 5])]
    #[case(MatchAlgorithm::Prefix, false, "ss", "ß", vec![0])]
    #[case(MatchAlgorithm::Prefix, false, "i", "İx", vec![0])]
    #[case(MatchAlgorithm::Fuzzy, false, "z", "e\u{301}z", vec![1])]
    #[case(MatchAlgorithm::Fuzzy, true, "z", "\"e\u{301}z\"", vec![2])]
    #[case(MatchAlgorithm::Fuzzy, true, "x", "\"\u{301}x\"", vec![1])]
    #[case(MatchAlgorithm::Prefix, true, "a\r\nb", "a\r\nb", vec![0, 1, 2])]
    #[case(MatchAlgorithm::Prefix, false, "A\r\nB", "'a\r\nb'", vec![1, 2, 3])]
    #[case(MatchAlgorithm::Fuzzy, false, "b", "a\r\nb", vec![2])]
    #[case(MatchAlgorithm::Fuzzy, true, "b", "\"a\r\nb\"", vec![3])]
    #[case(MatchAlgorithm::Prefix, false, "k", "Kx", vec![0])]
    #[case(MatchAlgorithm::Prefix, false, "ι", "ΐx", vec![0])]
    fn match_indices_follow_original_graphemes(
        #[case] match_algorithm: MatchAlgorithm,
        #[case] case_sensitive: bool,
        #[case] needle: &str,
        #[case] haystack: &str,
        #[case] expected: Vec<usize>,
    ) {
        let options = CompletionOptions {
            match_algorithm,
            case_sensitive,
            ..Default::default()
        };
        let mut matcher = NuMatcher::new(needle, &options, true);
        assert_eq!(matcher.check_match(haystack), Some(expected.clone()));
        assert!(matcher.add(haystack, haystack));
        assert_eq!(matcher.results(), vec![(haystack, expected)]);
    }

    fn fuzzy_options() -> CompletionOptions {
        CompletionOptions {
            case_sensitive: false,
            match_algorithm: MatchAlgorithm::Fuzzy,
            ..Default::default()
        }
    }

    /// The values offered for `needle` among `items`, best first.
    fn offered<'a>(needle: &str, items: &[&'a str]) -> Vec<&'a str> {
        let options = fuzzy_options();
        let mut matcher = NuMatcher::new(needle, &options, true);
        for item in items {
            matcher.add(item, *item);
        }
        matcher
            .results()
            .into_iter()
            .map(|(item, _)| item)
            .collect()
    }

    #[test]
    fn ties_go_to_the_shorter_candidate_then_natural_order_then_the_original_order() {
        // Same match, same length: natural order, so `file2` is before `file10`.
        assert_eq!(
            offered("fi", &["file10", "file2", "file1"]),
            vec!["file1", "file2", "file10"]
        );
        // The shorter candidate first.
        assert_eq!(
            offered("ab", &["abcd", "abc", "abcde"]),
            vec!["abc", "abcd", "abcde"]
        );
        // Nothing typed, or only the dashes of a flag: alphabetical, as before.
        assert_eq!(offered("", &["b", "aa", "c"]), vec!["aa", "b", "c"]);
        assert_eq!(
            offered("-", &["-h", "--mod", "--help"]),
            vec!["--help", "--mod", "-h"]
        );
        // Equal in everything: the order offered.
        assert_eq!(offered("a", &["a1", "A1"]), vec!["a1", "A1"]);
    }

    #[test]
    fn a_multi_word_name_matches_word_by_word() {
        let commands = [
            "commandline get-cursor",
            "commandline set-cursor",
            "cd",
            "str title-case",
        ];
        assert!(offered("cd cr", &commands).is_empty());
        assert_eq!(offered("str tit", &commands), vec!["str title-case"]);
        let git = [
            "git add",
            "git branch",
            "git checkout",
            "git cherry",
            "git cherry-pick",
            "git commit",
        ];
        let found = offered("git ch", &git);
        // `checkout` is near the top; `branch` has the `ch` only inside its word.
        assert_eq!(
            found,
            vec![
                "git cherry",
                "git checkout",
                "git cherry-pick",
                "git branch"
            ]
        );
    }

    /// Whether `found` is a typo correction.
    fn is_typo<T>(found: &Matched<T>) -> bool {
        found.score.is_some_and(|score| score.typo != 0)
    }

    #[test]
    fn a_typo_is_offered_only_when_nothing_else_matches() {
        let options = fuzzy_options();
        let mut matcher = NuMatcher::new("gruop", &options, true);
        for item in ["group-by", "grep", "glob"] {
            matcher.add(item, item);
        }
        // `group-by` is a typo away, and nothing matches the letters of `gruop` in order.
        let found: Vec<_> = matcher
            .results_ranked()
            .into_iter()
            .map(|found| (is_typo(&found), found.item, found.indices))
            .collect();
        assert_eq!(found, vec![(true, "group-by", vec![])]);
        // Letters scattered through a name do not read as meant: the typo is what is.
        let mut matcher = NuMatcher::new("gruop", &options, true);
        matcher.add("grxuxoxp", "scattered");
        matcher.add("group-by", "group-by");
        assert_eq!(
            matcher
                .results_ranked()
                .into_iter()
                .map(|found| (is_typo(&found), found.item))
                .collect::<Vec<_>>(),
            vec![(true, "group-by")]
        );
    }

    #[test]
    fn the_boost_decides_between_candidates_nothing_else_separates() {
        let options = fuzzy_options();
        let used = |name: &str| Boost {
            used: if name == "build" { 50 } else { 0 },
            git: 0,
        };
        let mut matcher = NuMatcher::new("b", &options, true).with_boost(used);
        for item in ["bench", "build", "bin"] {
            matcher.add(item, item);
        }
        let names: Vec<_> = matcher.results().into_iter().map(|(i, _)| i).collect();
        assert_eq!(names, vec!["build", "bin", "bench"]);
    }
}

#[cfg(test)]
mod jumps {
    use super::jumps;

    #[test]
    fn letters_that_start_words_or_follow_each_other_are_no_jumps() {
        assert_eq!(jumps("select", "sel"), 0);
        assert_eq!(jumps("git-commit-msg", "gcm"), 0);
        assert_eq!(jumps("tui select", "selec"), 0);
        assert_eq!(jumps("scselect", "selec"), 1);
        assert!(jumps("swift-package-collection", "selec") >= 2);
        // More letters never scatter less.
        assert!(jumps("math product", "apr") >= jumps("math product", "ap"));
    }
}
