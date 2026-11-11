//! Syntax highlighting of the line being edited.
//!
//! A line is parsed once per edit ([`LineParseCache`]) and laid out once: the flattened shapes of
//! the parse are cut into [`Segment`]s that cover the line exactly, each naming the shape whose
//! `$env.config.color_config` style paints it. Everything that depends on the cursor or the
//! configuration is left to the paint, which is a walk over the segments:
//!
//! * the parser flattens values with units, cell paths, ranges and signatures finely (see
//!   [`FlatShape`]), and the layout paints each shape by its `color_config` key;
//! * the brackets of the line are paired once per parse, from the pieces of the layout that are
//!   code, so those of a string or a comment are never taken for code, and the pair under the
//!   cursor is found without walking the syntax tree;
//! * a token the parser rejects is only drawn as an error once the cursor has left it, so that
//!   typing `$en` or `1 +` does not flash red on the way to `$env` or `1 + 2`;
//! * the styles of a configuration and the externals resolved on `PATH` are kept between paints.

use log::trace;
use nu_ansi_term::Style;
use nu_color_config::{get_matching_brackets_style, get_shape_color};
use nu_engine::env;
use nu_parser::{FlatShape, flatten_block, parse_probed};
use nu_path::AbsolutePathBuf;
use nu_protocol::{
    Config, ParseError, Span,
    ast::Block,
    engine::{EngineState, Probe, Stack, StateDelta, StateWorkingSet},
};
use nu_utils::time::Instant;
use reedline::{AbbrExpandContext, Highlighter, StyledText};
use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::OsStr,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

/// A highlighter that does nothing
///
/// Used to remove highlighting from a reedline instance
/// (letting NuHighlighter structs be dropped)
#[derive(Default)]
pub struct NoOpHighlighter {}

impl Highlighter for NoOpHighlighter {
    fn highlight(&self, _line: &str, _cursor: usize) -> reedline::StyledText {
        StyledText::new()
    }
}

/// How long an external found, or not found, on `PATH` is believed. A program installed in
/// another terminal shows as one within this long of being typed.
const EXTERNAL_LIFETIME: Duration = Duration::from_secs(3);

/// How many externals are remembered before the answers are dropped.
const EXTERNALS_KEPT: usize = 256;

/// Locks `mutex`, taking the data of a poisoned one: a cache holds nothing worth a panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The `color_config` key a rejected token is drawn with until the cursor has left it: what it
/// is on the way to being, from its first character. `None` leaves it unstyled.
fn pending(text: &str) -> Option<&'static str> {
    Some(match text.chars().next()? {
        '$' => "shape_variable",
        '"' | '\'' | '`' => "shape_string",
        '(' => "shape_block",
        '[' => "shape_list",
        '{' => "shape_record",
        '+' | '-' | '*' | '/' | '=' | '<' | '>' | '!' | '%' | '^' | '&' | '|' | '~' => {
            "shape_operator"
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// The layout: the line cut into painted pieces.
// ---------------------------------------------------------------------------------------------

/// What paints a [`Segment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Text no shape covers: whitespace, comments, the brackets of an empty block.
    Gap,
    /// The style of a shape's `color_config` key.
    Shape(&'static str),
    /// An external command, whose word is the span: it is painted as resolved when `PATH` has
    /// it and the configuration asks for that.
    External(Span),
}

/// A piece of the line, as byte offsets into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    start: usize,
    end: usize,
    kind: Kind,
}

/// The line cut into segments, with its brackets paired.
struct Layout {
    /// Ordered, adjacent, and covering the line.
    segments: Vec<Segment>,
    /// Each bracket of the line that is code, with the one it pairs with, sorted by position.
    pairs: Vec<(usize, usize)>,
}

impl Layout {
    /// Cuts `line` along `shapes`, which are spans of the engine state where the line starts at
    /// `offset`.
    fn new(line: &str, shapes: &[(Span, FlatShape)], offset: usize) -> Self {
        // Both ends of a span are moved to a character boundary, so that cutting never panics.
        let snap = |at: usize| (at..=line.len()).find(|&at| line.is_char_boundary(at));
        let mut cut: Vec<Segment> = Vec::with_capacity(shapes.len() * 2 + 1);
        let mut laid = 0;
        for (raw, shape) in shapes {
            // The shape of an aliased external is painted where the alias is written.
            let (span, kind) = match shape {
                FlatShape::External(alias) => (**alias, Kind::External(*raw)),
                _ => (*raw, Kind::Shape(shape.as_str())),
            };
            // Text of the engine state, such as the definition of an alias, is not on the line.
            let Some(start) = span.start.checked_sub(offset) else {
                continue;
            };
            let end = span.end.saturating_sub(offset).min(line.len());
            let (Some(start), Some(end)) = (snap(start), snap(end)) else {
                continue;
            };
            if end <= laid {
                // The same text again. The generic shape of a word, a string, gives way to a
                // more telling one the parser also flattened for it, such as a variable, and the
                // `$it` a row condition's column is read with gives way to the column.
                if let Some(last) = cut.last_mut()
                    && (last.start, last.end) == (start, end)
                    && (last.kind == Kind::Shape("shape_string")
                        || kind == Kind::Shape("shape_cell_path"))
                {
                    last.kind = kind;
                }
                continue;
            }
            let start = start.max(laid);
            if start >= end {
                continue;
            }
            if start > laid {
                cut.push(Segment {
                    start: laid,
                    end: start,
                    kind: Kind::Gap,
                });
            }
            cut.push(Segment { start, end, kind });
            laid = end;
        }
        if laid < line.len() {
            cut.push(Segment {
                start: laid,
                end: line.len(),
                kind: Kind::Gap,
            });
        }

        // The brackets of everything that is code are collected.
        let mut brackets = Vec::new();
        for segment in &cut {
            if let Kind::Gap
            | Kind::Shape(
                "shape_list"
                | "shape_table"
                | "shape_record"
                | "shape_block"
                | "shape_closure"
                | "shape_match_pattern"
                | "shape_signature",
            ) = segment.kind
            {
                scan_brackets(line, segment.start, segment.end, &mut brackets);
            }
        }
        Self {
            segments: cut,
            pairs: pair_up(&brackets),
        }
    }

    /// The pair of brackets the cursor is at, as their positions in order: the one it is on,
    /// or failing that the one it has just passed.
    fn matching(&self, line: &str, cursor: usize) -> Option<[usize; 2]> {
        let on = (cursor < line.len()).then_some(cursor);
        let before = line[..cursor]
            .chars()
            .next_back()
            .map(|c| cursor - c.len_utf8());
        [on, before].into_iter().flatten().find_map(|at| {
            let found = self
                .pairs
                .binary_search_by_key(&at, |(open, _)| *open)
                .ok()?;
            let other = self.pairs[found].1;
            Some([at.min(other), at.max(other)])
        })
    }
}

/// Records the brackets of `line[start..end]` in `out`, as their positions and bytes, leaving
/// out those in a comment.
fn scan_brackets(line: &str, start: usize, end: usize, out: &mut Vec<(usize, u8)>) {
    let bytes = line.as_bytes();
    let mut at = start;
    while at < end {
        match bytes[at] {
            // A comment starts a word, and runs to the end of its line.
            b'#' if at == 0 || bytes[at - 1].is_ascii_whitespace() => {
                at = line[at..end].find('\n').map_or(end, |newline| at + newline);
                continue;
            }
            byte @ (b'{' | b'}' | b'[' | b']' | b'(' | b')') => out.push((at, byte)),
            _ => {}
        }
        at += 1;
    }
}

/// Pairs the brackets of `brackets`, in order of position: each closing bracket with the last
/// one open if it is of its kind, and with none otherwise, so that a stray one pairs with
/// nothing and leaves the rest as they were. Both ways, sorted by the first.
fn pair_up(brackets: &[(usize, u8)]) -> Vec<(usize, usize)> {
    let mut open: Vec<(usize, u8)> = Vec::new();
    let mut pairs = Vec::new();
    for &(at, byte) in brackets {
        let opens = match byte {
            b'}' => b'{',
            b']' => b'[',
            b')' => b'(',
            _ => {
                open.push((at, byte));
                continue;
            }
        };
        if let Some(&(opened, kind)) = open.last()
            && kind == opens
        {
            open.pop();
            pairs.extend([(opened, at), (at, opened)]);
        }
    }
    pairs.sort_unstable();
    pairs
}

// ---------------------------------------------------------------------------------------------
// The parse.
// ---------------------------------------------------------------------------------------------

/// Engine bits that change span / block IDs. Overlay or file loads must miss the cache.
#[derive(Clone, Copy, PartialEq, Eq)]
struct HighlightEngineKey {
    span_start: usize,
    num_blocks: usize,
    num_decls: usize,
}

impl HighlightEngineKey {
    fn of(engine_state: &EngineState) -> Self {
        Self {
            span_start: engine_state.next_span_start(),
            num_blocks: engine_state.num_blocks(),
            num_decls: engine_state.num_decls(),
        }
    }
}

/// Parse/flatten result reused across paints of the same line.
#[derive(Clone)]
pub(crate) struct ParsedHighlight {
    pub(crate) block: Arc<Block>,
    /// What the parser expected at the end of the line, which completion reads.
    pub(crate) probe: Arc<Probe>,
    /// What this parse added to the engine state, such as the blocks of
    /// closures and the commands the line defines.
    pub(crate) delta: Arc<StateDelta>,
    /// Errors from parsing and type checking the line, in the order found.
    pub(crate) errors: Arc<[ParseError]>,
    /// A `use` in the line was parsed without loading its module, so the
    /// names it imports are unknown to this parse.
    pub(crate) skipped_module_load: bool,
    shapes: Arc<Vec<(Span, FlatShape)>>,
    /// The line cut into the pieces that are painted, which need not be cut again unless the
    /// line changes.
    layout: Arc<Layout>,
    pub(crate) global_span_offset: usize,
}

impl ParsedHighlight {
    /// A working set that sees what the parse added, for walking `block`.
    pub(crate) fn working_set<'a>(&self, engine_state: &'a EngineState) -> StateWorkingSet<'a> {
        let mut working_set = StateWorkingSet::new(engine_state);
        working_set.delta = StateDelta::clone(&self.delta);
        working_set
    }
}

struct HighlightCache {
    line: String,
    engine: HighlightEngineKey,
    parsed: ParsedHighlight,
}

/// The last parse of the line being edited, shared by the highlighter and the
/// status line so a keystroke parses the line once.
#[derive(Clone, Default)]
pub(crate) struct LineParseCache(Arc<Mutex<Option<HighlightCache>>>);

impl LineParseCache {
    /// The parse of `line`, from the cache when the last one was of the same
    /// line against the same engine state.
    pub(crate) fn parsed(&self, engine_state: &EngineState, line: &str) -> ParsedHighlight {
        let engine = HighlightEngineKey::of(engine_state);
        if let Some(cache) = lock(&self.0).as_ref()
            && cache.line == line
            && cache.engine == engine
        {
            return cache.parsed.clone();
        }

        let parsed = parse_highlight_line(engine_state, line);
        *lock(&self.0) = Some(HighlightCache {
            line: line.to_string(),
            engine,
            parsed: parsed.clone(),
        });
        parsed
    }
}

#[cfg(test)]
impl LineParseCache {
    /// Whether the last parse kept is of `line`.
    pub(crate) fn holds(&self, line: &str) -> bool {
        lock(&self.0)
            .as_ref()
            .is_some_and(|cache| cache.line == line)
    }
}

fn parse_highlight_line(engine_state: &EngineState, line: &str) -> ParsedHighlight {
    trace!("highlighting: {line}");

    let mut working_set = StateWorkingSet::new(engine_state);
    // Color `use` without parse-time-loading modules (the `d` hitch on `use std`).
    working_set.skip_module_load = true;
    let (block, probe) = parse_probed(&mut working_set, None, line.as_bytes(), line.len());
    let shapes = flatten_block(&working_set, &block);
    let global_span_offset = engine_state.next_span_start();
    let layout = Layout::new(line, &shapes, global_span_offset);
    ParsedHighlight {
        block,
        probe: Arc::new(probe),
        errors: working_set.parse_errors.into(),
        skipped_module_load: working_set.skipped_module_load,
        delta: Arc::new(working_set.delta),
        shapes: Arc::new(shapes),
        layout: Arc::new(layout),
        global_span_offset,
    }
}

// ---------------------------------------------------------------------------------------------
// Kept between paints.
// ---------------------------------------------------------------------------------------------

/// What the line's externals were found to be on `PATH`, kept briefly: a program resolves or it
/// does not, and searching every directory of `PATH` again on each keystroke buys nothing.
#[derive(Default)]
struct ResolvedExternals {
    /// The directory and `PATH` the answers are for. Another one drops them all.
    surroundings: Option<(Option<AbsolutePathBuf>, Option<String>)>,
    seen: HashMap<String, (Instant, bool)>,
}

/// The styles of one configuration by `color_config` key, each worked out once.
#[derive(Default)]
struct ShapeStyles(Mutex<HashMap<&'static str, Style>>);

impl ShapeStyles {
    fn get(&self, key: &'static str, config: &Config) -> Style {
        *lock(&self.0)
            .entry(key)
            .or_insert_with(|| get_shape_color(key, config))
    }
}

/// What a highlighter keeps from one paint to the next.
#[derive(Default)]
struct Caches {
    /// The styles of the configuration last painted with.
    styles: Mutex<Option<(Arc<Config>, Arc<ShapeStyles>)>>,
    externals: Mutex<ResolvedExternals>,
}

impl Caches {
    /// The styles of `config`: those of the last paint if it was with the same configuration.
    fn styles(&self, config: &Arc<Config>) -> Arc<ShapeStyles> {
        let mut kept = lock(&self.styles);
        match kept.as_ref() {
            Some((seen, styles)) if Arc::ptr_eq(seen, config) => Arc::clone(styles),
            _ => {
                let styles = Arc::new(ShapeStyles::default());
                *kept = Some((Arc::clone(config), Arc::clone(&styles)));
                styles
            }
        }
    }

    /// Whether the external whose word is `raw_span` resolves, as [`external_is_resolved`] says,
    /// unless a paint a moment ago already asked.
    fn external_is_resolved(
        &self,
        engine_state: &EngineState,
        stack: &Stack,
        line: &str,
        global_span_offset: usize,
        raw_span: Span,
    ) -> bool {
        let word = span_text(line, global_span_offset, engine_state, raw_span).into_owned();
        let surroundings = (
            engine_state.cwd(Some(stack)).ok(),
            env::path_str(engine_state, stack, raw_span).ok(),
        );
        {
            let mut kept = lock(&self.externals);
            if kept.surroundings.as_ref() != Some(&surroundings) {
                kept.seen.clear();
                kept.surroundings = Some(surroundings);
            }
            if let Some(&(at, resolved)) = kept.seen.get(&word)
                && at.elapsed() < EXTERNAL_LIFETIME
            {
                return resolved;
            }
        }
        // The search is the slow part, and the lock is not held for it.
        let resolved =
            external_is_resolved(engine_state, stack, line, global_span_offset, raw_span);
        let mut kept = lock(&self.externals);
        if kept.seen.len() >= EXTERNALS_KEPT {
            kept.seen.clear();
        }
        kept.seen.insert(word, (Instant::now(), resolved));
        resolved
    }
}

// ---------------------------------------------------------------------------------------------
// The highlighter.
// ---------------------------------------------------------------------------------------------

pub struct NuHighlighter {
    pub engine_state: Arc<EngineState>,
    pub stack: Arc<Stack>,
    cache: LineParseCache,
    caches: Caches,
}

impl NuHighlighter {
    pub fn new(engine_state: Arc<EngineState>, stack: Arc<Stack>) -> Self {
        Self {
            engine_state,
            stack,
            cache: LineParseCache::default(),
            caches: Caches::default(),
        }
    }

    /// The parse cache this highlighter fills, to share with the status line.
    pub(crate) fn parse_cache(&self) -> LineParseCache {
        self.cache.clone()
    }
}

impl Highlighter for NuHighlighter {
    fn highlight(&self, line: &str, cursor: usize) -> StyledText {
        let parsed = self.parsed_line(line);
        style_parsed(
            &self.engine_state,
            &self.stack,
            line,
            Cursor::Editing(cursor),
            &parsed,
            &self.caches,
        )
        .text
    }

    fn should_expand_abbr(&self, line: &str, cursor: usize, context: AbbrExpandContext) -> bool {
        let parsed = self.parsed_line(line);
        let global_cursor = cursor + parsed.global_span_offset;
        !parsed.shapes.iter().any(|(span, shape)| {
            span.contains(global_cursor)
                && match context {
                    AbbrExpandContext::WordAbbreviation => matches!(
                        shape,
                        FlatShape::String
                            | FlatShape::RawString
                            | FlatShape::StringInterpolation
                            | FlatShape::ExternalArg
                    ),
                    AbbrExpandContext::BangExpansion => false,
                }
        })
    }
}

impl NuHighlighter {
    fn parsed_line(&self, line: &str) -> ParsedHighlight {
        self.cache.parsed(&self.engine_state, line)
    }
}

/// Result of a syntax highlight operation
#[derive(Default)]
pub(crate) struct HighlightResult {
    pub(crate) text: StyledText,
    pub(crate) found_garbage: Option<Span>,
}

/// Where the cursor is, and what that means for a paint.
#[derive(Clone, Copy)]
enum Cursor {
    /// A line being edited: a token the parser rejects is not drawn as an error while the cursor
    /// is on it, or the line ends in it, as it is probably not finished.
    Editing(usize),
    /// A line as it stands, such as one `nu-highlight` is given: every error is drawn.
    Settled(usize),
}

impl Cursor {
    fn at(self) -> usize {
        match self {
            Self::Editing(at) | Self::Settled(at) => at,
        }
    }
}

pub(crate) fn highlight_syntax(
    engine_state: &EngineState,
    stack: &Stack,
    line: &str,
    cursor: usize,
) -> HighlightResult {
    let parsed = parse_highlight_line(engine_state, line);
    style_parsed(
        engine_state,
        stack,
        line,
        Cursor::Settled(cursor),
        &parsed,
        &Caches::default(),
    )
}

/// Whether the rejected token at `start..end` of `line` may yet be finished by a typist at
/// `cursor`: the cursor is in it or at its end, or the line ends in it and the cursor is beyond.
fn is_pending(line: &str, start: usize, end: usize, cursor: usize) -> bool {
    (start..=end).contains(&cursor) || (cursor > end && line[end..].trim().is_empty())
}

fn style_parsed(
    engine_state: &EngineState,
    stack: &Stack,
    line: &str,
    cursor: Cursor,
    parsed: &ParsedHighlight,
    caches: &Caches,
) -> HighlightResult {
    let config = stack.get_config(engine_state);
    let styles = caches.styles(&config);
    let layout = parsed.layout.as_ref();
    let mut result = HighlightResult::default();
    result.text.buffer.reserve(layout.segments.len());

    let mut at = cursor.at().min(line.len());
    while !line.is_char_boundary(at) {
        at -= 1;
    }
    let marks = layout.matching(line, at);
    let mark_style = |style: Style| get_matching_brackets_style(style, &config);

    // Neighbours of one style are one piece of text.
    let mut push = |style: Style, from: usize, to: usize| {
        if from >= to {
            return;
        }
        let text = &line[from..to];
        match result.text.buffer.last_mut() {
            Some((last, buffer)) if *last == style => buffer.push_str(text),
            _ => result.text.buffer.push((style, text.to_owned())),
        }
    };

    for &Segment { start, end, kind } in &layout.segments {
        let style = match kind {
            Kind::Gap => Style::new(),
            // Highlighting externals has a config point because of concerns that using which to
            // resolve externals may slow down things too much.
            Kind::External(raw) => {
                let resolved = config.highlight_resolved_externals
                    && caches.external_is_resolved(
                        engine_state,
                        stack,
                        line,
                        parsed.global_span_offset,
                        raw,
                    );
                styles.get(
                    match resolved {
                        true => "shape_external_resolved",
                        false => "shape_external",
                    },
                    &config,
                )
            }
            Kind::Shape("shape_garbage") => {
                result
                    .found_garbage
                    .get_or_insert_with(|| Span::new(start, end));
                match pending(&line[start..end]) {
                    pending
                        if matches!(cursor, Cursor::Editing(_))
                            && is_pending(line, start, end, at) =>
                    {
                        pending.map_or_else(Style::new, |key| styles.get(key, &config))
                    }
                    _ => styles.get("shape_garbage", &config),
                }
            }
            Kind::Shape(key) => styles.get(key, &config),
        };
        let mut from = start;
        for &mark in marks
            .iter()
            .flatten()
            .filter(|&&m| (start..end).contains(&m))
        {
            push(style, from, mark);
            push(mark_style(style), mark, mark + 1);
            from = mark + 1;
        }
        push(style, from, end);
    }

    result
}

/// Contents of `span` for `which`: the current line when the span is on it,
/// otherwise engine-state text (aliased externals).
fn span_text<'a>(
    line: &'a str,
    global_span_offset: usize,
    engine_state: &'a EngineState,
    span: Span,
) -> Cow<'a, str> {
    let start = span.start.saturating_sub(global_span_offset);
    let end = span.end.saturating_sub(global_span_offset);
    if span.start >= global_span_offset && end <= line.len() && start <= end {
        Cow::Borrowed(&line[start..end])
    } else {
        String::from_utf8_lossy(engine_state.get_span_contents(span))
    }
}

pub(crate) fn external_is_resolved(
    engine_state: &EngineState,
    stack: &Stack,
    line: &str,
    global_span_offset: usize,
    raw_span: Span,
) -> bool {
    let word = span_text(line, global_span_offset, engine_state, raw_span);
    let paths = env::path_str(engine_state, stack, raw_span).ok();
    let word_os = OsStr::new(word.as_ref());
    let paths_os = paths.as_deref().map(OsStr::new);
    if let Ok(cwd) = engine_state.cwd(Some(stack)) {
        which::which_in(word_os, paths_os, cwd).is_ok()
    } else {
        which::which_in_global(word_os, paths_os)
            .ok()
            .and_then(|mut i| i.next())
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nu_protocol::default_color_config;
    use reedline::{AbbrExpandContext, Highlighter};
    use rstest::rstest;

    fn make_highlighter() -> NuHighlighter {
        NuHighlighter::new(Arc::new(EngineState::new()), Arc::new(Stack::new()))
    }

    /// A highlighter that knows the shell's commands, for lines whose arguments are parsed.
    fn shell_highlighter() -> NuHighlighter {
        let engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        NuHighlighter::new(Arc::new(engine_state), Arc::new(Stack::new()))
    }

    /// The default style of the `color_config` key `key`.
    fn style(key: &str) -> Style {
        get_shape_color(key, &Config::default())
    }

    /// The style of each byte in `range` of `line` painted with the cursor at `cursor`.
    fn styles_at(
        h: &NuHighlighter,
        line: &str,
        cursor: usize,
        range: std::ops::Range<usize>,
    ) -> Vec<Style> {
        let mut styles = Vec::new();
        for (style, text) in h.highlight(line, cursor).buffer {
            styles.extend(std::iter::repeat_n(style, text.len()));
        }
        styles[range].to_vec()
    }

    /// The positions of the characters of `line` painted with an underline, as the matching
    /// brackets are, when the cursor is at `cursor`.
    fn underlined(h: &NuHighlighter, line: &str, cursor: usize) -> Vec<usize> {
        let styles = styles_at(h, line, cursor, 0..line.len());
        (0..line.len())
            .filter(|&at| styles[at].is_underline)
            .collect()
    }

    #[test]
    fn highlight_reuses_parse_cache_for_the_same_line() {
        let h = make_highlighter();
        let line = "let x = [1, 2]";
        let first = h.highlight(line, 0);
        let second = h.highlight(line, 0);
        assert_eq!(first.render_simple(), second.render_simple());
        assert_eq!(first.raw_string(), line);

        // Cursor-only change must restyle matching brackets without re-parsing.
        let on_open = h.highlight(line, 8);
        let on_open_again = h.highlight(line, 8);
        assert_eq!(on_open.render_simple(), on_open_again.render_simple());
        assert_eq!(on_open.raw_string(), line);
    }

    /// A bracket inside a negated operand still finds its match, with the cursor after it.
    #[rstest]
    #[case::unary_minus("-(1 + 2)")]
    #[case::not_comparison("let a = true; not $a == (1 == 2)")]
    fn matching_brackets_inside_negation(#[case] line: &str) {
        let h = make_highlighter();
        let styled = h.highlight(line, line.len());
        let underlined: String = styled
            .buffer
            .iter()
            .filter(|(style, _)| style.is_underline)
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(underlined, "()", "matching brackets in {line:?}");
    }

    #[test]
    fn highlight_slices_cover_the_whole_line() {
        let h = make_highlighter();
        for line in [
            "",
            "ls",
            "use std/iter",
            "1 + 2",
            "{a: 1}",
            "def f [] { 1 }",
        ] {
            let styled = h.highlight(line, line.len());
            assert_eq!(
                styled.raw_string(),
                line,
                "lost text while styling {line:?}"
            );
        }
    }

    #[rstest]
    // 4-byte emoji
    #[case("\"hello 🎉\" hi", 7, false)] // first byte of 🎉
    #[case("\"hello 🎉\" hi", 9, false)] // third byte of 🎉
    #[case("\"hello 🎉\" hi", 13, true)] // after closing quote
    // 8-byte zwj emoji
    #[case("\"hello 🤝🏿\" hi", 9, false)] // inside 🤝
    #[case("\"hello 🤝🏿\" hi", 11, false)] // first byte of 🏿
    #[case("\"hello 🤝🏿\" hi", 13, false)] // inside 🏿
    #[case("\"hello 🤝🏿\" hi", 17, true)] // after closing quote
    // 3-byte unicode
    #[case("\"こんにちは\" hi", 2, false)] // inside こ
    #[case("\"こんにちは\" hi", 5, false)] // inside ん
    #[case("\"こんにちは\" hi", 13, false)] // start of は
    #[case("\"こんにちは\" hi", 18, true)] // after closing quote
    // raw string
    #[case("r#'hello'# hi", 4, false)] // inside 'e'
    #[case("r#'hello'# hi", 11, true)] // after closing #
    // string interpolation
    #[case("$\"hello\" hi", 0, false)] // $ — opening StringInterpolation span (0..2)
    #[case("$\"hello\" hi", 4, false)] // inside literal 'hello'
    #[case("$\"hello\" hi", 9, true)] // after closing quote
    // no string
    #[case("1 + 2", 0, true)]
    #[case("1 + 2", 2, true)]
    // suppress abbreviation expansion in external commands
    #[case("ls -la", 0, true)] // on 'ls'  — FlatShape::External
    #[case("ls -la", 3, false)] // on '-la' — FlatShape::ExternalArg
    #[case("bash -c \"echo hello\"", 0, true)] // on 'bash'            — FlatShape::External
    #[case("bash -c \"echo hello\"", 5, false)] // on '-c'              — FlatShape::ExternalArg
    #[case("bash -c \"echo hello\"", 10, false)] // inside "echo hello"  — FlatShape::ExternalArg
    fn test_should_expand_word_abbr(
        #[case] line: &str,
        #[case] cursor: usize,
        #[case] expected: bool,
    ) {
        let h = make_highlighter();
        assert_eq!(
            h.should_expand_abbr(line, cursor, AbbrExpandContext::WordAbbreviation),
            expected
        );
    }

    #[rstest]
    // bare bang expressions allow expansion
    #[case("!!", 0, true)]
    #[case("!!", 1, true)]
    #[case("!ls", 0, true)]
    #[case("!ls", 2, true)]
    #[case("!-1", 1, true)]
    // bang inside string literals does not suppress expansion
    #[case("\"!!\"", 1, true)]
    #[case("\"!ls\"", 2, true)]
    #[case("r#'!!'#", 3, true)]
    #[case("$\"!!\"", 2, true)]
    // bang as external arg does not suppress expansion
    #[case("bash -c !!", 9, true)]
    #[case("bash -c !ls", 9, true)]
    // bang inside a string that is itself an external arg — shape is ExternalArg, not String.
    // currently there is no way to avoid this
    #[case("echo \"hi !!\"", 9, true)]
    fn test_should_expand_abbr_bang(
        #[case] line: &str,
        #[case] cursor: usize,
        #[case] expected: bool,
    ) {
        let h = make_highlighter();
        assert_eq!(
            h.should_expand_abbr(line, cursor, AbbrExpandContext::BangExpansion),
            expected
        );
    }

    /// The shapes the parser flattens finely have default styles of their own, apart from those
    /// of the int and the string they would otherwise be read as.
    #[test]
    fn the_fine_shapes_have_styles_of_their_own() {
        let defaults = default_color_config();
        let coarse = [
            style("shape_int"),
            style("shape_string"),
            style("shape_operator"),
        ];
        for key in [
            "shape_filesize",
            "shape_duration",
            "shape_cell_path",
            "shape_type",
        ] {
            assert!(defaults.contains_key(key), "{key}");
            assert!(!coarse.contains(&style(key)), "{key}");
        }
        assert_ne!(style("shape_type"), style("shape_signature"));
    }

    /// Whatever the line, its segments follow one another from its start to its end.
    #[rstest]
    #[case::empty("")]
    #[case::unfinished_string("echo \"abc")]
    #[case::unfinished_block("if true { 1 } else {")]
    #[case::empty_block("let a = ()")]
    #[case::unicode("$\"é(1 + 2)é\" | str trim # ü")]
    #[case::signature("def f [a: oneof<int, string>, --flag(-f): int = 5, ...rest] { }")]
    #[case::alias("alias ll = ls -l; ll")]
    fn the_layout_covers_the_line(#[case] line: &str) {
        let engine_state = shell_highlighter().engine_state;
        let layout = parse_highlight_line(&engine_state, line).layout;
        let mut at = 0;
        for segment in &layout.segments {
            assert_eq!(segment.start, at, "{:?} in {line:?}", layout.segments);
            assert!(segment.end > segment.start, "{segment:?} in {line:?}");
            at = segment.end;
        }
        assert_eq!(at, line.len());
    }

    /// The `nth` `text` of `line` is painted in the style of `key`: a number and its unit as one
    /// value, the operators of a range as range operators, the members of a cell path (a row
    /// condition's column too) as members, and a signature by its parts: the choices of a `oneof`
    /// are types, the commas and brackets between them operators, the names of a record type's
    /// fields names.
    #[rstest]
    #[case("10kb", "10kb", 0, "shape_filesize")]
    #[case("5min", "5min", 0, "shape_duration")]
    #[case("1..10", "..", 0, "shape_range")]
    #[case("1..10", "10", 0, "shape_int")]
    #[case("1..2..10", "..", 0, "shape_range")]
    #[case("1..2..10", "..", 1, "shape_range")]
    #[case("$env.PATH.0", "PATH", 0, "shape_cell_path")]
    #[case("$env.PATH.0", "0", 0, "shape_cell_path")]
    #[case("ls | get name", "name", 0, "shape_cell_path")]
    #[case("ls | where size > 1", "size", 0, "shape_cell_path")]
    #[case(ONEOF, "oneof", 0, "shape_type")]
    #[case(ONEOF, "<", 0, "shape_operator")]
    #[case(ONEOF, "int", 0, "shape_type")]
    #[case(ONEOF, "string", 0, "shape_type")]
    #[case(ONEOF, ",", 0, "shape_operator")]
    #[case(ONEOF, ",", 1, "shape_signature")]
    #[case(ONEOF, "--flag", 0, "shape_flag")]
    #[case(ONEOF, "-f)", 0, "shape_flag")]
    #[case(ONEOF, "5", 0, "shape_int")]
    #[case(ONEOF, "true", 0, "shape_bool")]
    #[case(ONEOF, "a:", 0, "shape_signature")]
    #[case(RECORD, "name", 0, "shape_signature")]
    #[case(RECORD, "string", 0, "shape_type")]
    #[case(RECORD, "size", 0, "shape_signature")]
    #[case(RECORD, "int", 0, "shape_type")]
    fn shapes_are_painted_by_their_key(
        #[case] line: &str,
        #[case] text: &str,
        #[case] nth: usize,
        #[case] key: &str,
    ) {
        let h = shell_highlighter();
        let (at, _) = line
            .match_indices(text)
            .nth(nth)
            .expect("the text is in the line");
        let text = text.trim_end_matches(')');
        let painted = styles_at(&h, line, 0, at..at + text.len());
        assert_eq!(
            painted,
            vec![style(key); text.len()],
            "{text:?} in {line:?}"
        );
    }

    const ONEOF: &str = "def f [a: oneof<int, string>, b?: int = 5, --flag(-f): bool = true] { }";
    const RECORD: &str = "def f [r: record<name: string, size: int>] { }";

    /// The brackets of the pair under the cursor, as positions in the line.
    #[rstest]
    #[case::block_at_the_end("if true { 1 } else { 2 }", 23, &[19, 23])]
    #[case::keyword_branch_on_open("if true { 1 } else { 2 }", 19, &[19, 23])]
    #[case::match_arms("let x = 1; match $x { 1 => 2 }", 20, &[20, 29])]
    #[case::empty_subexpression("let a = ()", 8, &[8, 9])]
    #[case::just_passed("(1 + 2) | math abs", 7, &[0, 6])]
    #[case::list("[1, [2], 3]", 0, &[0, 10])]
    #[case::inner_list("[1, [2], 3]", 4, &[4, 6])]
    #[case::not_the_one_in_a_string("echo \"(\" (1)", 9, &[9, 11])]
    #[case::not_the_one_in_a_comment("(1) # (", 0, &[0, 2])]
    #[case::signature("def f [a: int] { }", 6, &[6, 13])]
    #[case::unclosed("(1 + 2", 0, &[])]
    #[case::mismatched("(1 + 2]", 0, &[])]
    #[case::not_on_a_bracket("(1 + 2)", 3, &[])]
    #[case::nothing_before_the_start("1 + (2)", 0, &[])]
    fn the_pair_under_the_cursor_is_underlined(
        #[case] line: &str,
        #[case] cursor: usize,
        #[case] expected: &[usize],
    ) {
        let h = shell_highlighter();
        assert_eq!(
            underlined(&h, line, cursor),
            expected,
            "{line:?} at {cursor}"
        );
    }

    /// A token the parser rejects is not an error while it is being typed, only once the cursor
    /// has gone on.
    #[test]
    fn an_unfinished_token_is_not_drawn_as_an_error() {
        let h = shell_highlighter();
        let garbage = style("shape_garbage");
        for (line, cursor) in [
            ("echo $e", 7),
            ("1 +", 3),
            ("1 + ", 4),
            ("$", 1),
            ("echo $e", 6),
        ] {
            let painted = styles_at(&h, line, cursor, 0..line.len());
            assert!(
                !painted.contains(&garbage),
                "{line:?} at {cursor} is drawn as an error"
            );
        }
        // The variable being typed is drawn as the variable it is on the way to being.
        let variable = style("shape_variable");
        assert_eq!(styles_at(&h, "echo $e", 7, 5..7), [variable, variable]);

        // Once the cursor has left it, or there is more after it, it is one.
        for (line, cursor) in [("echo $e | ls", 12), ("echo $e", 2), ("echo $e | ls", 8)] {
            let painted = styles_at(&h, line, cursor, 5..7);
            assert_eq!(painted, [garbage, garbage], "{line:?} at {cursor}");
        }
    }

    /// `nu-highlight` and the like draw a line as it stands, errors and all.
    #[test]
    fn a_settled_line_draws_every_error() {
        let engine_state = shell_highlighter().engine_state;
        let stack = Stack::new();
        let result = highlight_syntax(&engine_state, &stack, "echo $e", 7);
        assert_eq!(result.found_garbage, Some(Span::new(5, 7)));
        let (style, text) = &result.text.buffer[result.text.buffer.len() - 1];
        assert_eq!(
            (text.as_str(), *style),
            ("$e", self::style("shape_garbage"))
        );
    }

    /// Moving the cursor over text with no bracket in it repaints nothing: no flicker, and no
    /// shifting of what is drawn.
    #[test]
    fn the_cursor_moves_over_plain_text_without_a_change() {
        let h = shell_highlighter();
        let line = "ls | where size > 10kb | get name # é";
        let first = h.highlight(line, 0).buffer;
        for cursor in (0..=line.len()).filter(|&at| line.is_char_boundary(at)) {
            assert_eq!(
                h.highlight(line, cursor).buffer,
                first,
                "cursor at {cursor}"
            );
        }
    }

    /// Typing a line one character at a time paints each prefix whole, wherever the cursor is.
    #[rstest]
    #[case("def f [a: oneof<int, string>] { \"é\" | str trim }")]
    #[case("ls | where size > 10kb # (c")]
    #[case("$\"é(1 + 2)é\" | [1 ..2] | get 0.é")]
    #[case("r#'a'# | { a: [1 2] } | to nuon")]
    fn every_prefix_of_a_line_is_painted_whole(#[case] line: &str) {
        let h = shell_highlighter();
        for end in (0..=line.len()).filter(|&at| line.is_char_boundary(at)) {
            let prefix = &line[..end];
            for cursor in (0..=end).filter(|&at| prefix.is_char_boundary(at)) {
                assert_eq!(h.highlight(prefix, cursor).raw_string(), prefix);
            }
        }
    }

    #[test]
    fn the_styles_of_a_configuration_are_built_once() {
        let caches = Caches::default();
        let config = Arc::new(Config::default());
        let first = caches.styles(&config);
        assert!(Arc::ptr_eq(&first, &caches.styles(&config)));
        assert!(!Arc::ptr_eq(
            &first,
            &caches.styles(&Arc::new(Config::default()))
        ));
    }

    #[test]
    fn an_external_is_looked_for_on_path_once_in_a_while() {
        let engine_state = EngineState::new();
        let stack = Stack::new();
        let caches = Caches::default();
        let line = "definitely-not-a-program-here";
        let span = Span::new(
            engine_state.next_span_start(),
            engine_state.next_span_start() + line.len(),
        );
        let resolved = |caches: &Caches| {
            caches.external_is_resolved(&engine_state, &stack, line, span.start, span)
        };
        assert!(!resolved(&caches));
        assert!(lock(&caches.externals).seen.contains_key(line));
        assert!(!resolved(&caches));
        assert_eq!(lock(&caches.externals).seen.len(), 1);
    }
}
