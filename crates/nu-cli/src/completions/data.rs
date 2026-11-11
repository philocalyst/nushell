//! Completing from the data the pipeline before the cursor holds.
//!
//! What [`piped`](super::piped) knows is a type: it can say `ls` has a `type` column, never that
//! the column holds `file` and `dir`, nor which keys a TOML file has. Only reading says so. Where
//! every command of the pipeline before the cursor only reads, [`evaluate`] runs it, within a
//! budget, and completion takes the columns, the keys and the values from what it gave.
//!
//! What may run is decided by [`Policy`] on the parse of the pipeline, never on its text:
//!
//! * a call must be of a builtin command on the lists below, found by the declaration the call
//!   resolved to, so a custom command that shares the name of a builtin is refused;
//! * an alias is looked through to the call it wraps, and that is held to the same rule;
//! * a custom command is looked through to its body, which is held to the same rule, and must
//!   leave the caller's environment alone: a wrapper such as `def ls [] { ls-builtin --all }`
//!   runs, and one that writes anywhere in it does not;
//! * there are no closures, external commands, redirections, assignments or other expressions
//!   that are not plain values, operators, cell paths or such calls;
//! * `open` reads one local file of a literal name, under a size limit, that no custom or plugin
//!   converter would read;
//! * the pipeline may follow `let` and `const` statements that declare the variables it reads
//!   (`let rows = (ls); $rows | where type == `), which are held to the same rule, and `$in` at its
//!   start is what flows into the block it is in (`ls | do { $in | where type == }`).
//!
//! The run is bounded by [`BUDGET`], [`ROWS`] and [`CELLS`], on a copy of the engine's state, and
//! what it gives is kept for a few seconds so that typing a word does not run `ls` for each
//! letter. A pipeline that is not run, and why, is in the trace as the `evaluation` source.

use super::{
    Candidates, Context, Origin, SemanticSuggestion, Trace,
    cache::{CacheView, Read},
    has_marker,
    piped::is_binding,
    quote::{Quote, Spelling, quote},
    to_reedline_span,
    trace::{Lookup, Outcome, Role},
};
use chrono::{DateTime, FixedOffset, Local};
use nu_engine::eval_block;
use nu_path::AbsolutePathBuf;
use nu_protocol::{
    Config, PipelineData, Signals, Span, SuggestionKind, Type, Value,
    ast::{Block, Call, Comparison, Expr, Expression, Operator, PathMember, Pipeline, Traverse},
    debugger::WithoutDebug,
    engine::{Command, CommandType, EngineState, Stack, StateDelta, StateWorkingSet},
};
use nu_utils::time::Instant;
use reedline::Suggestion;
use std::{
    collections::HashMap,
    ffi::OsStr,
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

/// How long a pipeline may run before it is stopped.
const BUDGET: Duration = Duration::from_millis(300);
/// The most rows kept of what a pipeline gives.
const ROWS: usize = 2_000;
/// The most values kept of what a pipeline gives, at any depth.
const CELLS: usize = 100_000;
/// The most columns kept of a record.
const COLUMNS: usize = 200;
/// The most characters kept of a string.
const TEXT: usize = 200;
/// The largest file `open` is let read.
const FILE: u64 = 1 << 20;
/// How many distinct values are offered when nothing is typed, and the most of a kind that is not
/// text: beyond that they are a wall, not a choice.
const DISTINCT: usize = 50;
/// How many distinct values are matched against what is typed.
const POOL: usize = 1_000;
/// How many kinds of value a column may hold for its preview to count the rows of each.
const KINDS: usize = 8;
/// How many of a column's values its preview shows when they neither repeat nor order.
const SAMPLES: usize = 6;

/// Keywords that only choose which of the blocks given them runs, or bind a variable to a value;
/// the blocks and values are looked at too.
const CONTROL: [&str; 4] = ["if", "match", "let", "const"];
/// Builtin commands that make data from the machine and never change it. The families `sys`,
/// `help` and `scope` are on the list whole.
const SOURCES: [&str; 6] = ["ls", "ps", "open", "history", "which", "du"];
/// The families of commands on the list whole, by their first word: those that read the machine,
/// and those that compute or reshape text and numbers.
const FAMILIES: [&str; 6] = ["sys", "help", "scope", "math", "str", "split"];
/// Builtin filters that only reshape what they are given. One that takes a closure is refused for
/// the closure, not for its name.
const FILTERS: [&str; 30] = [
    "append",
    "columns",
    "compact",
    "default",
    "describe",
    "drop",
    "enumerate",
    "first",
    "flatten",
    "get",
    "group-by",
    "headers",
    "insert",
    "last",
    "length",
    "lines",
    "prepend",
    "reject",
    "rename",
    "reverse",
    "select",
    "skip",
    "sort",
    "sort-by",
    "take",
    "transpose",
    "uniq",
    "upsert",
    "values",
    "wrap",
];

/// Whether the builtin `name` is on the lists.
fn only_reads(name: &str) -> bool {
    let family = name.split(' ').next().unwrap_or_default();
    SOURCES.contains(&name)
        || FILTERS.contains(&name)
        || name == "uniq-by"
        || name == "where"
        || FAMILIES.contains(&family)
        || CONTROL.contains(&name)
        || matches!(name, "is-empty" | "is-not-empty")
}

/// Which pipelines may be run, read off their parse.
struct Policy<'a> {
    working_set: &'a StateWorkingSet<'a>,
    /// Where a relative path is relative to.
    cwd: Option<AbsolutePathBuf>,
    /// How many aliases and custom commands deep the call being looked at was found.
    depth: usize,
}

/// How many aliases and custom commands deep a call is looked for.
const ALIASES: usize = 3;

impl<'a> Policy<'a> {
    /// Why `block` may not be run, if it may not.
    fn refuse(&self, block: &Block) -> Option<String> {
        // One pipeline, after the variables it reads are declared.
        let declares = |pipeline: &Pipeline| match pipeline.elements.as_slice() {
            [only] => {
                matches!(&only.expr.expr, Expr::Call(call) if is_binding(self.working_set, call))
            }
            _ => false,
        };
        let Some((_, declared)) = block.pipelines.split_last() else {
            return Some("it is empty".into());
        };
        if !declared.iter().all(declares) {
            return Some("it is not one pipeline".into());
        }
        if self.redirects(block) {
            return Some("it has a redirection".into());
        }
        block.find_map(self.working_set, &|expression| {
            self.expression(expression)
                .map_or(ControlFlow::Continue(()), |reason| {
                    ControlFlow::Break(Some(reason))
                })
        })
    }

    /// Why the body of a custom command may not be run: what [`refuse`](Self::refuse) asks of a
    /// pipeline, asked of each of its pipelines.
    fn refuse_body(&self, block: &Block) -> Option<String> {
        if self.redirects(block) {
            return Some("it has a redirection".into());
        }
        block.find_map(self.working_set, &|expression| {
            self.expression(expression)
                .map_or(ControlFlow::Continue(()), |reason| {
                    ControlFlow::Break(Some(reason))
                })
        })
    }

    /// The same rule, one alias or custom command deeper.
    fn deeper(&self) -> Self {
        Policy {
            working_set: self.working_set,
            cwd: self.cwd.clone(),
            depth: self.depth + 1,
        }
    }

    /// Whether an element of `block` is redirected, which may write a file.
    fn redirects(&self, block: &Block) -> bool {
        (block.pipelines.iter())
            .flat_map(|pipeline| &pipeline.elements)
            .any(|element| element.redirection.is_some())
    }

    /// Why `expression` itself may not be run. What it holds is looked at on its own.
    fn expression(&self, expression: &Expression) -> Option<String> {
        match &expression.expr {
            Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Binary(_)
            | Expr::Nothing
            | Expr::String(_)
            | Expr::RawString(_)
            | Expr::DateTime(_)
            | Expr::Filepath(..)
            | Expr::Directory(..)
            | Expr::GlobPattern(..)
            | Expr::CellPath(_)
            | Expr::FullCellPath(_)
            | Expr::Var(_)
            | Expr::Range(_)
            | Expr::List(_)
            | Expr::Table(_)
            | Expr::Record(_)
            | Expr::ValueWithUnit(_)
            | Expr::StringInterpolation(_)
            | Expr::GlobInterpolation(..)
            | Expr::UnaryNot(_)
            | Expr::UnaryMinus(_)
            | Expr::Collect(..)
            | Expr::Keyword(_)
            | Expr::MatchBlock(_)
            | Expr::VarDecl(_)
            | Expr::Operator(_) => None,
            Expr::BinaryOp(_, operator, _) => {
                matches!(operator.expr, Expr::Operator(Operator::Assignment(_)))
                    .then(|| "it assigns".into())
            }
            // A block runs in place, as the keyword given it chooses (`if`, `match`); what it holds is
            // looked at on its own.
            Expr::Subexpression(id) | Expr::RowCondition(id) | Expr::Block(id) => self
                .redirects(self.working_set.get_block(*id))
                .then(|| "it has a redirection".into()),
            Expr::Call(call) => self.call(call),
            Expr::Closure(_) => Some("it runs a closure".into()),
            Expr::ExternalCall(..) => Some("it runs an external command".into()),
            other => Some(format!("it has {}", other.description())),
        }
    }

    /// Why the call may not be run.
    fn call(&self, call: &Call) -> Option<String> {
        let command = self.working_set.get_decl(call.decl_id);
        if let Some(alias) = command.as_alias() {
            // The alias runs the call it wraps, with the arguments given here added to it.
            let deeper = self.deeper();
            return (self.depth >= ALIASES)
                .then(|| "it has aliases of aliases".into())
                .or_else(|| {
                    alias.wrapped_call.find_map(self.working_set, &|wrapped| {
                        deeper
                            .expression(wrapped)
                            .map_or(ControlFlow::Continue(()), |reason| {
                                ControlFlow::Break(Some(reason))
                            })
                    })
                });
        }
        let name = command.name();
        match command.command_type() {
            // `where` is a keyword of the parser, which makes it no less a builtin.
            CommandType::Builtin | CommandType::Keyword => {}
            CommandType::Custom => return self.custom(command),
            other => return Some(format!("`{name}` is not a builtin command ({other:?})")),
        }
        if !only_reads(name) {
            return Some(format!("`{name}` is not known to only read"));
        }
        if !call.parser_info.is_empty() {
            return Some(format!("`{name}` is called with parser information"));
        }
        match name {
            "history" if call.get_named_arg("clear").is_some() => {
                Some("`history --clear` writes".into())
            }
            "open" => self.open(call),
            _ => None,
        }
    }

    /// Why a custom command may not be run: its body may not be, it changes the caller's
    /// environment, or it is too many commands deep to look through.
    fn custom(&self, command: &dyn Command) -> Option<String> {
        let name = command.name();
        let Some(block_id) = command.block_id() else {
            return Some(format!("`{name}` is a custom command without a body"));
        };
        if self.depth >= ALIASES {
            return Some(format!("`{name}` is too many commands deep"));
        }
        let body = self.working_set.get_block(block_id);
        if body.redirect_env {
            return Some(format!("`{name}` changes the environment"));
        }
        if has_marker(command, "interactive") {
            return Some(format!("`{name}` is interactive"));
        }
        // The author's promise, `@non-destructive`, stands for the look at the body.
        if has_marker(command, "non-destructive") {
            return None;
        }
        self.deeper()
            .refuse_body(body)
            .map(|reason| format!("`{name}` is a custom command, and {reason}"))
    }

    /// Why `open` may not read what it is given: it reads one local file of a literal name, of a
    /// size that is read at once, which a builtin converter reads or no converter does.
    fn open(&self, call: &Call) -> Option<String> {
        for argument in call.positional_iter() {
            let (Expr::String(path)
            | Expr::RawString(path)
            | Expr::Filepath(path, _)
            | Expr::GlobPattern(path, _)) = &argument.expr
            else {
                return Some("`open` is not given a literal path".into());
            };
            if path.contains("://") || nu_glob::is_glob(path) {
                return Some("`open` is not given one local file".into());
            }
            let Some(cwd) = &self.cwd else {
                return Some("there is no working directory".into());
            };
            let path = nu_path::expand_path_with(path, cwd, true);
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() && metadata.len() <= FILE => {}
                Ok(_) => return Some("`open` is not given a small file".into()),
                Err(error) => return Some(format!("`open` cannot read the file: {error}")),
            }
            let converter = (path.extension().and_then(OsStr::to_str))
                .and_then(|extension| {
                    self.working_set
                        .find_decl(format!("from {extension}").as_bytes())
                })
                .map(|id| self.working_set.get_decl(id));
            if converter.is_some_and(|converter| converter.command_type() != CommandType::Builtin) {
                return Some("`open` would run a converter that is not builtin".into());
            }
        }
        None
    }
}

/// How reading a pipeline went, which the trace tells, and the data it gave.
struct Verdict {
    outcome: Outcome,
    data: Option<Arc<Value>>,
    /// Why running it failed, when it ran and did.
    failed: Option<String>,
}

/// The data of the pipeline whose text is at `span` (the elements before the cursor), if it can
/// be run and gives any. The trace tells how it went.
pub(crate) fn evaluate(ctx: &Context, span: Span) -> Option<Arc<Value>> {
    let started = Instant::now();
    if let Some(trace) = ctx.trace {
        trace.open("evaluation", Role::Fallback);
    }
    let text = String::from_utf8_lossy(ctx.working_set.get_span_contents(span));
    // `$in` at the start of a pipeline in a block is what flows into the block.
    let text = match ctx.feed {
        Some(feed) if starts_with_in(&text) => format!(
            "{} | {text}",
            String::from_utf8_lossy(ctx.working_set.get_span_contents(feed))
        ),
        _ => text.into_owned(),
    };
    let verdict = read(ctx, &with_prelude(ctx, &text));
    if let Some(trace) = ctx.trace {
        let rows = verdict.data.as_deref().map_or(0, |data| rows(data).len());
        trace.close(verdict.outcome, rows, rows, started.elapsed());
    }
    verdict.data
}

/// Whether the pipeline `text` starts with `$in`.
fn starts_with_in(text: &str) -> bool {
    text.trim_start().strip_prefix("$in").is_some_and(|rest| {
        !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '-')
    })
}

/// `text` after the `let` and `const` statements of `ctx.prelude` that declare the variables it
/// reads, directly or by one another, so that `let rows = (ls); $rows | where name == ⌶` can be
/// read like `ls | where name == ⌶`.
fn with_prelude(ctx: &Context, text: &str) -> String {
    /// Whether `$name` is read in `text`, and not `$name-more`.
    fn reads(text: &str, name: &str) -> bool {
        let variable = format!("${name}");
        text.match_indices(&variable).any(|(at, _)| {
            let after = text[at + variable.len()..].chars().next();
            !after.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-')
        })
    }
    let mut wanted = text.to_owned();
    let mut declared = Vec::new();
    for span in ctx.prelude.iter().rev() {
        let statement = String::from_utf8_lossy(ctx.working_set.get_span_contents(*span));
        let name = (statement.split_whitespace().nth(1)).map(|name| name.trim_end_matches(':'));
        if name.is_some_and(|name| reads(&wanted, name)) {
            // What it reads is wanted in turn.
            wanted.push('\n');
            wanted.push_str(&statement);
            declared.push(statement.into_owned());
        }
    }
    declared.reverse();
    declared.push(text.to_owned());
    declared.join("; ")
}

/// What the pipeline `text` gives, if it may be run, as [`evaluate`] decides, for a preview: it
/// is asked for once a suggestion is selected, with no completion around it.
pub(crate) fn evaluate_text(
    engine_state: &EngineState,
    stack: &Stack,
    text: &str,
) -> Option<Arc<Value>> {
    read_text(
        engine_state,
        stack,
        &evaluations(engine_state, stack),
        None,
        text,
    )
    .data
}

/// The one cache of what pipelines gave, shared by everything that evaluates one outside a
/// completion (hover, previews, the status line, record help), for the environment it runs in:
/// moving the cursor over `ls | first` runs `ls` once per [`EVALUATION_LIFETIME`], not once per
/// paint.
///
/// [`EVALUATION_LIFETIME`]: super::cache
fn evaluations(engine_state: &EngineState, stack: &Stack) -> CacheView {
    static SHARED: std::sync::OnceLock<super::cache::CompletionCache> = std::sync::OnceLock::new();
    let cache =
        SHARED.get_or_init(|| super::cache::CompletionCache::new(super::cache::DEFAULT_CACHE_SIZE));
    CacheView::new(
        cache.clone(),
        super::cache::CacheEnv::of(engine_state, stack),
    )
}

/// Why the pipeline `text` fails, when it may be run as [`evaluate`] decides and running it does:
/// what the line will say once it is run, said before. `None` while it runs, gives bytes, or runs
/// past its budget.
pub(crate) fn failure_of(engine_state: &EngineState, stack: &Stack, text: &str) -> Option<String> {
    read_text(
        engine_state,
        stack,
        &evaluations(engine_state, stack),
        None,
        text,
    )
    .failed
}

/// Parse `text` apart from the line, decide whether it may run, and run it or take what it gave a
/// moment ago.
fn read(ctx: &Context, text: &str) -> Verdict {
    read_text(
        ctx.working_set.permanent_state,
        ctx.stack,
        ctx.cache,
        ctx.trace,
        text,
    )
}

/// [`read`], with what it needs of a completion context.
fn read_text(
    engine_state: &EngineState,
    stack: &Stack,
    cache: &CacheView,
    trace: Option<&Trace>,
    text: &str,
) -> Verdict {
    let refused = |reason: String| {
        if let Some(trace) = trace {
            trace.note(reason);
        }
        Verdict {
            outcome: Outcome::Refused,
            data: None,
            failed: None,
        }
    };

    let mut working_set = StateWorkingSet::new(engine_state);
    let block = nu_parser::parse(&mut working_set, None, text.as_bytes(), false);
    if !working_set.parse_errors.is_empty() || !working_set.compile_errors.is_empty() {
        return refused("it does not parse on its own".into());
    }
    let policy = Policy {
        working_set: &working_set,
        cwd: engine_state.cwd(Some(stack)).ok(),
        depth: 0,
    };
    if let Some(reason) = policy.refuse(&block) {
        return refused(reason);
    }

    let (read, outcome, lookup) = match cache.evaluation(text) {
        Some(read) => (read, Outcome::Cached, Lookup::Hit),
        None => {
            let delta = working_set.render();
            let read = run(engine_state, delta, stack.clone(), block);
            cache.remember_evaluation(text, read.clone());
            (read, Outcome::Ran, Lookup::Miss)
        }
    };
    if let Some(trace) = trace {
        trace.cached(lookup);
    }
    match read {
        Read::Data(data) => Verdict {
            outcome,
            data: Some(data),
            failed: None,
        },
        Read::TimedOut => {
            if let Some(trace) = trace {
                trace.note(format!("it ran past {BUDGET:?}"));
            }
            Verdict {
                outcome: Outcome::TimedOut,
                data: None,
                failed: None,
            }
        }
        Read::Failed(error) => {
            if let Some(trace) = trace {
                trace.note(error.clone());
            }
            Verdict {
                outcome: Outcome::Declined,
                data: None,
                failed: Some(error).filter(|error| error != GIVES_BYTES),
            }
        }
    }
}

/// Run `block` on a copy of the engine's state and of the stack, for at most [`BUDGET`].
fn run(engine_state: &EngineState, delta: StateDelta, stack: Stack, block: Arc<Block>) -> Read {
    let mut state = engine_state.clone();
    if let Err(error) = state.merge_delta(delta) {
        return Read::Failed(error.to_string());
    }
    let interrupt = Arc::new(AtomicBool::new(false));
    state.set_signals(Signals::new(Arc::clone(&interrupt)));

    let (sender, receiver) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("completion-evaluation".into())
        .spawn(move || {
            // The reader may have given up by now.
            let _ = sender.send(give(&state, stack, &block));
        });
    if let Err(error) = spawned {
        return Read::Failed(error.to_string());
    }
    match receiver.recv_timeout(BUDGET) {
        Ok(Ok(value)) => Read::Data(Arc::new(clip(value, &mut { CELLS }))),
        Ok(Err(error)) => Read::Failed(error),
        Err(RecvTimeoutError::Timeout) => {
            interrupt.store(true, Ordering::Relaxed);
            Read::TimedOut
        }
        Err(RecvTimeoutError::Disconnected) => Read::Failed("the evaluation panicked".into()),
    }
}

/// What `block` gives: a value, or the first [`ROWS`] of a stream.
/// What [`give`] says of a pipeline that gives bytes: no failure, only no data.
const GIVES_BYTES: &str = "it gives bytes, not data";

/// What `block` gives, or why it failed: what the error points at, if it says, as that is the
/// part of it that says what to do (`Filesize is only supported for binary/byte stream input`),
/// else its message.
fn give(state: &EngineState, mut stack: Stack, block: &Block) -> Result<Value, String> {
    let span = Span::unknown();
    let body = eval_block::<WithoutDebug>(state, &mut stack, block, PipelineData::empty())
        .map_err(|error| {
            use miette::Diagnostic;
            let label = (error.labels().into_iter().flatten())
                .find_map(|label| label.label().map(str::to_owned))
                .filter(|label| !label.trim().is_empty());
            label.unwrap_or_else(|| error.to_string())
        })?
        .body;
    match body {
        PipelineData::Empty => Ok(Value::nothing(span)),
        PipelineData::Value(value, _) => Ok(value),
        PipelineData::ListStream(stream, _) => {
            Ok(Value::list(stream.into_iter().take(ROWS).collect(), span))
        }
        PipelineData::ByteStream(..) => Err(GIVES_BYTES.into()),
    }
}

/// `value` cut down to what completion reads: at most [`ROWS`] rows, [`COLUMNS`] columns and
/// `budget` values in all, strings of at most [`TEXT`] characters, and nothing of what has no
/// meaning to it (errors, bytes, closures, custom values).
fn clip(value: Value, budget: &mut usize) -> Value {
    let span = value.span();
    *budget = budget.saturating_sub(1);
    match value {
        Value::String { val, .. } if val.chars().count() > TEXT => {
            Value::string(val.chars().take(TEXT).collect::<String>(), span)
        }
        Value::List { vals, .. } => {
            let mut kept = Vec::new();
            for item in vals.into_iter().take(ROWS) {
                if *budget == 0 {
                    break;
                }
                kept.push(clip(item, budget));
            }
            Value::list(kept, span)
        }
        Value::Record { val, .. } => {
            let mut kept = nu_protocol::Record::new();
            for (key, item) in val.into_owned().into_iter().take(COLUMNS) {
                if *budget == 0 {
                    break;
                }
                kept.push(key, clip(item, budget));
            }
            Value::record(kept, span)
        }
        Value::Error { .. }
        | Value::Binary { .. }
        | Value::Closure { .. }
        | Value::Custom { .. } => Value::nothing(span),
        other => other,
    }
}

/// The rows of `data`: the records of a table, or the one record or value there is.
pub(crate) fn rows(data: &Value) -> &[Value] {
    match data {
        Value::List { vals, .. } => vals,
        other => std::slice::from_ref(other),
    }
}

/// What the cells of the column `key` of `rows` hold between them, to preview the column by: how
/// many rows hold each value when there are a few kinds of them (`file 32, dir 12`), the lowest,
/// middle and highest of values that order (sizes, numbers, dates), else a few of the values.
/// Rows without a value are counted as `(empty)`. `None` for a column of records or lists, whose
/// members say more.
pub(crate) fn column_summary(rows: &[Value], key: &str) -> Option<Value> {
    let span = Span::unknown();
    let cells: Vec<Option<&Value>> = (rows.iter())
        .map(|row| {
            row.as_record()
                .ok()?
                .get(key)
                .filter(|cell| !cell.is_nothing())
        })
        .collect();
    let present: Vec<&Value> = cells.iter().flatten().copied().collect();
    let empty = cells.len() - present.len();
    let plain = |cell: &&Value| {
        !matches!(
            cell,
            Value::Record { .. }
                | Value::List { .. }
                | Value::Closure { .. }
                | Value::Binary { .. }
                | Value::Custom { .. }
                | Value::Error { .. }
        )
    };
    if present.is_empty() || !present.iter().all(plain) {
        return None;
    }

    let config = Config::default();
    let mut kinds: Vec<(String, i64, &Value)> = Vec::new();
    for cell in &present {
        let text = cell.to_abbreviated_string(&config);
        match kinds.iter_mut().find(|(known, ..)| *known == text) {
            Some((_, count, _)) => *count += 1,
            None => kinds.push((text, 1, cell)),
        }
    }
    let ty = present[0].get_type();
    let orders = matches!(
        ty,
        Type::Int | Type::Float | Type::Filesize | Type::Duration | Type::Date
    ) && present.iter().all(|cell| cell.get_type() == ty);

    let mut summary = nu_protocol::Record::new();
    if kinds.len() <= KINDS && kinds.len() < present.len() {
        kinds.sort_by_key(|(_, count, _)| std::cmp::Reverse(*count));
        for (text, count, _) in kinds {
            summary.push(text, Value::int(count, span));
        }
    } else if orders {
        let mut sorted = present;
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let at = |index: usize| sorted.get(index).map(|cell| (*cell).clone());
        for (name, cell) in [
            ("lowest", at(0)),
            ("middle", at(sorted.len() / 2)),
            ("highest", sorted.last().map(|cell| (*cell).clone())),
        ] {
            if let Some(cell) = cell {
                summary.push(name, cell);
            }
        }
    } else {
        let some = kinds
            .into_iter()
            .take(SAMPLES)
            .map(|(_, _, cell)| cell.clone());
        return Some(Value::list(some.collect(), span));
    }
    if empty > 0 {
        summary.push("(empty)", Value::int(empty as i64, span));
    }
    Some(Value::record(summary, span))
}

/// A value as it is written after an operator: text is quoted as the parser needs, anything else
/// is a literal of its own type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Operand {
    Text(String),
    Literal(String),
}

impl Operand {
    /// How `value` is written to be compared with `operator`, if it is an operand for it.
    fn of(value: &Value, operator: Comparison) -> Option<Self> {
        use Comparison::*;
        let text = matches!(
            operator,
            RegexMatch | NotRegexMatch | StartsWith | NotStartsWith | EndsWith | NotEndsWith
        );
        let ordered = matches!(
            operator,
            LessThan | LessThanOrEqual | GreaterThan | GreaterThanOrEqual
        );
        let equal = matches!(operator, Equal | NotEqual);
        match value {
            Value::String { val, .. } if equal || text => Some(Self::Text(val.clone())),
            Value::Int { val, .. } if equal || ordered => Some(Self::Literal(val.to_string())),
            Value::Float { val, .. } if (equal || ordered) && val.is_finite() => {
                Some(Self::Literal(format!("{val:?}")))
            }
            Value::Bool { val, .. } if equal => Some(Self::Literal(val.to_string())),
            Value::Filesize { val, .. } if equal || ordered => {
                Some(Self::Literal(format!("{}b", val.get())))
            }
            Value::Duration { val, .. } if equal || ordered => {
                Some(Self::Literal(format!("{val}ns")))
            }
            Value::Nothing { .. } if equal => Some(Self::Literal("null".into())),
            _ => None,
        }
    }

    /// What the operand is matched by.
    fn haystack(&self) -> &str {
        match self {
            Self::Text(text) | Self::Literal(text) => text,
        }
    }
}

/// The values the column at `members` holds, to compare with `operator`: the distinct ones, most
/// frequent first, each with the number of rows that have it. A column of numbers, sizes or
/// durations with more than [`DISTINCT`] different values offers none, nor do dates.
pub(crate) fn operands(members: &[PathMember], operator: Comparison, ctx: &Context) -> Candidates {
    let mut candidates = Candidates::default();
    let Some(data) = ctx.prior.and_then(|span| evaluate(ctx, span)) else {
        return candidates;
    };
    if let Some(moments) = dated(rows(&data), members) {
        return relative_dates(&moments, operator, ctx);
    }

    // Each operand with the number of rows that have it, in the order first seen.
    let mut seen: HashMap<Operand, usize> = HashMap::new();
    let mut counted: Vec<(Operand, Type, usize)> = Vec::new();
    for row in rows(&data) {
        let Ok(cell) = row.follow_cell_path(members) else {
            continue;
        };
        let Some(operand) = Operand::of(&cell, operator) else {
            continue;
        };
        match seen.get(&operand) {
            Some(&at) => counted[at].2 += 1,
            None => {
                seen.insert(operand.clone(), counted.len());
                counted.push((operand, cell.get_type(), 1));
            }
        }
    }
    let wall = counted.len() > DISTINCT
        && counted
            .iter()
            .any(|(operand, ..)| matches!(operand, Operand::Literal(_)));
    // Most frequent first; a stable sort keeps the order seen among equals.
    counted.sort_by_key(|(.., count)| std::cmp::Reverse(*count));

    let typed = ctx.prefix_str();
    let inside = Quote::opened_by(&typed);
    let needle = inside.map_or(&*typed, |_| &typed[1..]);
    let needle = needle.strip_suffix(['"', '\'', '`']).unwrap_or(needle);
    candidates.needle = Some(needle.to_owned());
    candidates.ordered = true;
    candidates.typos = false;
    // A number on its way to a size or a duration goes on with the units it can have, the
    // likeliest first, ahead of the values that start like it.
    if let Some(ty) = counted.first().map(|(_, ty, _)| ty) {
        let before = candidates.items.len();
        push_units(&mut candidates, ty, needle, ctx);
        candidates.sort &= candidates.items.len() == before;
    }
    if wall {
        return candidates;
    }
    let shown = if needle.is_empty() { DISTINCT } else { POOL };

    // Text that needs quotes is in single quotes, unless the user has opened others.
    let spelling = Spelling {
        inside: inside.or(Some(Quote::Single)),
        ..Spelling::default()
    };
    let span = to_reedline_span(ctx.span, ctx.offset);
    for (operand, ty, count) in counted.into_iter().take(shown) {
        let value = match &operand {
            Operand::Text(text) => quote(text, spelling).into_owned(),
            Operand::Literal(literal) => literal.clone(),
        };
        candidates.push_as(
            operand.haystack(),
            SemanticSuggestion {
                suggestion: Suggestion {
                    value,
                    span,
                    description: Some(match count {
                        1 => "1 row".into(),
                        count => format!("{count} rows"),
                    }),
                    append_whitespace: true,
                    ..Suggestion::default()
                },
                kind: Some(SuggestionKind::Value(ty)),
                origin: Origin::Derived,
                ..SemanticSuggestion::default()
            },
        );
    }
    candidates
}

/// The dates the column at `members` of `rows` holds, when it holds dates.
fn dated(rows: &[Value], members: &[PathMember]) -> Option<Vec<DateTime<FixedOffset>>> {
    let moments: Vec<_> = rows
        .iter()
        .filter_map(|row| match row.follow_cell_path(members).ok()?.as_ref() {
            Value::Date { val, .. } => Some(*val),
            _ => None,
        })
        .collect();
    (!moments.is_empty()).then_some(moments)
}

/// The spans of time a date is usually compared across, as a duration is written and in seconds.
const SPANS_OF_TIME: &[(&str, i64)] = &[
    ("1hr", 3_600),
    ("1day", 86_400),
    ("1wk", 604_800),
    ("4wk", 2_419_200),
    ("52wk", 31_449_600),
];

/// What a date is compared with: a time relative to now, `(date now) - 1day`, or the span of time
/// itself after `older-than` and `newer-than`, each with how many of `moments` it lets through.
fn relative_dates(
    moments: &[DateTime<FixedOffset>],
    operator: Comparison,
    ctx: &Context,
) -> Candidates {
    use Comparison::*;
    let now = Local::now().fixed_offset();
    let spans = matches!(operator, OlderThan | NewerThan);
    let newer = matches!(operator, GreaterThan | GreaterThanOrEqual | NewerThan);
    let older = matches!(operator, LessThan | LessThanOrEqual | OlderThan);
    let mut candidates = Candidates {
        ordered: true,
        typos: false,
        needle: Some(ctx.prefix_str().into_owned()),
        ..Candidates::default()
    };
    if !newer && !older {
        return candidates;
    }
    let span = to_reedline_span(ctx.span, ctx.offset);
    for (written, seconds) in SPANS_OF_TIME {
        let since = now - chrono::Duration::seconds(*seconds);
        let count = moments
            .iter()
            .filter(|moment| {
                if newer {
                    **moment > since
                } else {
                    **moment < since
                }
            })
            .count();
        let value = if spans {
            (*written).to_owned()
        } else {
            format!("(date now) - {written}")
        };
        candidates.push_as(
            &value,
            SemanticSuggestion {
                suggestion: Suggestion {
                    value: value.clone(),
                    span,
                    description: Some(match count {
                        1 => "1 row".into(),
                        count => format!("{count} rows"),
                    }),
                    append_whitespace: true,
                    ..Suggestion::default()
                },
                kind: Some(SuggestionKind::Value(if spans {
                    Type::Duration
                } else {
                    Type::Date
                })),
                origin: Origin::Derived,
                ..SemanticSuggestion::default()
            },
        );
    }
    candidates
}

/// The spans of time that can follow `(date now) - `, or the units the number being written can
/// have.
pub(crate) fn durations(ctx: &Context) -> Candidates {
    let needle = ctx.prefix_str().into_owned();
    let mut candidates = Candidates {
        ordered: true,
        typos: false,
        needle: Some(needle.clone()),
        sort: false,
        ..Candidates::default()
    };
    if needle.is_empty() {
        let span = to_reedline_span(ctx.span, ctx.offset);
        for (written, _) in SPANS_OF_TIME {
            candidates.push_as(
                *written,
                SemanticSuggestion {
                    suggestion: Suggestion {
                        value: (*written).to_owned(),
                        span,
                        append_whitespace: true,
                        ..Suggestion::default()
                    },
                    kind: Some(SuggestionKind::Value(Type::Duration)),
                    ..SemanticSuggestion::default()
                },
            );
        }
    } else {
        push_units(&mut candidates, &Type::Duration, &needle, ctx);
    }
    candidates
}

/// The number `needle` starts with and the rest of it, when it is a number on its way to having
/// a unit.
fn number_being_written(needle: &str) -> Option<(&str, &str)> {
    let digits = needle
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(needle.len());
    (digits > 0 && needle.starts_with(|c: char| c.is_ascii_digit()))
        .then(|| needle.split_at(digits))
}

/// Offers the number `needle` with each unit a value of type `ty` can have that goes on from
/// what is written of it, biggest step first: `1kb`, `1mb`, `1gb` for `1` where a size is wanted.
pub(crate) fn push_units(candidates: &mut Candidates, ty: &Type, needle: &str, ctx: &Context) {
    const SIZES: &[&str] = &["kb", "mb", "gb", "tb", "b", "kib", "mib", "gib"];
    const TIMES: &[&str] = &["sec", "min", "hr", "day", "wk", "ms"];
    let units = match ty {
        Type::Filesize => SIZES,
        Type::Duration => TIMES,
        _ => return,
    };
    let Some((number, unit)) = number_being_written(needle) else {
        return;
    };
    let unit = unit.to_ascii_lowercase();
    let span = to_reedline_span(ctx.span, ctx.offset);
    for name in units.iter().filter(|name| name.starts_with(unit.as_str())) {
        let value = format!("{number}{name}");
        candidates.push_as(
            &value,
            SemanticSuggestion {
                suggestion: Suggestion {
                    value: value.clone(),
                    span,
                    append_whitespace: true,
                    ..Suggestion::default()
                },
                kind: Some(SuggestionKind::Value(ty.clone())),
                ..SemanticSuggestion::default()
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine with the shell commands.
    fn engine() -> EngineState {
        nu_command::add_shell_command_context(nu_cmd_lang::create_default_context())
    }

    /// Why `source` may not be run in `engine`, if it may not.
    fn refusal(engine: &EngineState, source: &str) -> Option<String> {
        let mut working_set = StateWorkingSet::new(engine);
        let block = nu_parser::parse(&mut working_set, None, source.as_bytes(), false);
        assert!(
            working_set.parse_errors.is_empty(),
            "{source}: {:?}",
            working_set.parse_errors
        );
        let policy = Policy {
            working_set: &working_set,
            cwd: std::env::current_dir()
                .ok()
                .and_then(|cwd| AbsolutePathBuf::try_from(cwd).ok()),
            depth: 0,
        };
        policy.refuse(&block)
    }

    #[test]
    fn what_only_reads_may_run() {
        let engine = engine();
        for source in [
            "ls",
            "ls | where type == dir | select name | first 3",
            "ps | where name =~ 'x' | sort-by cpu",
            "sys host",
            "sys disks | get mount",
            "help commands | where name == ls",
            "scope variables",
            "[[a b]; [1 2]] | rename c",
            "{a: 1, b: {c: 2}} | get b",
            "open Cargo.toml | get package",
            "ls | group-by type | transpose",
            "which ls",
            // A condition written as a closure runs only what is in it, which is looked at too.
            "ls | where {|f| $f.size > 1}",
            // The variables it reads are declared first, by statements that only read.
            "let rows = (ls); $rows | where type == dir",
            "const rows = [[a]; [1]]; let b = ($rows | get a); $b",
        ] {
            assert_eq!(refusal(&engine, source), None, "{source}");
        }
    }

    #[test]
    fn what_may_write_or_run_other_code_may_not() {
        let engine = engine();
        for source in [
            "rm foo",
            "ls | save out.txt",
            "ls | each {|f| $f.name}",
            "ls | sort-by --custom {|a, b| $a.name < $b.name}",
            "ls | insert x (rm foo)",
            "ls | where (mkdir $in.name) == null",
            "ls | where {|f| rm $f.name}",
            "^ls",
            "ls out> out.txt",
            "mut x = 1; $x = 2",
            "history --clear",
            "let rows = (rm foo); $rows",
            "let rows = ls | save out.txt; $rows",
            "ls; ls",
            "open https://example.com",
            "open *.toml",
            "open nothing-here.toml",
            "http get https://example.com",
            "cd /tmp",
        ] {
            assert!(refusal(&engine, source).is_some(), "{source}");
        }
    }

    #[test]
    fn a_custom_command_runs_when_its_body_only_reads() {
        let mut engine = engine();
        let mut working_set = StateWorkingSet::new(&engine);
        // The user's own `ls`: a builtin under another name, wrapped in a choice.
        nu_parser::parse(
            &mut working_set,
            None,
            b"alias ls-builtin = ls
            def ls [...args: glob] {
              if ($args | is-empty) {
                ls-builtin --all | sort-by type name
              } else {
                ls-builtin --all ...$args | sort-by type name
              }
            }
            def --env goto [] { cd /tmp; ls }
            def outer [] { ls | first 3 }",
            false,
        );
        assert!(working_set.parse_errors.is_empty());
        engine.merge_delta(working_set.render()).expect("merge");
        assert_eq!(refusal(&engine, "ls | where type == dir"), None);
        assert_eq!(refusal(&engine, "outer | get name"), None);
        let why = refusal(&engine, "goto").expect("refused");
        assert!(why.contains("environment"), "{why}");
    }

    #[test]
    fn a_custom_command_runs_when_its_author_promises_it_is_non_destructive() {
        let mut engine = engine();
        let mut working_set = StateWorkingSet::new(&engine);
        nu_parser::parse(
            &mut working_set,
            None,
            b"def risky [] { ^git branch | lines | wrap name }
            @non-destructive
            def branches [] { ^git branch | lines | wrap name }
            alias names = branches
            @non-destructive
            def --env goto [] { cd /tmp; ls }
            @non-destructive
            @interactive
            def pick [] { ^fzf }",
            false,
        );
        assert!(working_set.parse_errors.is_empty());
        engine.merge_delta(working_set.render()).expect("merge");
        // Without the promise the external call in the body is refused; with it, the body is trusted.
        assert!(refusal(&engine, "risky").is_some());
        assert_eq!(refusal(&engine, "branches | where name == 'main'"), None);
        // An alias of it counts too.
        assert_eq!(refusal(&engine, "names"), None);
        // The promise does not excuse a changed environment, nor a command that owns the terminal.
        assert!(refusal(&engine, "goto").is_some_and(|why| why.contains("environment")));
        assert!(refusal(&engine, "pick").is_some_and(|why| why.contains("interactive")));
    }

    #[test]
    fn a_custom_command_is_refused_whatever_it_is_named() {
        let mut engine = engine();
        let mut working_set = StateWorkingSet::new(&engine);
        nu_parser::parse(
            &mut working_set,
            None,
            b"def ls [] { rm foo }\nalias lsa = ls\nalias danger = rm",
            false,
        );
        assert!(working_set.parse_errors.is_empty());
        engine.merge_delta(working_set.render()).expect("merge");
        let why = refusal(&engine, "ls").expect("refused");
        assert!(why.contains("custom command"), "{why}");
        // An alias is the call it wraps: `lsa` wraps the custom `ls`, `danger` the real `rm`.
        assert!(refusal(&engine, "lsa").is_some());
        assert!(refusal(&engine, "danger foo").is_some());
    }

    #[test]
    fn an_alias_of_a_command_that_reads_is_run() {
        let mut engine = engine();
        let mut working_set = StateWorkingSet::new(&engine);
        nu_parser::parse(&mut working_set, None, b"alias lsa = ls --all", false);
        engine.merge_delta(working_set.render()).expect("merge");
        assert_eq!(refusal(&engine, "lsa | first 2"), None);
    }

    #[test]
    fn what_is_kept_of_a_big_value_is_bounded() {
        let span = Span::unknown();
        let many = (0..ROWS * 2).map(|i| Value::int(i as i64, span)).collect();
        let kept = clip(Value::list(many, span), &mut { CELLS });
        assert_eq!(rows_of(&kept), ROWS);

        // The values kept at any depth are counted against one budget.
        let table = (0..100)
            .map(|_| Value::list((0..100).map(|i| Value::int(i, span)).collect(), span))
            .collect();
        let kept = clip(Value::list(table, span), &mut 1_000);
        let cells: usize = rows(&kept).iter().map(rows_of).sum();
        assert!(cells < 1_000 && cells > 0, "{cells}");

        let long = Value::string("x".repeat(TEXT * 2), span);
        assert_eq!(clip(long, &mut 10).as_str().expect("a string").len(), TEXT);
        let bytes = Value::binary(vec![0; 1 << 20], span);
        assert!(matches!(clip(bytes, &mut 10), Value::Nothing { .. }));
    }

    fn rows_of(value: &Value) -> usize {
        match value {
            Value::List { vals, .. } => vals.len(),
            _ => 0,
        }
    }

    #[test]
    fn a_column_is_previewed_by_what_its_cells_hold() {
        let span = Span::unknown();
        let rows = |cells: Vec<Value>| -> Vec<Value> {
            (cells.into_iter())
                .map(|cell| Value::record(nu_protocol::record! { "c" => cell }, span))
                .collect()
        };
        let text = |text: &str| Value::string(text, span);
        // A few kinds of value are counted, the commonest first.
        let kinds = rows(vec![text("file"), text("dir"), text("file")]);
        let counted = column_summary(&kinds, "c").expect("a summary");
        let counted = counted.as_record().expect("a record");
        assert_eq!(counted.columns().collect::<Vec<_>>(), ["file", "dir"]);
        // Sizes that do not repeat are ranged.
        let sizes = rows(
            (1..=20)
                .map(|kb| Value::filesize(kb * 1000, span))
                .collect(),
        );
        let ranged = column_summary(&sizes, "c").expect("a summary");
        let ranged = ranged.as_record().expect("a record");
        assert_eq!(
            ranged.columns().collect::<Vec<_>>(),
            ["lowest", "middle", "highest"]
        );
        // Records say more by their members.
        let nested = rows(vec![Value::record(nu_protocol::Record::new(), span)]);
        assert_eq!(column_summary(&nested, "c"), None);
    }
}
