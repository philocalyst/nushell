//! The REPL's history hint: fish-style ghost text taken from a history line that starts with what
//! was typed, skipping lines that name a path which no longer exists.
//!
//! [`NuHinter`] replaces reedline's `CwdAwareHinter`, which offers the newest matching line even
//! when `cd crosspoint-reader` can no longer work because the directory is gone. It
//!
//! 1. fetches up to [`CANDIDATE_LIMIT`] history lines starting with the typed line, and narrows
//!    that set in memory while the user keeps typing,
//! 2. ranks the lines: run in this directory, then successful, then most frequent, then newest,
//! 3. offers the first line whose arguments pass [`names_missing_path`], which parses the line and
//!    checks each argument a command declared with
//!    [`BuiltinCompletion::Path`](nu_protocol::BuiltinCompletion::Path) `{ exists: true, .. }`,
//! 4. and whose next word the completer still offers, by its latest answer for the line being
//!    typed: a line that names a branch long deleted is not one to offer again ([`Reality`]).
//!    Nothing is run for a hint, so a completer that reaches the network or owns the terminal
//!    is never started by typing.

use super::external_hinter::first_hint_token;
use crate::completions::{LatestAnswer, positional_at};
use log::debug;
use nu_ansi_term::Style;
use nu_engine::glob_from;
use nu_parser::parse;
use nu_path::expand_path_with;
use nu_protocol::{
    Completion, NuGlob, RequiredPath, Signals, Spanned,
    ast::{Argument, Call, Expr, Expression, Traverse},
    engine::{EngineState, StateWorkingSet},
};
use reedline::{Hinter, History, HistoryItem, ReedlineError, ReedlineErrorVariants, SearchQuery};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fs,
    ops::{ControlFlow, Range},
    path::Path,
    sync::Arc,
};

/// How many history lines one query fetches.
const CANDIDATE_LIMIT: usize = 64;

/// How many candidate lines are parsed and checked against the file system per keystroke.
const VALIDATION_BUDGET: usize = 16;

/// How many matches of a glob are examined, in the one directory it reads.
const GLOB_SCAN_LIMIT: usize = 64;

/// How many verdicts are remembered before the memo starts over.
const VERDICT_CAPACITY: usize = 1024;

/// The history lines that start with `prefix`, newest first, capped at [`CANDIDATE_LIMIT`].
#[derive(Debug, Default)]
struct Slice(Vec<HistoryItem>);

impl Slice {
    /// Whether the cap may have cut off older matches.
    fn is_truncated(&self) -> bool {
        self.0.len() >= CANDIDATE_LIMIT
    }

    /// The lines of this slice starting with `prefix`, or `None` when that cannot be exactly
    /// what history would return for `prefix`.
    ///
    /// A truncated slice holds the newest matches of its own prefix, so everything it holds
    /// that matches the longer `prefix` is the newest part of the answer. It is the whole
    /// answer only if that part fills the cap, or if the slice was not truncated.
    fn narrow(&self, prefix: &str) -> Option<Slice> {
        let kept: Vec<_> = self
            .0
            .iter()
            .filter(|item| item.command_line.starts_with(prefix))
            .cloned()
            .collect();
        (!self.is_truncated() || kept.len() >= CANDIDATE_LIMIT).then_some(Slice(kept))
    }
}

/// The history lines for one typed prefix in one directory.
#[derive(Debug)]
struct Candidates {
    /// What the lines start with.
    prefix: String,
    /// The directory the shell was in.
    cwd: String,
    /// The history length these were fetched at.
    history_len: i64,
    /// The newest lines overall.
    recent: Slice,
    /// The newest lines that were run in `cwd`; stays empty on histories that don't record it.
    in_cwd: Slice,
}

impl Candidates {
    /// Ask `history` for the lines starting with `prefix`.
    fn fetch(history: &dyn History, prefix: &str, cwd: &str, history_len: i64) -> Self {
        let session = history.session();
        let search = |mut query: SearchQuery| {
            query.limit = Some(CANDIDATE_LIMIT as i64);
            match history.search(query) {
                Ok(mut items) => {
                    items.truncate(CANDIDATE_LIMIT);
                    items
                }
                Err(ReedlineError(ReedlineErrorVariants::HistoryFeatureUnsupported { .. })) => {
                    Vec::new()
                }
                Err(err) => {
                    debug!("history hint: searching history failed: {err}");
                    Vec::new()
                }
            }
        };
        Self {
            prefix: prefix.to_owned(),
            cwd: cwd.to_owned(),
            history_len,
            recent: Slice(search(SearchQuery::last_with_prefix(
                prefix.to_owned(),
                session,
            ))),
            in_cwd: Slice(search(SearchQuery::last_with_prefix_and_cwd(
                prefix.to_owned(),
                cwd.to_owned(),
                session,
            ))),
        }
    }

    /// Whether these can be narrowed to `line` in `cwd` without asking history again.
    fn narrowable(&self, line: &str, cwd: &str, history_len: i64) -> bool {
        self.history_len == history_len && self.cwd == cwd && line.starts_with(&self.prefix)
    }

    /// The lines starting with `line`, best first. `None` if only history can say.
    fn ranked(&self, line: &str) -> Option<Vec<String>> {
        let recent = self.recent.narrow(line)?;
        let in_cwd = self.in_cwd.narrow(line)?;
        Some(rank(&recent.0, &in_cwd.0, &self.cwd))
    }
}

/// What ranking knows about one distinct command line.
#[derive(Default)]
struct Stats {
    in_cwd: bool,
    /// 2 for a success, 1 when unknown, 0 for a failure; the best of its runs.
    outcome: u8,
    runs: usize,
    newest: i64,
}

/// The distinct lines of `recent` and `in_cwd`, best first: run in `cwd`, then successful, then
/// most often run, then newest.
fn rank(recent: &[HistoryItem], in_cwd: &[HistoryItem], cwd: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut stats: HashMap<&str, Stats> = HashMap::new();
    for item in recent.iter().chain(in_cwd) {
        // The same run can come back from both queries.
        if item.id.is_some_and(|id| !seen.insert(id)) {
            continue;
        }
        let outcome = match item.exit_status {
            Some(0) => 2,
            None => 1,
            Some(_) => 0,
        };
        let entry = stats.entry(&item.command_line).or_default();
        entry.in_cwd |= item.cwd.as_deref() == Some(cwd);
        entry.outcome = entry.outcome.max(outcome);
        entry.runs += 1;
        entry.newest = entry.newest.max(item.id.map_or(-1, |id| id.0));
    }

    // `recent` is newest first, so on a tie of every key the earlier line wins.
    let mut lines: Vec<_> = stats.into_iter().collect();
    lines.sort_by_cached_key(|(line, stat)| {
        let first_seen = recent
            .iter()
            .position(|item| item.command_line == *line)
            .unwrap_or(usize::MAX);
        (
            Reverse(stat.in_cwd),
            Reverse(stat.outcome),
            Reverse(stat.runs),
            Reverse(stat.newest),
            first_seen,
        )
    });
    lines.into_iter().map(|(line, _)| line.to_owned()).collect()
}

/// Whether the next word of a history line is one its completer still knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reality {
    /// It knows the word, or has nothing to say about it.
    Real,
    /// It offered what can come there and the word is not among it.
    Gone,
}

/// A history hint that skips lines naming paths which do not exist.
pub struct NuHinter {
    engine_state: Arc<EngineState>,
    style: Style,
    hint: String,
    /// The history length of this prompt, counted on the first keystroke and forgotten when the
    /// line is emptied. `count_all` may scan the whole history, so it is not asked for every key.
    history_len: Option<i64>,
    cache: Option<Candidates>,
    /// Whether a line passes, per (line, cwd), for this prompt.
    verdicts: HashMap<(String, String), bool>,
    /// What the completer last answered, which says whether a word is still among its values.
    /// Nothing is run for a hint: a word the completer has not been asked about is real.
    answers: Option<LatestAnswer>,
}

impl NuHinter {
    /// A hinter that parses candidates with the commands of `engine_state`.
    pub fn new(engine_state: Arc<EngineState>, style: Style) -> Self {
        Self {
            engine_state,
            style,
            hint: String::new(),
            history_len: None,
            cache: None,
            verdicts: HashMap::new(),
            answers: None,
        }
    }

    /// Lets the hinter check the next word of a line against what the completer last answered
    /// for the line being typed: a branch that is gone is not among the branches it offered.
    #[must_use]
    pub fn with_answers(mut self, answers: LatestAnswer) -> Self {
        self.answers = Some(answers);
        self
    }

    /// What the completer's latest answer says of the next word of `candidate` past the `typed`
    /// bytes written, which are the line being edited. Only an answer for this line, or for an
    /// earlier state of the word being typed, says anything; it is narrowed by what was typed
    /// since, which can only take values away.
    fn reality(&self, candidate: &str, typed: usize) -> Reality {
        let Some(word) = next_word(candidate, typed) else {
            return Reality::Real;
        };
        let Some(latest) = self.answers.as_ref().and_then(LatestAnswer::get) else {
            return Reality::Real;
        };
        let Some(values) = &latest.values else {
            return Reality::Real;
        };
        let line = candidate.get(..typed).unwrap_or_default();
        // The word being typed, of which the answer was for an earlier state.
        let same_word = latest.start == word.start
            && line
                .strip_prefix(latest.line.as_str())
                .is_some_and(|more| !more.contains(char::is_whitespace));
        // A word further on that binds the same parameter as the empty word the answer was
        // for (`tool --force gone` once `tool ` was answered): the parser says which it binds.
        let same_parameter = || {
            latest.line == line
                && latest.start == line.len()
                && latest.slot.is_some()
                && positional_at(&self.engine_state, candidate, word.start) == latest.slot
        };
        if !same_word && !same_parameter() {
            return Reality::Real;
        }
        let unquoted = |text: &str| text.trim_matches(['\'', '"', '`']).to_owned();
        let written = candidate.get(word).map(unquoted).unwrap_or_default();
        if values.iter().any(|value| unquoted(value) == written) {
            Reality::Real
        } else {
            Reality::Gone
        }
    }

    /// The history lines starting with `line`, best first, from the cache when it still applies.
    fn candidates(&mut self, history: &dyn History, line: &str, cwd: &str) -> Vec<String> {
        let history_len = *self
            .history_len
            .get_or_insert_with(|| history.count_all().unwrap_or(-1));
        if let Some(cache) = &self.cache
            && cache.narrowable(line, cwd, history_len)
            && let Some(ranked) = cache.ranked(line)
        {
            return ranked;
        }
        let fresh = Candidates::fetch(history, line, cwd, history_len);
        let ranked = fresh.ranked(line).unwrap_or_default();
        self.cache = Some(fresh);
        ranked
    }

    /// Whether `line` can be offered in `cwd`.
    fn passes(&mut self, line: &str, cwd: &str) -> bool {
        let key = (line.to_owned(), cwd.to_owned());
        if let Some(&verdict) = self.verdicts.get(&key) {
            return verdict;
        }
        let missing = names_missing_path(&self.engine_state, line, Path::new(cwd));
        if let Some(path) = &missing {
            debug!("history hint: skipping {line:?}: {path}");
        }
        if self.verdicts.len() >= VERDICT_CAPACITY {
            self.verdicts.clear();
        }
        self.verdicts.insert(key, missing.is_none());
        missing.is_none()
    }

    /// The rest of the best history line that starts with `line` and can still work.
    fn find_hint(&mut self, history: &dyn History, line: &str, cwd: &str) -> String {
        if line.is_empty() {
            // A new prompt, or the line was cleared: history may have changed.
            self.history_len = None;
            self.cache = None;
            return String::new();
        }
        let mut budget = VALIDATION_BUDGET;
        for candidate in self.candidates(history, line, cwd) {
            // Nothing to offer after a line that is already complete.
            let Some(rest) = candidate.get(line.len()..).filter(|rest| !rest.is_empty()) else {
                continue;
            };
            // Every candidate looked at counts, checked now or before: the
            // verdict cache saves work but must never change which hint wins.
            if budget == 0 {
                break;
            }
            budget -= 1;
            if self.passes(&candidate, cwd) {
                match self.reality(&candidate, line.len()) {
                    Reality::Real => return rest.to_owned(),
                    Reality::Gone => continue,
                }
            }
        }
        String::new()
    }
}

impl Hinter for NuHinter {
    fn handle(
        &mut self,
        line: &str,
        _pos: usize,
        history: &dyn History,
        use_ansi_coloring: bool,
        cwd: &str,
    ) -> String {
        self.hint = self.find_hint(history, line, cwd);
        if use_ansi_coloring && !self.hint.is_empty() {
            self.style.paint(&self.hint).to_string()
        } else {
            self.hint.clone()
        }
    }

    fn complete_hint(&self) -> String {
        self.hint.clone()
    }

    fn next_hint_token(&self) -> String {
        first_hint_token(&self.hint)
    }

    fn continues_text_before_cursor(&self) -> bool {
        true
    }
}

/// Where the next word of `line` is that is not a flag, past the `typed` bytes written: the word
/// being typed if there is more of it, or the one after.
fn next_word(line: &str, typed: usize) -> Option<Range<usize>> {
    let mut start = 0;
    for word in line.split_inclusive(char::is_whitespace) {
        let end = start + word.trim_end().len();
        if end > typed && !word.trim_end().is_empty() && !word.starts_with('-') {
            return Some(start..end);
        }
        start += word.len();
    }
    None
}

/// Parse `line` and name the first path it needs that does not exist in `cwd`.
///
/// Only the first statement is examined. Later statements run after it has maybe changed the
/// directory or created what they use, as in `mkdir out; cd out`. Commands that are external or
/// unknown, and arguments that are not literals, are never a reason to reject a line.
fn names_missing_path(engine_state: &EngineState, line: &str, cwd: &Path) -> Option<String> {
    let mut working_set = StateWorkingSet::new(engine_state);
    let block = parse(&mut working_set, None, line.as_bytes(), false);
    let first = block.pipelines.first()?;
    first.elements.iter().find_map(|element| {
        element
            .expr
            .find_map(&working_set, &|expr: &Expression| match &expr.expr {
                Expr::Call(call) => match missing_in_call(&working_set, call, cwd) {
                    Some(missing) => ControlFlow::Break(Some(missing)),
                    None => ControlFlow::Continue(()),
                },
                _ => ControlFlow::Continue(()),
            })
    })
}

/// The first argument of `call` that must exist but doesn't, described for the log.
fn missing_in_call(working_set: &StateWorkingSet, call: &Call, cwd: &Path) -> Option<String> {
    let signature = working_set.get_decl(call.decl_id).signature();
    let requirement = |completion: Option<&Completion>| match completion {
        Some(Completion::Builtin(builtin)) => builtin.required_path(),
        _ => None,
    };
    let fixed = signature.required_positional.len() + signature.optional_positional.len();
    let positionals = call
        .arguments
        .iter()
        .filter(|arg| matches!(arg, Argument::Positional(_) | Argument::Unknown(_)))
        .count();
    let spreads = call
        .arguments
        .iter()
        .any(|arg| matches!(arg, Argument::Spread(_)));

    let mut index = 0;
    for argument in &call.arguments {
        let (expr, required) = match argument {
            Argument::Positional(expr) | Argument::Unknown(expr) => {
                index += 1;
                let required = signature
                    .get_positional(index - 1)
                    .and_then(|param| requirement(param.completion.as_ref()))
                    // The destination of `cp a b`; a spread may hide which argument is last.
                    .filter(|required| {
                        !(required.last_is_target
                            && index > fixed
                            && (index == positionals || spreads))
                    });
                (expr, required)
            }
            Argument::Named((long, short, Some(expr))) => {
                let flag = if long.item.is_empty() {
                    short
                        .as_ref()
                        .and_then(|short| short.item.chars().next())
                        .and_then(|short| signature.get_short_flag(short))
                } else {
                    signature.get_long_flag(&long.item)
                };
                (
                    expr,
                    flag.and_then(|flag| requirement(flag.completion.as_ref())),
                )
            }
            Argument::Named(_) | Argument::Spread(_) => continue,
        };
        if let Some(required) = required
            && !exists_as_required(expr, required, cwd)
        {
            return Some(format!(
                "`{}` of `{}` does not exist",
                String::from_utf8_lossy(working_set.get_span_contents(expr.span)),
                working_set.get_decl(call.decl_id).name(),
            ));
        }
    }
    None
}

/// Whether the literal `expr` names what `required` asks of it in `cwd`. Anything that isn't a
/// literal path, or is `-` (`cd -`), passes.
fn exists_as_required(expr: &Expression, required: RequiredPath, cwd: &Path) -> bool {
    let satisfies = |path: &Path| {
        if required.dirs {
            path.is_dir()
        } else {
            // A dangling symlink can still be removed.
            fs::symlink_metadata(path).is_ok()
        }
    };
    let (text, expand) = match &expr.expr {
        Expr::Filepath(path, no_expand) | Expr::Directory(path, no_expand) => {
            (path.as_str(), !*no_expand)
        }
        Expr::GlobPattern(pattern, no_expand) => (pattern.as_str(), !*no_expand),
        Expr::String(text) | Expr::RawString(text) => (text.as_str(), false),
        _ => return true,
    };
    if text.is_empty() || text == "-" {
        return true;
    }
    if !(expand && nu_glob::is_glob(text)) {
        return satisfies(&expand_path_with(text, cwd, expand));
    }
    // A glob is looked up only when that reads one directory: its patterns are in its last
    // component. `rm **/*.orig` from the home directory would read the whole tree on the way to
    // a paint, so whether that matches anything stays unknown, and the line passes.
    let (dir, last) = text
        .rsplit_once(std::path::is_separator)
        .unwrap_or(("", text));
    if text.contains("**") || nu_glob::is_glob(dir) {
        return true;
    }
    let pattern = Spanned {
        item: NuGlob::Expand(last.to_owned()),
        span: expr.span,
    };
    let dir = expand_path_with(dir, cwd, true);
    // The span must be a real one: the errors `glob_from` builds panic on an unknown span.
    match glob_from(&pattern, &dir, expr.span, None, Signals::empty()) {
        Ok((_, matches)) => matches
            .take(GLOB_SCAN_LIMIT)
            .filter_map(Result::ok)
            .any(|path| satisfies(&path)),
        Err(_) => false,
    }
}
