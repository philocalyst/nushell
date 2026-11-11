//! The REPL's status line: a row under the line being edited that says what the line does not
//! already say, drawn under the text it is about. It starts at the parameter the cursor is at and
//! holds what is still to come, as what is written and the command's name are right above it.
//! In a row condition it is how many rows pass, and after a `|` what is handed on. What is wrong
//! with the line is drawn on the words at fault, with the words for it here while the cursor is
//! on them. A line of const commands shows its value.

use crate::{
    completions::{catch_completion_panic, evaluate_text, failure_of, rank::damerau_levenshtein},
    syntax_highlight::{LineParseCache, ParsedHighlight, external_is_resolved},
    validation::is_incomplete,
};
use miette::Diagnostic;
use nu_ansi_term::{Color, Style};
use nu_color_config::StyleComputer;
use nu_parser::{FlatShape, TokenContents, lex, math_result_type, parse_probed};
use nu_protocol::{
    BlockId, Config, DeclId, ParseError, PipelineData, Signals, Span, StatusLineConfig,
    SyntaxShape, Type, Value,
    ast::{Argument, Block, Call, Expr, Expression, Operator, PipelineElement, Traverse},
    engine::{
        EngineState, Expect, Probe, Stack, StateDelta, StateWorkingSet,
        probe::{innermost_call, touches},
    },
    eval_const::eval_const_subexpression,
};
use nu_utils::strip_ansi_unlikely;
use reedline::{RepaintSignal, Sign};
use std::{
    ops::ControlFlow,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// How long a paint waits for a const preview before interrupting it.
const PREVIEW_BUDGET: Duration = Duration::from_millis(25);

/// The stack of the preview thread, as roomy as nu-dap's evaluation thread.
/// Const evaluation recurses for each level of nesting, and a line nested
/// deep enough to overflow the default 2 MiB would abort the REPL.
const PREVIEW_STACK_SIZE: usize = 32 * 1024 * 1024;

/// The most graphemes of a preview kept: more than a row of `u16::MAX`
/// columns, the widest a terminal reports, so `render` still decides where
/// the ellipsis goes.
const PREVIEW_GRAPHEMES: usize = u16::MAX as usize;

/// Between the signature, the type and the preview.
const GAP: &str = "   ";

/// Ahead of the status, to set it apart from output.
const INDENT: &str = "  ";

/// Ties the status to the word above it.
const TIE: &str = "╰ ";

/// The [`reedline::StatusLine`] of the REPL, built for each prompt.
pub(crate) struct NuStatusLine {
    engine_state: Arc<EngineState>,
    parse_cache: LineParseCache,
    config: StatusLineConfig,
    styles: StatusStyles,
    preview: ConstPreview,
    rows: OffThePaint<Type>,
    trials: OffThePaint<String>,
    /// How many rows a pipeline gives, for what a condition lets through.
    counts: OffThePaint<usize>,
    /// What the last condition to run let through, for the pipeline before it: `(input, passed,
    /// of)`. It stays, dimmed, while the condition is being written and does not run.
    passed: Mutex<Option<(String, usize, usize)>>,
    programs: Programs,
    /// Whether the sign's note shows on this paint, which takes the room of what the parameter
    /// being typed is for.
    noted: bool,
    /// The width of the screen the status was last asked for.
    columns: u16,
    /// Where in the line the command starts whose signature the status last showed, for it to be
    /// drawn under.
    anchored: std::cell::Cell<Option<usize>>,
}

/// What is worked out of pipelines by running them, kept by their text: the types of the rows
/// one gives, which say what a condition's columns hold when the commands before it do not
/// declare it (as `ls` does not), or why one fails. Each pipeline runs once, off the paint, as
/// completion reads data (only one that only reads, within a budget). A paint before it is done
/// shows nothing of it, and the paint asked for once it is shows it.
struct OffThePaint<T> {
    known: Arc<std::sync::Mutex<std::collections::HashMap<String, Option<T>>>>,
    /// How many pipelines are being worked out right now.
    running: Arc<std::sync::atomic::AtomicUsize>,
    /// Asks for the paint that shows what was worked out.
    repaint: Option<RepaintSignal>,
}

impl<T> Default for OffThePaint<T> {
    fn default() -> Self {
        Self {
            known: Arc::default(),
            running: Arc::default(),
            repaint: None,
        }
    }
}

impl<T> OffThePaint<T> {
    /// Whether nothing is being worked out, so a paint now shows all there is to show.
    fn is_idle(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Acquire) == 0
    }
}

impl<T: Clone + Send + 'static> OffThePaint<T> {
    /// What `work` made of `text`, if it is done; otherwise it starts it.
    fn get(
        &self,
        engine_state: &Arc<EngineState>,
        text: &str,
        work: fn(&EngineState, &str) -> Option<T>,
    ) -> Option<T> {
        let mut known = self.known.lock().ok()?;
        if let Some(found) = known.get(text) {
            return found.clone();
        }
        // Marked as being worked out, so that a paint while it is does not start it again.
        known.insert(text.to_owned(), None);
        drop(known);
        let (engine_state, text, known, running, repaint) = (
            Arc::clone(engine_state),
            text.to_owned(),
            Arc::clone(&self.known),
            Arc::clone(&self.running),
            self.repaint.clone(),
        );
        running.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let spawned = thread::Builder::new()
            .name("status line run".into())
            .spawn(move || {
                let made = catch_completion_panic(|| work(&engine_state, &text))
                    .ok()
                    .flatten();
                let shows = made.is_some();
                if let Ok(mut known) = known.lock() {
                    known.insert(text, made);
                }
                running.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                if let Some(repaint) = repaint.filter(|_| shows) {
                    repaint.request_repaint();
                }
            });
        if spawned.is_err() {
            self.running
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
        None
    }
}

/// The type of the rows the pipeline `text` gives.
fn rows_of(engine_state: &EngineState, text: &str) -> Option<Type> {
    evaluate_text(engine_state, &Stack::new(), text).map(|value| value.get_type())
}

/// How many rows the pipeline `text` gives.
fn count_of(engine_state: &EngineState, text: &str) -> Option<usize> {
    let value = evaluate_text(engine_state, &Stack::new(), text)?;
    Some(match value.as_ref() {
        Value::List { vals, .. } => vals.len(),
        _ => 1,
    })
}

/// Why the pipeline `text` fails.
fn failure(engine_state: &EngineState, text: &str) -> Option<String> {
    failure_of(engine_state, &Stack::new(), text)
}

/// How the parts of the status line are drawn.
pub(crate) struct StatusStyles {
    /// Everything but what the styles below pick out.
    pub(crate) text: Style,
    /// The parameter the cursor is on.
    pub(crate) active: Style,
    /// An error, which takes the whole row.
    pub(crate) error: Style,
    /// The type of the parameter the cursor is on, in the colour the line gives a signature.
    pub(crate) types: Style,
}

impl StatusStyles {
    /// The status line drawn in `text`, with the parameter the cursor is on
    /// in bold and underlined, and errors in `error`.
    pub(crate) fn new(text: Style, error: Style) -> Self {
        Self {
            text,
            active: text.bold().underline(),
            error,
            types: text.bold().underline(),
        }
    }

    /// These styles, with types in the colour of `types`.
    pub(crate) fn with_types(self, types: Style) -> Self {
        let types = match types.foreground {
            Some(color) => self.active.fg(color),
            None => self.active,
        };
        Self { types, ..self }
    }

    /// The colour of types where `color_config.status_line_type` sets none: a muted sage, apart
    /// from the colours a prompt and the line's highlighting use.
    pub(crate) fn default_types() -> Style {
        Style::new().fg(Color::Fixed(108))
    }

    /// The styles `color_config` sets: `status_line`, or [`StatusStyles::default_text`] when it
    /// doesn't, and `status_line_error`, red unless set.
    pub(crate) fn from_config(config: &Config, style_computer: &StyleComputer) -> Self {
        // Like `hints` closures, these only get `null` passed in.
        let nothing = Value::nothing(Span::unknown());
        let text = if config.color_config.contains_key("status_line") {
            style_computer.compute("status_line", &nothing)
        } else {
            Self::default_text()
        };
        Self::new(text, style_computer.compute("status_line_error", &nothing)).with_types(
            if config.color_config.contains_key("status_line_type") {
                style_computer.compute("status_line_type", &nothing)
            } else {
                Self::default_types()
            },
        )
    }

    /// The style of the status line unless `color_config.status_line` is set: a muted steel blue
    /// of its own, apart from the dark gray of the history hints that would otherwise be taken
    /// for the same thing.
    pub(crate) fn default_text() -> Style {
        Style::new().fg(Color::Fixed(67))
    }
}

impl NuStatusLine {
    /// A status line over `engine_state` that reads the line from the parse
    /// the highlighter keeps in `parse_cache`.
    pub(crate) fn new(
        engine_state: Arc<EngineState>,
        parse_cache: LineParseCache,
        config: StatusLineConfig,
        styles: StatusStyles,
    ) -> Self {
        Self {
            engine_state,
            parse_cache,
            config,
            styles,
            preview: ConstPreview::default(),
            rows: OffThePaint::default(),
            trials: OffThePaint::default(),
            counts: OffThePaint::default(),
            passed: Mutex::default(),
            programs: Programs::default(),
            noted: false,
            columns: u16::MAX,
            anchored: std::cell::Cell::new(None),
        }
    }

    /// Whether every pipeline a paint started has been worked out, so the next paint is final.
    pub(crate) fn is_idle(&self) -> bool {
        self.rows.is_idle() && self.trials.is_idle() && self.counts.is_idle()
    }

    /// Asks for a paint through `repaint` once what a pipeline was run for is known.
    pub(crate) fn with_repaint(mut self, repaint: RepaintSignal) -> Self {
        self.rows.repaint = Some(repaint.clone());
        self.counts.repaint = Some(repaint.clone());
        self.trials.repaint = Some(repaint);
        self
    }

    /// The parts of the status for `line` with the cursor at byte `pos`.
    fn segments(&mut self, line: &str, pos: usize) -> Vec<(Style, String)> {
        // reedline keeps `pos` on a char boundary of `line`; a host that
        // doesn't gets no status rather than a panic.
        let Some(before_cursor) = line.get(..pos) else {
            return vec![];
        };
        if line.trim().is_empty() {
            return vec![];
        }
        let parsed = self.parse_cache.parsed(&self.engine_state, line);
        let offset = parsed.global_span_offset;
        let cursor = offset + pos;
        // Where the text typed before the cursor ends, so a cursor resting
        // after a space still belongs to the call or element before it.
        let typed = before_cursor.trim_end().len();
        // A `use` the parse didn't load leaves what it imports unknown, which
        // reads as errors the line doesn't have and changes what it computes.
        let imports_known = !parsed.skipped_module_load;

        let working_set = parsed.working_set(&self.engine_state);
        // What is wrong with the line is a sign at its end, and the sign's note, not this.

        let mut segments = vec![];
        // The last character typed before the cursor.
        if let Some(search) = typed.checked_sub(1).map(|i| offset + i) {
            if self.config.signature {
                let probe = self.probe(&parsed, line, pos);
                segments.extend(self.signature(line, &parsed, &working_set, &probe, cursor));
                let anchored = self.anchored.get().map(|at| at.saturating_sub(offset));
                self.anchored.set(anchored);
            }
            // What the line gives, once there is nothing more to say about what is being written:
            // a vague type under every word is only more to read past.
            if self.config.output_type
                && segments.is_empty()
                && let Some(ty) = output_type(&parsed.block, &working_set, search)
                && !matches!(ty, Type::List(inner) if matches!(**inner, Type::Any))
                && !matches!(ty, Type::OneOf(_))
            {
                self.anchored.set(Some(pos));
                segments.extend(self.tied());
                segments.push((self.styles.text, format!("→ {ty}")));
            }
        }
        if self.config.preview
            && imports_known
            && let Some(value) =
                self.preview
                    .preview(line, &parsed, &working_set, &self.engine_state)
        {
            push_part(&mut segments, self.styles.text, format!("= {value}"));
        }
        segments
    }

    /// What the parser expected at byte `pos` of `line`: the probe of the highlighter's parse
    /// when the cursor is at its end, as it nearly always is, or a parse of its own.
    fn probe(&self, parsed: &ParsedHighlight, line: &str, pos: usize) -> Arc<Probe> {
        if pos == line.len() {
            return Arc::clone(&parsed.probe);
        }
        let mut working_set = StateWorkingSet::new(&self.engine_state);
        working_set.skip_module_load = true;
        Arc::new(parse_probed(&mut working_set, None, line.as_bytes(), pos).1)
    }

    /// The signature of the call the probe finds the cursor in, from the parameter at `cursor`
    /// on, that parameter in the active style; or what the closure or condition being written is
    /// given, or what a `|` hands on.
    fn signature(
        &self,
        line: &str,
        parsed: &ParsedHighlight,
        working_set: &StateWorkingSet,
        probe: &Probe,
        cursor: usize,
    ) -> Vec<(Style, String)> {
        let (block, offset) = (&parsed.block, parsed.global_span_offset);
        // After a `|` what comes next is still to be chosen, and what it will be given says what
        // it can be.
        let typed = line[..cursor - offset].trim_end();
        if let Some(before) = typed
            .strip_suffix('|')
            .filter(|before| !before.ends_with('|'))
        {
            let pipe = offset + before.len();
            return self.handed_on(block, working_set, offset + before.trim_end().len(), pipe);
        }
        let Some(found) = probe.narrowest(true) else {
            return vec![];
        };
        let call = probe.narrowest(false);
        // A command being named, when nothing narrower holds the cursor: what it takes. A program
        // that runs has no signature, and a word that names nothing yet is on its way to a
        // command inside the call around it.
        if let Expect::Command { .. } | Expect::ExternalArg { .. } = found.what
            && call.is_none_or(|call| found.within.len() < call.within.len())
        {
            if let Some(named) =
                innermost_call(block, working_set, |call| touches(call.head, cursor))
            {
                self.anchored.set(Some(cursor));
                return self.parameters(working_set, named.decl_id, None);
            }
            let program = match found.what {
                Expect::Command { .. } => found.span,
                _ => {
                    let head = working_set.get_span_contents(found.within);
                    let len = head.iter().position(u8::is_ascii_whitespace);
                    Span::new(
                        found.within.start,
                        found.within.start + len.unwrap_or(head.len()),
                    )
                }
            };
            let caret = usize::from(working_set.get_span_contents(program).starts_with(b"^"));
            let program = Span::new(program.start + caret, program.end);
            if external_is_resolved(&self.engine_state, &Stack::new(), line, offset, program) {
                return vec![];
            }
        }
        let Some(found) = call else {
            return vec![];
        };
        // Drawn under the word being typed.
        let written = working_set.get_span_contents(Span::new(found.within.start, cursor));
        let word = written
            .rsplit(u8::is_ascii_whitespace)
            .next()
            .unwrap_or_default();
        self.anchored.set(Some(cursor - word.len()));
        let Some(decl_id) = found.decl_id() else {
            return vec![];
        };
        let call = innermost_call(block, working_set, |call| {
            call.head.start == found.within.start
        });
        let signature = working_set.get_decl(decl_id).signature();

        // In a condition, what it lets through so far, as long as only space follows it.
        if let Some(condition) = call
            .and_then(|call| call.arguments.last())
            .and_then(Argument::expr)
            .filter(|arg| matches!(arg.expr, Expr::RowCondition(_)))
            .filter(|arg| {
                arg.span.start < cursor
                    && (working_set.get_span_contents(Span::new(arg.span.end.min(cursor), cursor)))
                        .iter()
                        .all(u8::is_ascii_whitespace)
            })
        {
            self.anchored.set(Some(condition.span.start));
            return self.rows_that_pass(block, working_set, cursor);
        }
        let index = match &found.what {
            Expect::Positional { index, .. } => *index,
            // A flag says what its value takes; the name of it is right above, and one that is
            // still being typed is the completions' to name.
            what => {
                let flag = match what {
                    Expect::FlagValue { long, .. } => Some(long.as_str()),
                    _ => call.and_then(|call| {
                        call.arguments.iter().find_map(|arg| match arg {
                            Argument::Named((long, ..)) if touches(arg.span(), cursor) => {
                                Some(long.item.as_str())
                            }
                            _ => None,
                        })
                    }),
                };
                let Some(flag) = flag.and_then(|long| signature.get_long_flag(long)) else {
                    return vec![];
                };
                let Some(ty) = flag.arg.as_ref().and_then(shape_label) else {
                    return vec![];
                };
                let mut segments = self.tied();
                segments.extend(type_parts(&ty, self.styles.types, self.styles.active));
                return self.described(segments, flag.desc);
            }
        };
        // The argument being typed, or the keyword still awaiting its value (`let x = ⌶`).
        let argument = call.and_then(|call| {
            let mut positionals =
                (call.arguments.iter()).filter(|arg| !matches!(arg, Argument::Named(..)));
            (positionals.clone().find(|arg| arg.span() == found.span))
                .or_else(|| positionals.nth(index))
                .and_then(Argument::expr)
        });
        match argument.map(|arg| &arg.expr) {
            // A keyword that is written says no more than the line does.
            Some(Expr::Keyword(_)) => return vec![],
            // In a closure, what it is given, which its body is written from: under its brace.
            Some(Expr::Closure(_) | Expr::Block(_)) => {
                self.anchored.set(Some(found.span.start));
                let inner = innermost_block(block, working_set, found.within.start);
                let piped = element_at(inner, working_set, found.within.start);
                return self.given_to_closure(piped.and_then(|piped| piped.before));
            }
            _ => {}
        }
        // Past the first of a list of them, the next is for the same as the last.
        if index > signature.required_positional.len() + signature.optional_positional.len() {
            return vec![];
        }
        self.parameters(working_set, decl_id, Some(index))
    }

    /// The parameters of `decl_id` from the positional `index` on, that one in the active style
    /// with its type and what it is for; all of them in the text style when the command's name is
    /// being typed (no `index`).
    fn parameters(
        &self,
        working_set: &StateWorkingSet,
        decl_id: DeclId,
        index: Option<usize>,
    ) -> Vec<(Style, String)> {
        let signature = working_set.get_decl(decl_id).signature();
        let label = signature.usage();
        let Some(first) = label.positional_part(index.unwrap_or(0)) else {
            return vec![];
        };
        // What the parameter being typed takes and is for, which a preview need not list.
        let parameter = index
            .and_then(|index| signature.get_positional(index))
            .map(|arg| (shape_label(&arg.shape), arg.desc.clone()));

        // Each part still to come, as it is drawn.
        let mut parts: Vec<Vec<(Style, String)>> = Vec::new();
        for (at, part) in label.parts.into_iter().enumerate().skip(first) {
            if at != first || index.is_none() {
                parts.push(vec![(self.styles.text, part)]);
                continue;
            }
            // The parameter being typed says what it takes inside its brackets:
            // `<condition: bool>`.
            let last = part.char_indices().last().map_or(0, |(at, _)| at);
            let (open, close) = part.split_at(last);
            let mut segments = vec![(self.styles.active, open.to_owned())];
            // A type that is the parameter's name, as `<condition: condition>`, says nothing.
            let named = open.trim_start_matches(['<', '(', '{', '.']);
            if let Some(ty) = parameter
                .as_ref()
                .and_then(|(ty, _)| ty.as_ref())
                .filter(|ty| *ty != named)
            {
                segments.push((self.styles.active, ": ".to_owned()));
                segments.extend(type_parts(ty, self.styles.types, self.styles.active));
            }
            segments.push((self.styles.active, close.to_owned()));
            parts.push(segments);
        }
        // Then what the parameter is for, unless a note says more.
        let description = (!self.noted)
            .then(|| parameter.map(|(_, description)| description))
            .flatten()
            .and_then(|description| description.lines().next().map(str::to_owned))
            .filter(|line| !line.is_empty());
        let budget =
            usize::from(self.columns).saturating_sub(INDENT.len() + TIE.chars().count() + 1);
        let mut segments = self.tied();
        segments.extend(fit(parts, description, budget, self.styles.text));
        segments
    }

    /// What a closure is given: the rows of what is piped into the call it is an argument of,
    /// as its type says or running the pipeline `before` it finds; the columns of each row, or
    /// its type. Nothing when that is not known.
    fn given_to_closure(&self, before: Option<(&Expression, String)>) -> Vec<(Style, String)> {
        let Some((piped, before)) = before else {
            return vec![];
        };
        let Some(ty) =
            known_rows(&piped.ty).or_else(|| self.rows.get(&self.engine_state, &before, rows_of))
        else {
            return vec![];
        };
        let mut segments = self.tied();
        segments.push((self.styles.types, "$in".to_owned()));
        segments.push((self.styles.text, ": ".to_owned()));
        // Each row is what a closure is run on.
        let Some(shape) = self.shape(&ty, true) else {
            return vec![];
        };
        segments.extend(shape);
        segments
    }

    /// How a type reads where the status says what is handed on: the columns of a table or a
    /// record inside its brackets, `table<name, size>`, what a row of a table is as `record<..>`
    /// when `row`, and any other type as it is. `None` for one that says nothing.
    fn shape(&self, ty: &Type, row: bool) -> Option<Vec<(Style, String)>> {
        let around = self.styles.text;
        let (kind, columns) = match ty {
            Type::Table(columns) if !columns.is_empty() => {
                (if row { "record" } else { "table" }, columns)
            }
            Type::Record(columns) if !columns.is_empty() => ("record", columns),
            Type::List(inner) if row && !matches!(**inner, Type::Any) => {
                return self.shape(inner, false);
            }
            Type::Any | Type::Nothing => return None,
            Type::List(inner) if matches!(**inner, Type::Any) => return None,
            ty => return Some(type_parts(&ty.to_string(), self.styles.types, around)),
        };
        let mut segments = vec![
            (self.styles.types, kind.to_owned()),
            (around, "<".to_owned()),
        ];
        for (at, (name, _)) in columns.iter().enumerate() {
            if at > 0 {
                segments.push((around, ", ".to_owned()));
            }
            segments.push((around, name.clone()));
        }
        segments.push((around, ">".to_owned()));
        Some(segments)
    }

    /// What ties the status to the word it is drawn under.
    fn tied(&self) -> Vec<(Style, String)> {
        vec![(self.styles.text.dimmed(), TIE.to_owned())]
    }

    /// `segments` with what they are for after them, as much of its first line as fits.
    fn described(
        &self,
        mut segments: Vec<(Style, String)>,
        description: String,
    ) -> Vec<(Style, String)> {
        if let Some(line) = description.lines().next().filter(|line| !line.is_empty())
            && !self.noted
        {
            push_part(&mut segments, self.styles.text.italic(), line.to_owned());
        }
        segments
    }

    /// How many rows the condition of the pipeline element at `cursor` lets through, or how many
    /// it last did while it does not run, as it does not while it is being written.
    fn rows_that_pass(
        &self,
        block: &Block,
        working_set: &StateWorkingSet,
        cursor: usize,
    ) -> Vec<(Style, String)> {
        let inner = innermost_block(block, working_set, cursor.saturating_sub(1));
        let Some(Piped {
            through,
            before: Some((_, input)),
            ..
        }) = element_at(inner, working_set, cursor)
        else {
            return vec![];
        };
        let Ok(mut passed) = self.passed.lock() else {
            return vec![];
        };
        if passed.as_ref().is_some_and(|(of, ..)| *of != input) {
            *passed = None;
        }
        let of = self.counts.get(&self.engine_state, &input, count_of);
        let through = self.counts.get(&self.engine_state, &through, count_of);
        let fresh = through.zip(of);
        if let Some((through, of)) = fresh {
            *passed = Some((input, through, of));
        }
        let Some((_, count, of)) = *passed else {
            return vec![];
        };
        let mut style = if count == 0 {
            self.styles.active
        } else {
            self.styles.text
        };
        if fresh.is_none() {
            style = self.styles.text.dimmed();
        }
        let mut segments = self.tied();
        segments.push((style, format!("{count} of {of} rows")));
        segments
    }

    /// What the pipeline element that ends at `end`, before the `|` at `pipe`, gives: its
    /// columns, or its type, drawn under the pipe so that it is clear what is handed on from where.
    fn handed_on(
        &self,
        block: &Block,
        working_set: &StateWorkingSet,
        end: usize,
        pipe: usize,
    ) -> Vec<(Style, String)> {
        let inner = innermost_block(block, working_set, pipe);
        let Some(piped) = element_at(inner, working_set, end) else {
            return vec![];
        };
        let declared = Some(&piped.element.ty)
            .filter(|ty| !matches!(ty, Type::Any | Type::Nothing))
            .filter(|ty| match ty {
                Type::Table(columns) | Type::Record(columns) => !columns.is_empty(),
                _ => true,
            })
            .cloned();
        let Some(ty) =
            declared.or_else(|| self.rows.get(&self.engine_state, &piped.through, rows_of))
        else {
            return vec![];
        };
        self.anchored.set(Some(pipe));
        let Some(shape) = self.shape(&ty, false) else {
            return vec![];
        };
        let mut segments = self.tied();
        segments.extend(shape);
        segments
    }

    /// The words at fault in the line: those the parser has a fix for (`=` in a condition is
    /// written `==`), and in its row conditions the columns the rows do not have and the operands
    /// an operator cannot work on. With each, the words for what is wrong. What is being typed is
    /// not finished, nor is it wrong yet.
    fn faults(
        &self,
        parsed: &ParsedHighlight,
        working_set: &mut StateWorkingSet,
        cursor: usize,
    ) -> Vec<(Span, String)> {
        let mut calls: Vec<(Call, Span, BlockId)> = Vec::new();
        parsed.block.flat_map(
            working_set,
            &|expr: &Expression| match &expr.expr {
                Expr::Call(call) => (call.arguments.iter())
                    .filter_map(Argument::expr)
                    .find_map(|arg| match arg.expr {
                        Expr::RowCondition(block_id) => {
                            Some(((**call).clone(), arg.span, block_id))
                        }
                        _ => None,
                    })
                    .into_iter()
                    .collect(),
                _ => vec![],
            },
            &mut calls,
        );
        // `&&` is `and` in a condition, and `;` between commands.
        let in_condition = |span: Span| {
            calls.iter().any(|(_, condition, _)| {
                condition.start <= span.start
                    && !(working_set.get_span_contents(Span::new(condition.start, span.start)))
                        .iter()
                        .any(|byte| matches!(byte, b'|' | b';'))
            })
        };
        let mut faults: Vec<(Span, String)> = Vec::new();
        for fix in parsed.errors.iter().filter_map(ParseError::fix) {
            let meant = match in_condition(fix.span) {
                true => fix.replacements.first(),
                false => fix.replacements.last(),
            };
            if let Some(meant) = meant
                && !touches(fix.span, cursor)
                && !faults.iter().any(|(span, _)| *span == fix.span)
            {
                let written = String::from_utf8_lossy(working_set.get_span_contents(fix.span));
                faults.push((fix.span, format!("`{written}` is written `{meant}`")));
            }
        }
        for (call, _, block_id) in &calls {
            // A comparison is only judged by the rows it will see, where those are known.
            let Some((rows, comparisons)) =
                self.comparisons_of(*block_id, call, parsed, working_set)
            else {
                continue;
            };
            for (mut lhs, mut operator, mut rhs) in comparisons {
                let typing = |span: Span| span.start <= cursor && cursor <= span.end;
                let Some(name) = column_name(&lhs) else {
                    continue;
                };
                if typing(lhs.span) {
                    continue;
                }
                let Some(ty) = column_type(&rows, &name) else {
                    faults.push((lhs.span, format!("the rows have no column `{name}`")));
                    continue;
                };
                if typing(rhs.span) || matches!(rhs.expr, Expr::Garbage) {
                    continue;
                }
                lhs.ty = ty;
                let (_, error) = math_result_type(working_set, &mut lhs, &mut operator, &mut rhs);
                if let Some(error) = error {
                    faults.push((rhs.span, error_message(&error)));
                }
            }
        }
        faults
    }

    /// The type of the rows the condition `block_id` of `call` will see, when known, and the
    /// comparisons it makes, looking through `and`, `or` and `not`.
    fn comparisons_of(
        &self,
        block_id: nu_protocol::BlockId,
        call: &Call,
        parsed: &ParsedHighlight,
        working_set: &StateWorkingSet,
    ) -> Option<(Type, Vec<Comparison>)> {
        let inner = innermost_block(&parsed.block, working_set, call.head.start);
        let (piped, input) = element_at(inner, working_set, call.head.start)?.before?;
        let rows =
            known_rows(&piped.ty).or_else(|| self.rows.get(&self.engine_state, &input, rows_of))?;
        let first = |id| {
            let block = working_set.get_block(id);
            Some(&block.pipelines.first()?.elements.first()?.expr)
        };
        let mut expr = first(block_id)?;
        // `$in` in a condition has it collected: the condition is a subexpression of that.
        while let Expr::Collect(_, inner) = &expr.expr
            && let Expr::Subexpression(inner_id) = inner.expr
        {
            expr = first(inner_id)?;
        }
        let mut comparisons = Vec::new();
        gather_comparisons(expr, &mut comparisons);
        Some((rows, comparisons))
    }
}

/// `ty`, when it says what columns the rows have.
fn known_rows(ty: &Type) -> Option<Type> {
    let known = match ty {
        Type::Table(columns) | Type::Record(columns) => !columns.is_empty(),
        Type::List(inner) => matches!(&**inner, Type::Table(c) | Type::Record(c) if !c.is_empty()),
        _ => false,
    };
    known.then(|| ty.clone())
}

/// A comparison of a row condition: what is on its left, its operator, what is on its right.
type Comparison = (Expression, Expression, Expression);

/// The comparisons in `expr`, looking through `and`, `or` and `not`.
fn gather_comparisons(expr: &Expression, out: &mut Vec<Comparison>) {
    match &expr.expr {
        Expr::BinaryOp(lhs, operator, rhs) => match operator.expr {
            Expr::Operator(Operator::Boolean(_)) => {
                gather_comparisons(lhs, out);
                gather_comparisons(rhs, out);
            }
            Expr::Operator(_) => out.push(((**lhs).clone(), (**operator).clone(), (**rhs).clone())),
            _ => {}
        },
        Expr::UnaryNot(inner) => gather_comparisons(inner, out),
        _ => {}
    }
}

/// The name of the column `expr` reads, when it is one.
fn column_name(expr: &Expression) -> Option<String> {
    match &expr.expr {
        Expr::FullCellPath(path) if !path.tail.is_empty() => Some(
            (path.tail.iter())
                .map(|member| member.to_string())
                .collect::<Vec<_>>()
                .join("."),
        ),
        _ => None,
    }
}

/// A pipeline element, with the text of its pipeline through it, and the element before it, which
/// is piped into it, with the text through that.
struct Piped<'a> {
    element: &'a Expression,
    through: String,
    before: Option<(&'a Expression, String)>,
}

/// The pipeline element of `block` at `at`.
fn element_at<'a>(block: &'a Block, working_set: &StateWorkingSet, at: usize) -> Option<Piped<'a>> {
    let text = |from: usize, to: usize| {
        String::from_utf8_lossy(working_set.get_span_contents(Span::new(from, to)))
            .trim()
            .to_owned()
    };
    block.pipelines.iter().find_map(|pipeline| {
        let elements = &pipeline.elements;
        let index = (elements.iter()).position(|element| touches(element.expr.span, at))?;
        let first = elements.first()?.expr.span.start;
        let element = &elements[index].expr;
        let before = index.checked_sub(1).map(|before| &elements[before].expr);
        Some(Piped {
            element,
            through: text(first, element.span.end),
            before: before.map(|before| (before, text(first, before.span.end))),
        })
    })
}

/// The type of the column `name` of rows of type `rows`.
fn column_type(rows: &Type, name: &str) -> Option<Type> {
    let columns = match rows {
        Type::Table(columns) | Type::Record(columns) => columns,
        Type::List(inner) => return column_type(inner, name),
        _ => return None,
    };
    columns
        .iter()
        .find(|(column, _)| column == name)
        .map(|(_, ty)| ty.clone())
}

/// How a parameter of shape `shape` reads in a usage: the shape's name, the choices of a `oneof`
/// as `glob | string`, a closure as `closure`, and nothing for one that takes anything, as an
/// expression does, though the shape is called `variable`.
pub(crate) fn shape_label(shape: &SyntaxShape) -> Option<String> {
    Some(match shape {
        SyntaxShape::Any | SyntaxShape::Nothing | SyntaxShape::MathExpression => return None,
        SyntaxShape::OneOf(shapes) => {
            let mut choices: Vec<String> = Vec::new();
            for choice in shapes.iter().filter_map(shape_label) {
                if !choices.contains(&choice) {
                    choices.push(choice);
                }
            }
            return (!choices.is_empty()).then(|| choices.join(" | "));
        }
        SyntaxShape::Keyword(_, inner) => return shape_label(inner),
        SyntaxShape::Closure(_) => "closure".to_owned(),
        SyntaxShape::RowCondition => "condition".to_owned(),
        SyntaxShape::Record(_) => "record".to_owned(),
        SyntaxShape::Table(_) => "table".to_owned(),
        SyntaxShape::List(inner) => match shape_label(inner) {
            Some(inner) => format!("list<{inner}>"),
            None => "list".to_owned(),
        },
        shape => shape.to_string(),
    })
}

impl reedline::StatusLine for NuStatusLine {
    fn status(&mut self, line: &str, pos: usize, columns: u16, use_ansi_coloring: bool) -> String {
        let sign = reedline::StatusLine::sign(self, line, pos);
        self.noted = sign.is_some_and(|sign| {
            sign.note.is_some()
                && (sign.span).is_none_or(|span| (span.start..=span.end).contains(&pos))
        });
        self.columns = columns;
        self.anchored.set(None);
        let segments = self.segments(line, pos);
        render(&segments, columns, use_ansi_coloring)
    }

    /// What is wrong with the line, as a sign at its end like a sign column's, its note saying
    /// what and what was meant while the cursor is on it: an error behind the word being typed
    /// (`ls --hepl `), or a program that is no command (`crgo build`). The word being typed is
    /// the completions' to answer.
    fn anchor(&mut self, _line: &str, _pos: usize) -> Option<usize> {
        self.anchored.get()
    }

    fn sign(&mut self, line: &str, pos: usize) -> Option<Sign> {
        if !self.config.errors || line.trim().is_empty() {
            return None;
        }
        let before_cursor = line.get(..pos)?;
        let parsed = self.parse_cache.parsed(&self.engine_state, line);
        if parsed.skipped_module_load {
            return None;
        }
        let offset = parsed.global_span_offset;
        let typed = before_cursor.trim_end().len();
        let mut working_set = parsed.working_set(&self.engine_state);
        let within_line =
            |span: Span| span.start.saturating_sub(offset)..span.end.saturating_sub(offset);
        // The note goes with the first fault the cursor is on, and all of them are drawn.
        let mark = |faults: Vec<(Span, String)>| -> Option<Sign> {
            let on_it = |span: &Span| span.start <= offset + pos && offset + pos <= span.end;
            let spoken = faults
                .iter()
                .find(|(span, _)| on_it(span))
                .or_else(|| faults.first())?;
            Some(Sign {
                text: String::new(),
                style: self.styles.error,
                note: Some(spoken.1.clone()),
                span: Some(within_line(spoken.0)),
                marks: faults.iter().map(|(span, _)| within_line(*span)).collect(),
            })
        };
        let faults = self.faults(&parsed, &mut working_set, offset + pos);
        if !faults.is_empty() {
            return mark(faults);
        }
        if let Some(error) = first_error(&parsed, offset + typed, offset + pos) {
            let note = remedy(error, &working_set).unwrap_or_else(|| error_message(error));
            return mark(vec![(error.span(), note)]);
        }
        // The pipeline element at the cursor failing where what is piped into it does not, as
        // `first 4kb` does on rows: what running the line will say, said before it is run.
        if let Some(Piped {
            element,
            through,
            before: Some((_, input)),
        }) = element_at(&parsed.block, &working_set, offset + pos)
            && let Some(failed) = self.trials.get(&self.engine_state, &through, failure)
            && self
                .trials
                .get(&self.engine_state, &input, failure)
                .is_none()
        {
            return mark(vec![(arguments_of(element.span, &working_set), failed)]);
        }
        // A program that names nothing that runs, unless it is the word being typed: the cursor
        // at its end with nothing after it.
        let shapes = nu_parser::flatten_block(&working_set, &parsed.block);
        let (head, word) = shapes.into_iter().find_map(|(_, shape)| {
            let FlatShape::External(head) = shape else {
                return None;
            };
            let word = line.get(within_line(*head))?;
            let typing = head.end == offset + pos && line[pos..].trim().is_empty();
            (!typing && !word.is_empty()).then(|| (*head, word.to_owned()))
        })?;
        let resolves = |word: &str| {
            let span = Span::new(head.start, head.start + word.len());
            external_is_resolved(&self.engine_state, &Stack::new(), line, offset, span)
        };
        let slip = !resolves(&word)
            || (word != word.to_lowercase()
                && self.programs.meant(&self.engine_state, &word).as_deref()
                    == Some(word.to_lowercase().as_str()));
        if !slip || word.contains(std::path::is_separator) {
            return None;
        }
        let note = match self.programs.meant(&self.engine_state, &word) {
            Some(meant) => format!("`{word}` is no command: `{meant}`?"),
            None => format!("`{word}` is no command"),
        };
        mark(vec![(head, note)])
    }
}

/// The command each word that names none was most likely meant to be, by the word: worked out
/// once, from the commands of the engine and the programs on `PATH`.
#[derive(Default)]
struct Programs {
    meant: std::collections::HashMap<String, Option<String>>,
}

impl Programs {
    /// The command two edits at most from `word`, a swap counting as one, case aside.
    fn meant(&mut self, engine_state: &EngineState, word: &str) -> Option<String> {
        if let Some(meant) = self.meant.get(word) {
            return meant.clone();
        }
        let paths = engine_state
            .get_env_var("PATH")
            .and_then(|path| path.as_list().ok())
            .map(|dirs| {
                dirs.iter()
                    .filter_map(|dir| dir.coerce_str().ok().map(|dir| dir.into_owned()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let on_path = paths
            .iter()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flat_map(|dir| {
                dir.filter_map(Result::ok)
                    .filter_map(|entry| entry.file_name().into_string().ok())
            });
        let internal = engine_state
            .get_decls_sorted(false)
            .into_iter()
            .map(|(name, _)| String::from_utf8_lossy(&name).into_owned())
            .filter(|name| !name.contains(' '));
        let meant = nearest(word, internal.chain(on_path));
        self.meant.insert(word.to_owned(), meant.clone());
        meant
    }
}

/// Of `names`, the one two edits at most from `word`, a swap of two letters counting as one, case
/// aside, as completion corrects typos: the fewest edits, then the shortest.
fn nearest(word: &str, names: impl Iterator<Item = String>) -> Option<String> {
    let letters = |text: &str| -> Vec<String> {
        (text.to_lowercase().graphemes(true))
            .map(str::to_owned)
            .collect()
    };
    let typed = letters(word);
    names
        .filter(|name| name != word)
        .map(|name| (damerau_levenshtein(&typed, &letters(&name)), name))
        .filter(|(slips, _)| *slips <= 2)
        .min_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.len().cmp(&y.len())))
        .map(|(_, name)| name)
}

/// The `parts` of a signature joined by spaces, then what the parameter being typed is for when
/// all of it fits in `budget` columns. The part being typed comes first, and what does not fit is
/// cut at the end, as any status is.
fn fit(
    parts: Vec<Vec<(Style, String)>>,
    description: Option<String>,
    budget: usize,
    text: Style,
) -> Vec<(Style, String)> {
    let mut segments = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            segments.push((text, " ".to_owned()));
        }
        segments.extend(part);
    }
    let width: usize = (segments.iter())
        .map(|(_, part)| shown_text(part).width())
        .sum();
    if let Some(description) =
        description.filter(|description| width + GAP.len() + description.width() <= budget)
    {
        push_part(&mut segments, text.italic(), description);
    }
    segments
}

/// A type as it reads, `int | filesize` or `list<string>`, in parts: the names of types in
/// `types`, and what joins them, the `|` between choices and the brackets and colons of a
/// collection, in `around`, so that each choice stands apart.
pub(crate) fn type_parts(ty: &str, types: Style, around: Style) -> Vec<(Style, String)> {
    let mut parts: Vec<(Style, String)> = Vec::new();
    for c in ty.chars() {
        let style = if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
            types
        } else {
            around
        };
        match parts.last_mut() {
            Some((last, text)) if *last == style => text.push(c),
            _ => parts.push((style, c.to_string())),
        }
    }
    parts
}

/// Append `part` after a gap, or first when nothing is there yet.
fn push_part(segments: &mut Vec<(Style, String)>, style: Style, part: String) {
    if !segments.is_empty() {
        segments.push((style, GAP.to_string()));
    }
    segments.push((style, part));
}

/// The part of the pipeline element `element` after its command: what running it said is wrong
/// is rarely the name. All of it when it is only a name.
fn arguments_of(element: Span, working_set: &StateWorkingSet) -> Span {
    let text = working_set.get_span_contents(element);
    match text.iter().position(u8::is_ascii_whitespace) {
        Some(at) => {
            let rest = &text[at..];
            let blank = rest.iter().take_while(|c| c.is_ascii_whitespace()).count();
            Span::new(element.start + at + blank, element.end)
        }
        None => element,
    }
}

/// The error to show: the first one not about the text still being typed.
/// None while the line is unfinished, as the validator judges it.
///
/// An error touching the cursor is in the token being typed. One saying
/// something is missing, found between the end of the typed text (`typed`)
/// and the cursor, is about the token that comes next: `str substring `
/// lacks its range only until it is typed.
fn first_error(parsed: &ParsedHighlight, typed: usize, cursor: usize) -> Option<&ParseError> {
    if is_incomplete(&parsed.errors) {
        return None;
    }
    parsed.errors.iter().find(|error| {
        let span = error.span();
        let at_cursor = span.start <= cursor && cursor <= span.end;
        let next_token = awaits_input(error) && span.start <= cursor && typed <= span.end;
        !(at_cursor || next_token)
    })
}

/// What to write instead of what `error` is about, where that is plain: the flag a misspelt one
/// was meant to be.
fn remedy(error: &ParseError, working_set: &StateWorkingSet) -> Option<String> {
    let ParseError::UnknownFlag(command, flag, ..) = error else {
        return None;
    };
    let decl_id = working_set.find_decl(command.as_bytes())?;
    let signature = working_set.get_decl(decl_id).signature();
    // The parser names the flag without its dashes, `hepl` for `--hepl`.
    let dashed = |name: &str| match name.chars().count() {
        1 => format!("-{name}"),
        _ => format!("--{name}"),
    };
    let typed = flag.trim_start_matches('-');
    let flags = (signature.named.iter()).filter_map(|named| named.long_name().map(str::to_owned));
    let meant = nearest(typed, flags)?;
    Some(format!(
        "`{command}` has no flag `{}`: `{}`?",
        dashed(typed),
        dashed(&meant)
    ))
}

/// Whether `error` says the line stops short of something it needs.
fn awaits_input(error: &ParseError) -> bool {
    matches!(
        error,
        ParseError::MissingPositional(..)
            | ParseError::KeywordMissingArgument(..)
            | ParseError::MissingFlagParam(..)
            | ParseError::MissingRequiredFlag(..)
            | ParseError::MissingType(..)
            | ParseError::IncompleteMathExpression(..)
            | ParseError::Expected(..)
            | ParseError::ExpectedWithStringMsg(..)
            | ParseError::ExpectedKeyword(..)
    )
}

/// The error's headline and its first label on one line, as `Type mismatch:
/// expected int, found string`. A label that only restates the headline, as
/// `command doesn't support string input` does, is left out.
fn error_message(error: &ParseError) -> String {
    let headline = error.to_string();
    let headline = headline.trim_end_matches('.');
    let label = error
        .labels()
        .and_then(|mut labels| labels.find_map(|label| label.label().map(str::to_owned)))
        .filter(|label| !restates(label, headline));
    match label {
        Some(label) => format!("{headline}: {label}"),
        None => headline.to_string(),
    }
}

/// Whether most of the words of `label` are already in `headline`.
fn restates(label: &str, headline: &str) -> bool {
    let words = |text: &str| -> Vec<String> {
        text.split_whitespace()
            .map(|word| {
                word.trim_matches(|c: char| c.is_ascii_punctuation())
                    .to_lowercase()
            })
            .filter(|word| !word.is_empty())
            .collect()
    };
    let headline = words(headline);
    let label = words(label);
    let shared = label.iter().filter(|word| headline.contains(word)).count();
    label.is_empty() || shared * 4 >= label.len() * 3
}

/// The block that `pos` is in at the pipeline element of `block` it is at, a closure, block or
/// subexpression of that element and so on inward, or `block` itself.
fn innermost_block<'a>(
    block: &'a Block,
    working_set: &'a StateWorkingSet,
    pos: usize,
) -> &'a Block {
    element_before(block, pos)
        .and_then(|element| {
            (element.expr).find_map(working_set, &|expr| block_around(expr, working_set, pos))
        })
        .map_or(block, |inner| innermost_block(inner, working_set, pos))
}

/// The pipeline element of `block` at `pos`: the last to start at or before it.
fn element_before(block: &Block, pos: usize) -> Option<&PipelineElement> {
    let starts_before = |span: Span| span.start <= pos;
    let pipeline = block.pipelines.iter().rev().find(|pipeline| {
        (pipeline.elements.first()).is_some_and(|element| starts_before(element.expr.span))
    })?;
    (pipeline.elements.iter())
        .rev()
        .find(|element| starts_before(element.expr.span))
}

/// The type of the pipeline element at `pos` (or the last one before it) in
/// the innermost closure, block or subexpression around `pos`, if the parser
/// knows more than `any`. None after a `;` that ends the pipeline: nothing
/// flows into the next one. A newline doesn't end it here, as a `|` on the
/// next line continues it.
fn output_type<'a>(
    block: &'a Block,
    working_set: &'a StateWorkingSet,
    pos: usize,
) -> Option<&'a Type> {
    let block = innermost_block(block, working_set, pos);
    let element = element_before(block, pos)?;
    let end = element.expr.span.end;
    if end <= pos {
        // Lexed, so a `;` in a comment or a redirection's file name is not one.
        let after = working_set.get_span_contents(Span::new(end, pos + 1));
        let (tokens, _) = lex(after, end, &[], &[], true);
        if tokens
            .iter()
            .any(|token| token.contents == TokenContents::Semicolon)
        {
            return None;
        }
    }
    Some(&element.expr.ty).filter(|ty| !matches!(ty, Type::Any | Type::Nothing))
}

/// For [`Traverse::find_map`]: the block `expr` is, a closure, block or
/// subexpression, when `pos` is inside it. On its brackets `pos` is outside,
/// as it is for the call the status is about, so a cursor after the `}` of
/// `update name {|r| $r.name | str upcase }` shows the type of `update`.
/// A row condition is not a block here: `where size > 1kb` shows the type
/// of `where`, not `bool`.
fn block_around<'a>(
    expr: &'a Expression,
    working_set: &'a StateWorkingSet,
    pos: usize,
) -> ControlFlow<Option<&'a Block>> {
    if !expr.span.contains(pos) {
        return ControlFlow::Break(None);
    }
    let (Expr::Block(block_id) | Expr::Closure(block_id) | Expr::Subexpression(block_id)) =
        &expr.expr
    else {
        return ControlFlow::Continue(());
    };
    // A block can have no brackets, as the value of `let x = [1 2] | math
    // sum` does, and a block still being typed has no closing one yet.
    let text = working_set.get_span_contents(expr.span);
    let closing = match text.first() {
        Some(b'{') => Some(b'}'),
        Some(b'(') => Some(b')'),
        _ => None,
    };
    let on_bracket = closing.is_some_and(|closing| {
        pos == expr.span.start
            || (text.len() > 1 && text.last() == Some(&closing) && pos + 1 == expr.span.end)
    });
    ControlFlow::Break((!on_bracket).then(|| working_set.get_block(*block_id).as_ref()))
}

/// Draws `segments`, cut to fit in `columns` with an ellipsis, so the status
/// line stays on one row. Styles apply only with `use_ansi_coloring`, and
/// escapes the text carries itself, as a preview of `ansi red` does, are
/// dropped: the row draws its own styles only. Control characters show as
/// [`shown_text`] writes them, so the width is the width of what shows.
fn render(segments: &[(Style, String)], columns: u16, use_ansi_coloring: bool) -> String {
    if segments.is_empty() {
        return String::new();
    }
    let segments: Vec<_> = segments
        .iter()
        .map(|(style, text)| (style, shown_text(text)))
        .collect();
    // The full width: reedline moves the cursor after drawing the row, so text
    // filling it exactly wraps onto nothing.
    let limit = usize::from(columns).saturating_sub(INDENT.len());
    // Measured grapheme by grapheme, as the loop below cuts and reedline
    // draws: the width of a whole string can be less, as for lam-alef.
    let total: usize = segments
        .iter()
        .flat_map(|(_, text)| text.graphemes(true))
        .map(UnicodeWidthStr::width)
        .sum();
    let budget = if total > limit {
        limit.saturating_sub(1)
    } else {
        limit
    };

    let mut out = String::from(INDENT);
    let mut used = 0;
    for (style, text) in segments {
        let mut piece = String::new();
        let mut cut = false;
        for grapheme in text.graphemes(true) {
            let width = grapheme.width();
            if used + width > budget {
                cut = true;
                break;
            }
            used += width;
            piece.push_str(grapheme);
        }
        if cut {
            piece.push('…');
        }
        if use_ansi_coloring {
            out.push_str(&style.paint(piece).to_string());
        } else {
            out.push_str(&piece);
        }
        if cut {
            break;
        }
    }
    out
}

/// `text` as the status line shows it: ANSI escapes dropped, and the other
/// control characters written the way a nushell string escapes them.
fn shown_text(text: &str) -> String {
    strip_ansi_unlikely(&one_line(text)).into_owned()
}

/// `text` on one row: line breaks and tabs shown as `\n`, `\r` and `\t`, and
/// every other control character but `ESC` and `BEL` as its `\u{..}` escape,
/// as `\u{7f}` for DEL. reedline would draw these as `^?` or `M-^E`, wider
/// than `render` measures them. `ESC` and `BEL` are left for
/// [`strip_ansi_unlikely`], which drops the sequences they start and end, as
/// an OSC 8 link ending in `BEL`, and drops a `BEL` of its own.
fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\x1b' | '\x07' => out.push(c),
            c if c.is_control() => out.extend(c.escape_unicode()),
            c => out.push(c),
        }
    }
    out
}

/// Evaluates lines built only from const commands, off the REPL thread, so a
/// slow one doesn't stall the keystroke that asked for it. A paint waits
/// `budget` for the value, then interrupts the evaluation: iterating a range
/// stops there, as in `1..10000000000 | math sum`. A command that doesn't
/// check for interrupts keeps running, and no other line is evaluated until
/// it ends.
struct ConstPreview {
    /// How long a paint waits for the value: [`PREVIEW_BUDGET`].
    budget: Duration,
    /// A copy of the REPL's engine state with an interrupt flag of its own,
    /// made on the first preview of the prompt.
    engine_state: Option<Arc<EngineState>>,
    /// The interrupt flag of `engine_state`, set when a paint gives up.
    interrupt: Arc<AtomicBool>,
    /// The thread of the last evaluation, which may still run after the
    /// paint stopped waiting for it.
    worker: Option<JoinHandle<()>>,
    /// The last line asked about and its preview, so moving the cursor
    /// doesn't evaluate the line again.
    last: Option<(String, Option<String>)>,
}

impl Default for ConstPreview {
    fn default() -> Self {
        Self {
            budget: PREVIEW_BUDGET,
            engine_state: None,
            interrupt: Arc::default(),
            worker: None,
            last: None,
        }
    }
}

impl ConstPreview {
    /// The value of `line`, which parsed as `parsed`, when it is built from
    /// const commands and evaluates within the budget.
    fn preview(
        &mut self,
        line: &str,
        parsed: &ParsedHighlight,
        working_set: &StateWorkingSet,
        repl_engine_state: &EngineState,
    ) -> Option<String> {
        if let Some((last_line, preview)) = &self.last
            && last_line == line
        {
            return preview.clone();
        }
        let preview = if parsed.errors.is_empty() && is_previewable(&parsed.block, working_set) {
            if self.is_busy() {
                // An interrupted evaluation still runs: show nothing for now
                // and evaluate this line on a later paint.
                return None;
            }
            self.evaluate(parsed, repl_engine_state)
        } else {
            None
        };
        self.last = Some((line.to_string(), preview.clone()));
        preview
    }

    /// Whether the last evaluation still runs. One that panicked has ended.
    fn is_busy(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }

    /// Evaluates `parsed` on a thread of its own and waits `budget` for the
    /// value, then interrupts the evaluation and gives up on it.
    fn evaluate(
        &mut self,
        parsed: &ParsedHighlight,
        repl_engine_state: &EngineState,
    ) -> Option<String> {
        self.interrupt.store(false, Ordering::Relaxed);
        let engine_state = self
            .engine_state
            .get_or_insert_with(|| {
                let mut engine_state = repl_engine_state.clone();
                engine_state.set_signals(Signals::new(self.interrupt.clone()));
                Arc::new(engine_state)
            })
            .clone();

        let (sender, receiver) = mpsc::channel();
        let block = parsed.block.clone();
        let delta = parsed.delta.clone();
        let worker = thread::Builder::new()
            .name("status line preview".into())
            .stack_size(PREVIEW_STACK_SIZE)
            .spawn(move || {
                // A const command that panics must not reach the REPL's panic
                // hook, which takes the terminal out of raw mode mid-edit. The
                // sender is dropped instead, and the paint gets no preview.
                let Ok(preview) =
                    catch_completion_panic(|| evaluate_const_line(&engine_state, &block, &delta))
                else {
                    return;
                };
                // The paint that asked may have stopped waiting.
                let _ = sender.send(preview);
            })
            .ok()?;
        self.worker = Some(worker);

        match receiver.recv_timeout(self.budget) {
            Ok(preview) => preview,
            Err(_) => {
                self.interrupt.store(true, Ordering::Relaxed);
                None
            }
        }
    }
}

/// The value of `block` as the status line shows it, or `None` when it
/// isn't a constant, has nothing to show or was interrupted. The text is
/// [`shown_text`], cut
/// to [`PREVIEW_GRAPHEMES`], so a huge value costs a paint no more than one
/// that fills the row.
fn evaluate_const_line(
    engine_state: &EngineState,
    block: &Block,
    delta: &StateDelta,
) -> Option<String> {
    let mut working_set = StateWorkingSet::new(engine_state);
    working_set.delta = delta.clone();
    let span = block.span.unwrap_or_else(Span::unknown);
    let value = eval_const_subexpression(&working_set, block, PipelineData::empty(), span)
        .and_then(|data| data.into_value(span))
        .ok()?;
    // Interrupted while collected, a stream ends early: what it holds is
    // only part of the value, and nobody waits for it.
    if engine_state.signals().interrupted()
        || matches!(value, Value::Nothing { .. } | Value::Error { .. })
    {
        return None;
    }
    let mut text = shown_text(&value.to_expanded_string(", ", engine_state.get_config()));
    if let Some((end, _)) = text.grapheme_indices(true).nth(PREVIEW_GRAPHEMES) {
        text.truncate(end);
    }
    Some(text)
}

/// Whether every command in `block` is const, so it can be evaluated while
/// it is typed, and it computes something: a literal would preview as itself.
fn is_previewable(block: &Block, working_set: &StateWorkingSet) -> bool {
    #[derive(PartialEq)]
    enum Part {
        Computed,
        NotConst,
    }
    let mut parts = vec![];
    block.flat_map(
        working_set,
        &|expr| match &expr.expr {
            Expr::Call(call) if working_set.get_decl(call.decl_id).is_const() => {
                vec![Part::Computed]
            }
            Expr::Call(_)
            | Expr::ExternalCall(..)
            | Expr::Closure(_)
            | Expr::RowCondition(_)
            | Expr::Collect(..)
            | Expr::Overlay(_) => vec![Part::NotConst],
            Expr::BinaryOp(..) | Expr::UnaryNot(_) | Expr::Var(_) => vec![Part::Computed],
            Expr::FullCellPath(path) if !path.tail.is_empty() => vec![Part::Computed],
            _ => vec![],
        },
        &mut parts,
    );
    let redirected = block
        .pipelines
        .iter()
        .flat_map(|pipeline| &pipeline.elements)
        .any(|element| element.redirection.is_some());
    !redirected && parts.contains(&Part::Computed) && !parts.contains(&Part::NotConst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nu_protocol::{
        ShellError, Signature,
        engine::{Call, Command, Stack},
        record,
    };
    use rstest::rstest;

    /// A status line over the default commands, with every part on, colored
    /// hints and a preview that waits as long as the evaluation takes.
    fn status_line(config: StatusLineConfig) -> NuStatusLine {
        let mut engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        // The programs the status line finds are the ones on the engine's `PATH`.
        let path = std::env::var("PATH").unwrap_or_default();
        engine_state.add_env_var("PATH".into(), Value::test_string(path));
        let mut status_line = NuStatusLine::new(
            Arc::new(engine_state),
            LineParseCache::default(),
            config,
            StatusStyles::new(
                Style::new().fg(Color::DarkGray),
                Style::new().fg(Color::Red),
            ),
        );
        status_line.preview.budget = Duration::from_secs(60);
        status_line
    }

    fn only(part: &str) -> StatusLineConfig {
        StatusLineConfig {
            errors: part == "errors",
            signature: part == "signature",
            output_type: part == "type",
            preview: part == "preview",
            ..StatusLineConfig::default()
        }
    }

    /// The status for `line` with the cursor at its end, as text.
    fn status(config: StatusLineConfig, line: &str) -> String {
        shown(&mut status_line(config), line)
    }

    /// What the sign for `line`, with the cursor at its end, says it means; empty with no sign.
    fn note(config: StatusLineConfig, line: &str) -> String {
        reedline::StatusLine::sign(&mut status_line(config), line, line.len())
            .and_then(|sign| sign.note)
            .unwrap_or_default()
    }

    /// What `status_line` shows for `line` with the cursor at its end.
    fn shown(status_line: &mut NuStatusLine, line: &str) -> String {
        shown_at(status_line, line, line.len())
    }

    /// What `status_line` shows for `line` with the cursor at byte `pos`.
    fn shown_at(status_line: &mut NuStatusLine, line: &str, pos: usize) -> String {
        let segments = status_line.segments(line, pos);
        let said: String = segments.into_iter().map(|(_, text)| text).collect();
        said.strip_prefix(TIE).map_or(said.clone(), str::to_owned)
    }

    /// What the status says for `line`: it starts with `start`, and is empty when that is.
    #[rstest]
    #[case::the_first_parameter("str substring ", "<range>")]
    #[case::what_is_ahead_of_it("str replace ", "<find: string> <replace>")]
    #[case::a_written_one_is_gone("str replace a ", "<replace: string | closure>")]
    #[case::an_optional_one("ls | first ", "(rows: int | filesize)")]
    #[case::the_first_of_a_list("ls | select ", "...(rest: cell-path)")]
    #[case::in_the_name_what_it_will_take("sort-by", "...(comparator)")]
    #[case::a_flag_says_what_its_value_takes("into datetime --format ", "string   Specify")]
    #[case::a_table_handed_on("[[a b]; [1 2]] | ", "table<a, b>")]
    #[case::a_record_handed_on("{a: 1, b: 2} | ", "record<a, b>")]
    #[case::a_list_handed_on("[1 2] | ", "list<int>")]
    #[case::a_closure_is_told_what_it_is_given("[[a b]; [1 2]] | each { ", "$in: record<a, b>")]
    #[case::the_rest_of_a_list("ls | select name type ", "")]
    #[case::a_program("cargo build", "")]
    #[case::a_program_in_a_closure("ls | each {|f| $f.name | cargo", "")]
    #[case::a_keyword_that_is_written("let x = ", "")]
    #[case::a_flag_being_typed("ls --al", "")]
    #[case::a_switch("ls --all", "")]
    #[case::past_the_last_parameter("ls | length ", "")]
    fn the_status_starts_at_the_parameter_being_typed(#[case] line: &str, #[case] start: &str) {
        let said = status(only("signature"), line);
        assert!(
            said.starts_with(start) && (said.is_empty() == start.is_empty()),
            "{said}"
        );
        assert!(!said.contains("{flags}"), "{said}");
    }

    /// What `status_line` comes to show for `line`, once what it runs off the paint is done.
    fn settled(status_line: &mut NuStatusLine, line: &str) -> String {
        (0..400)
            .find_map(|_| {
                let said = shown(status_line, line);
                if said.is_empty() {
                    std::thread::sleep(Duration::from_millis(5));
                    return None;
                }
                Some(said)
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_condition_says_how_many_rows_pass() {
        let mut status_line = status_line(only("signature"));
        let rows = "[[size name]; [1kb a] [2kb b] [3kb c]] | where";
        let line = format!("{rows} size > 1kb");
        let mut said = settled(&mut status_line, &line);
        // The count is of the rows handed in, known once both have run.
        for _ in 0..400 {
            if said.starts_with("2 of 3") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            said = shown(&mut status_line, &line);
        }
        assert_eq!(said, "2 of 3 rows");
        // Chained, it is still the whole condition.
        let line = format!("{rows} size > 1kb and name == b");
        for _ in 0..400 {
            said = shown(&mut status_line, &line);
            if said.starts_with("1 of 3") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(said, "1 of 3 rows");
        // While it does not run, what it last let through stays.
        let said = shown(&mut status_line, &format!("{rows} size > 1kb and name =="));
        assert_eq!(said, "1 of 3 rows");
    }

    /// The parts of `line` a sign marks and what its note says, once it has settled.
    fn marks_of(line: &str) -> (Vec<String>, String) {
        let mut status_line = status_line(only("errors"));
        let sign = (0..400).find_map(|_| {
            let sign = reedline::StatusLine::sign(&mut status_line, line, line.len());
            if sign.is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
            sign
        });
        let sign = sign.unwrap_or_default();
        assert_eq!(sign.text, "", "a mark draws nothing at the end of the row");
        let marks = (sign.marks.iter())
            .map(|mark| line[mark.clone()].to_owned())
            .collect();
        (marks, sign.note.unwrap_or_default())
    }

    #[test]
    fn what_is_wrong_in_a_condition_is_marked_where_it_is() {
        let rows = "[[size name]; [1kb a]] | where";
        let (marks, note) = marks_of(&format!("{rows} name = a "));
        assert_eq!(marks, ["="], "{note}");
        assert!(note.contains("`==`"), "{note}");

        let (marks, _) = marks_of(&format!("{rows} name == a && size > 1kb "));
        assert_eq!(marks, ["&&"]);

        let (marks, _) = marks_of(&format!("{rows} size > 'big' and name == a "));
        assert_eq!(marks, ["'big'"]);

        // Each of a chain is marked, not only the first.
        let (marks, _) = marks_of(&format!("{rows} size > 'big' and siz == 1 "));
        assert_eq!(marks, ["'big'", "siz"]);

        // What is being typed is not wrong yet.
        let mut status_line = status_line(only("errors"));
        let line = format!("{rows} siz");
        for _ in 0..20 {
            assert_eq!(
                reedline::StatusLine::sign(&mut status_line, &line, line.len()),
                None
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The status is drawn under what it is about: the word the cursor is on, the space after it
    /// where the next one goes, the pipe that hands on what it says, a closure's opening brace.
    #[rstest]
    #[case::the_word("str replace abc ", 13, "", Some(12))]
    #[case::the_space_after_it("str replace abc ", 16, "<replace", Some(16))]
    #[case::the_pipe("[[a b]; [1 2]] | ", 17, "table<a, b>", Some(15))]
    #[case::the_brace("[[a b]; [1 2]] | each { ", 24, "$in", Some(22))]
    fn the_status_is_drawn_under_what_it_is_about(
        #[case] line: &str,
        #[case] pos: usize,
        #[case] drawn: &str,
        #[case] anchor: Option<usize>,
    ) {
        let mut status_line = status_line(only("signature"));
        let shown = reedline::StatusLine::status(&mut status_line, line, pos, 100, false);
        assert!(
            shown.trim().trim_start_matches(TIE).starts_with(drawn),
            "{shown}"
        );
        assert_eq!(
            reedline::StatusLine::anchor(&mut status_line, line, pos),
            anchor
        );
    }

    #[test]
    fn a_narrow_row_keeps_the_parameter_being_typed() {
        let mut status_line = status_line(only("signature"));
        let line = "str replace ";
        let drawn = reedline::StatusLine::status(&mut status_line, line, line.len(), 30, false);
        assert!(drawn.contains("<find"), "{drawn}");
        assert!(drawn.width() <= 30, "{drawn}");
    }

    #[test]
    fn what_a_parameter_is_for_is_said_only_when_all_of_it_fits() {
        let text = Style::new();
        let parts = || {
            vec![
                vec![(text, "<cond>".to_owned())],
                vec![(text, "<then>".to_owned())],
            ]
        };
        let shown = |budget| -> String {
            let fitted = fit(parts(), Some("Condition to check.".into()), budget, text);
            fitted.into_iter().map(|(_, text)| text).collect()
        };
        assert_eq!(shown(200), "<cond> <then>   Condition to check.");
        assert_eq!(shown(30), "<cond> <then>");
    }

    #[test]
    fn an_element_that_fails_on_what_is_piped_in_is_signed_before_it_runs() {
        let mut status_line = status_line(only("errors"));
        // The first paint starts the run and shows nothing of it; one after it is done shows it.
        let mut sign_of = |line: &str| {
            (0..400).find_map(|_| {
                let sign = reedline::StatusLine::sign(&mut status_line, line, line.len());
                if sign.is_none() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                sign
            })
        };
        let sign = sign_of("[[size]; [1kb]] | first 4kb").expect("a sign");
        let note = sign.note.unwrap_or_default();
        assert!(note.contains("binary"), "{note}");
        assert_eq!(sign.span, Some(24..27));
        assert_eq!(
            sign.marks.first(),
            Some(&(24..27)),
            "the argument, not the command"
        );
        assert!(sign_of("[[size]; [1kb]] | first 1").is_none());
    }

    #[test]
    fn deeply_nested_closures_and_subexpressions_are_read_quickly() {
        let depth = 40;
        let line = format!(
            "{}ls | length{}",
            "ls | each {|x| (".repeat(depth),
            ") }".repeat(depth)
        );
        let before = line.len() - ") }".len() * depth;
        let started = nu_utils::time::Instant::now();
        let shown = thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let mut status_line = status_line(StatusLineConfig::default());
                (0..=line.len())
                    .filter(|&pos| line.is_char_boundary(pos))
                    .for_each(|pos| {
                        shown_at(&mut status_line, &line, pos);
                    });
                shown_at(&mut status_line, &line, before)
            })
            .expect("the test thread starts")
            .join()
            .expect("the test thread doesn't panic");
        drop(shown);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
    }

    #[rstest]
    #[case::still_typing_the_token(r#"let x: int = "a""#, "")]
    #[case::token_finished(r#"let x: int = "a" "#, "Type mismatch: expected int, found string")]
    #[case::argument_not_typed_yet("str substring ", "")]
    #[case::argument_skipped(
        "str substring | str length",
        "Missing required positional argument: missing range"
    )]
    #[case::label_restates_the_headline(
        "[1 2] | str downcase | ls",
        "Command does not support list<int> input"
    )]
    #[case::unfinished_block("if true {", "")]
    #[case::a_misspelt_flag_with_the_one_meant(
        "ls --hepl ",
        "`ls` has no flag `--hepl`: `--help`?"
    )]
    #[case::a_use_not_loaded_hides_errors("use ./consts.nu *; $X + 1", "")]
    #[case::a_variable_not_found("$X + 1", "Variable not found")]
    fn errors_wait_for_the_text_being_typed(#[case] line: &str, #[case] expected: &str) {
        // An error is a sign at the end of the line, which says it, not the status text.
        assert_eq!(status(only("errors"), line), "");
        assert_eq!(note(only("errors"), line), expected);
    }

    /// The type comes from the block the cursor is in, or on its closing
    /// bracket from the block around it, as the signature does. `before` is
    /// the text before the cursor.
    #[rstest]
    #[case::pipeline_end("[1 2 3] | math sum", "", "→ number")]
    #[case::after_a_pipe(
        "ls | ",
        "",
        "→ table<name: string, type: string, size: filesize, modified: datetime>"
    )]
    #[case::after_a_semicolon("[1 2 3]; ", "", "")]
    #[case::semicolon_in_a_comment("[1 2 3] # a;b", "", "→ list<int>")]
    #[case::unknown("ls | get name", "", "")]
    #[case::in_a_closure("ls | update name {|r| [1 2] | math sum", " }", "→ number")]
    #[case::in_an_unclosed_closure("ls | update name {|r| [1 2] | math sum", "", "→ number")]
    #[case::after_a_pipe_in_a_closure("ls | update name {|r| [1 2] | ", "}", "→ list<int>")]
    #[case::after_the_closure("ls | update name {|r| [1 2] | math sum }", "", "")]
    #[case::in_a_subexpression("([1 2] | math sum", " | into string)", "→ number")]
    #[case::value_of_a_let("let x = [1 2] | math sum", "", "→ number")]
    fn type_of_the_innermost_pipeline(
        #[case] before: &str,
        #[case] after: &str,
        #[case] expected: &str,
    ) {
        let line = format!("{before}{after}");
        let mut status_line = status_line(only("type"));
        assert_eq!(shown_at(&mut status_line, &line, before.len()), expected);
    }

    #[rstest]
    #[case::inside_a_char("é", 1)]
    #[case::past_the_end("ls", 3)]
    fn cursor_off_the_line_shows_nothing(#[case] line: &str, #[case] pos: usize) {
        let mut status_line = status_line(StatusLineConfig::default());
        assert_eq!(shown_at(&mut status_line, line, pos), "");
    }

    #[rstest]
    #[case::const_commands(r#""a,b" | split row ",""#, "= [a, b]")]
    #[case::arithmetic("1 + 2", "= 3")]
    #[case::control_characters(r#""a\t\u{85}b" | str trim"#, r"= a\t\u{85}b")]
    #[case::link_ending_in_bel(r#""\e]8;;http://a\u{7}" + "link""#, "= link")]
    #[case::stray_bel(r#""a\u{7}b" | str trim"#, "= ab")]
    #[case::input_ends_at_a_semicolon("[1 2 3]; describe", "= nothing")]
    #[case::literal(r#""abc""#, "")]
    #[case::not_const("ls | length", "")]
    fn preview_of_a_const_line(#[case] line: &str, #[case] expected: &str) {
        assert_eq!(status(only("preview"), line), expected);
    }

    #[test]
    fn preview_past_its_budget_is_not_shown() {
        let mut status_line = status_line(only("preview"));
        status_line.preview.budget = Duration::ZERO;

        assert_eq!(shown(&mut status_line, "1..1000000 | math sum"), "");
        if let Some(worker) = status_line.preview.worker.take() {
            worker.join().expect("the preview doesn't panic");
        }
    }

    #[test]
    fn interrupted_preview_has_no_value() {
        let mut engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        engine_state.set_signals(Signals::new(Arc::new(AtomicBool::new(true))));
        let parsed = LineParseCache::default().parsed(&engine_state, "1..1000000 | math sum");

        let preview = evaluate_const_line(&engine_state, &parsed.block, &parsed.delta);
        assert_eq!(preview, None);
    }

    /// `math abs` returns a stream at once, so the interrupt lands while the
    /// const collects it.
    #[test]
    fn const_interrupted_while_collected_is_an_error() {
        let mut engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        let interrupt = Arc::new(AtomicBool::new(false));
        engine_state.set_signals(Signals::new(interrupt.clone()));
        let interrupter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            interrupt.store(true, Ordering::Relaxed);
        });

        let line = "const xs = 1..100000000000 | math abs";
        let parsed = LineParseCache::default().parsed(&engine_state, line);
        interrupter.join().expect("the interrupter doesn't panic");
        assert!(
            parsed
                .errors
                .iter()
                .any(|error| error.to_string().contains("interrupted")),
            "{:?}",
            parsed.errors
        );
    }

    /// Like [`const_interrupted_while_collected_is_an_error`], with the
    /// preview collecting the stream: part of it is no value to show.
    #[test]
    fn preview_interrupted_while_collected_has_no_value() {
        let mut engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        let interrupt = Arc::new(AtomicBool::new(false));
        engine_state.set_signals(Signals::new(interrupt.clone()));
        let parsed = LineParseCache::default().parsed(&engine_state, "1..100000000000 | math abs");
        let interrupter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            interrupt.store(true, Ordering::Relaxed);
        });

        let preview = evaluate_const_line(&engine_state, &parsed.block, &parsed.delta);
        interrupter.join().expect("the interrupter doesn't panic");
        assert_eq!(preview, None);
    }

    /// Const evaluation recurses for each level of nesting, deeper than the
    /// default 2 MiB stack of a thread holds here.
    #[test]
    fn deeply_nested_line_previews() {
        let depth = 64;
        let line = format!("{}1 + 1{}", "(".repeat(depth), ")".repeat(depth));
        // The line is parsed on a thread as roomy as the REPL's main one.
        let shown = thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || status(only("preview"), &line))
            .expect("the test thread starts")
            .join()
            .expect("the test thread doesn't panic");
        assert_eq!(shown, "= 2");
    }

    /// A const command with a bug.
    #[derive(Clone)]
    struct Panics;

    impl Command for Panics {
        fn name(&self) -> &str {
            "panics"
        }

        fn signature(&self) -> Signature {
            Signature::build("panics").input_output_type(Type::Nothing, Type::Int)
        }

        fn description(&self) -> &str {
            "Panics when it runs."
        }

        fn is_const(&self) -> bool {
            true
        }

        fn run(
            &self,
            _: &EngineState,
            _: &mut Stack,
            _: &Call,
            _: PipelineData,
        ) -> Result<PipelineData, ShellError> {
            panic!("a const command with a bug")
        }

        fn run_const(
            &self,
            _: &StateWorkingSet,
            _: &mut Stack,
            _: &Call,
            _: PipelineData,
        ) -> Result<PipelineData, ShellError> {
            panic!("a const command with a bug")
        }
    }

    #[test]
    fn panic_in_a_preview_is_caught() {
        let mut status_line = status_line(only("preview"));
        let mut engine_state = (*status_line.engine_state).clone();
        let mut working_set = StateWorkingSet::new(&engine_state);
        working_set.add_decl(Box::new(Panics));
        let delta = working_set.render();
        engine_state
            .merge_delta(delta)
            .expect("the command is added");
        status_line.engine_state = Arc::new(engine_state);

        assert_eq!(shown(&mut status_line, "panics"), "");
        let worker = status_line
            .preview
            .worker
            .take()
            .expect("the line was evaluated");
        assert!(worker.join().is_ok(), "the panic left the worker");
    }

    #[test]
    fn line_asked_about_while_busy_is_evaluated_later() {
        let mut status_line = status_line(only("preview"));
        let (release, wait) = mpsc::channel::<()>();
        status_line.preview.worker = Some(thread::spawn(move || {
            let _ = wait.recv();
        }));

        assert_eq!(shown(&mut status_line, "1 + 2"), "");
        drop(release);
        if let Some(worker) = status_line.preview.worker.take() {
            worker.join().expect("the stand-in worker doesn't panic");
        }
        assert_eq!(shown(&mut status_line, "1 + 2"), "= 3");
    }

    #[test]
    fn preview_is_cut_past_any_row() {
        // `[1, 2, 3, …, 20000]` is over 120,000 characters.
        let preview = status(only("preview"), "1..20000 | math abs");
        assert_eq!(
            preview.graphemes(true).count(),
            "= ".len() + PREVIEW_GRAPHEMES
        );
    }

    #[test]
    fn disabled_parts_show_nothing() {
        let config = StatusLineConfig {
            errors: false,
            signature: false,
            output_type: false,
            preview: false,
            ..StatusLineConfig::default()
        };
        assert_eq!(status(config, "[1 2 3] | math sum"), "");
    }

    /// `render` cuts to the width with an ellipsis, drops the escapes of the text (which take no
    /// room), measures control characters as written and graphemes as it cuts: lam-alef is one
    /// column as a string, two as graphemes.
    #[rstest]
    #[case("abcdefghij", 20, "  abcdefghij")]
    #[case("abcdefghij", 12, "  abcdefghij")]
    #[case("abcdefghij", 10, "  abcdefg…")]
    #[case("\x1b[31mhot\x1b[0m", 20, "  hot")]
    #[case("\x1b[31mhot\x1b[0m", 4, "  h…")]
    #[case("\x1b[31mhot\x1b[0m", 5, "  hot")]
    #[case("a\u{7f}\u{85}\tb\r\n", 22, r"  a\u{7f}\u{85}\tb\r\n")]
    #[case("a\u{7f}\u{85}\tb\r\n", 21, r"  a\u{7f}\u{85}\tb\r…")]
    #[case("\u{644}\u{627}\u{644}\u{627}", 5, "  \u{644}\u{627}…")]
    fn render_fits_the_row(#[case] text: &str, #[case] columns: u16, #[case] expected: &str) {
        let style = Style::new().fg(Color::DarkGray);
        let segments = vec![(style, text.to_string())];
        assert_eq!(render(&segments, columns, false), expected);
        let colored = render(&segments, columns, true);
        assert_eq!(strip_ansi_unlikely(&colored), expected);
    }

    #[rstest]
    #[case::signature(
        "signature",
        "str substring ",
        "╰ <range> ...(rest)   The indexes to substring [start end]."
    )]
    #[case::preview_of_colored_text("preview", r#"(ansi red) + "hot""#, "= hot")]
    fn colors_follow_use_ansi_coloring(
        #[case] part: &str,
        #[case] line: &str,
        #[case] plain: &str,
    ) {
        let mut status_line = status_line(only(part));
        let mut draw = |use_ansi_coloring| {
            reedline::StatusLine::status(&mut status_line, line, line.len(), 100, use_ansi_coloring)
        };

        assert_eq!(draw(false), format!("  {plain}"));
        let colored = draw(true);
        assert!(colored.contains('\x1b'));
        assert_eq!(strip_ansi_unlikely(&colored), format!("  {plain}"));
    }

    /// The styles of a `color_config` holding only `entries`, as a theme
    /// that replaces it does.
    fn styles_from(entries: &[(&str, Value)]) -> StatusStyles {
        let config = Config {
            color_config: entries
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
            ..Config::default()
        };
        let mut engine_state = EngineState::new();
        engine_state.set_config(config);
        let stack = Stack::new();
        let style_computer = StyleComputer::from_config(&engine_state, &stack);
        StatusStyles::from_config(engine_state.get_config(), &style_computer)
    }

    #[test]
    fn styles_come_from_color_config() {
        let error = record! {
            "fg" => Value::test_string("yellow"),
            "attr" => Value::test_string("b"),
        };
        let styles = styles_from(&[
            ("status_line", Value::test_string("blue")),
            ("status_line_error", Value::test_record(error)),
        ]);

        assert_eq!(styles.text, Style::new().fg(Color::Blue));
        assert_eq!(
            styles.active,
            Style::new().fg(Color::Blue).bold().underline()
        );
        assert_eq!(styles.error, Style::new().fg(Color::Yellow).bold());
    }

    #[test]
    fn styles_not_set_are_the_status_line_own_and_red() {
        // Not the dark gray of the history hints, which would be taken for the same thing.
        let hints = Style::new().fg(Color::DarkGray);
        let styles = styles_from(&[("hints", Value::test_string("dark_gray"))]);

        assert_eq!(styles.text, StatusStyles::default_text());
        assert_ne!(styles.text, hints);
        assert_eq!(styles.active, styles.text.bold().underline());
        assert_eq!(styles.error, Style::new().fg(Color::Red));
    }

    #[rstest]
    #[case(
        "command doesn't support string input",
        "Command does not support string input",
        true
    )]
    #[case("expected int, found string", "Type mismatch", false)]
    #[case("missing range", "Missing required positional argument", false)]
    fn restated_labels(#[case] label: &str, #[case] headline: &str, #[case] expected: bool) {
        assert_eq!(restates(label, headline), expected);
    }
}
