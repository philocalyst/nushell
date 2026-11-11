//! A record of how one completion was answered, which `debug complete` reports.
//!
//! A [`Trace`] rides on the engine and, through [`Context::trace`], reaches every source. Only
//! a completion that is being traced has one; any other holds `None`, so each hook is a branch
//! that is skipped and allocates nothing.
//!
//! What is gathered:
//!
//! - the readings of the line: the site the cursor is at, and what a completer is handed there;
//! - the sources asked, in order, each with what it did, how many candidates it offered and how
//!   many matching kept, whether the cache answered, and how long it took. A custom completer's
//!   source also keeps what it returned and the parts of it that were not suggestions;
//! - the candidates that matching removed, and why.

use super::{
    Context, DeclaredInputs,
    completer::{CompletionSite, SiteKind, find_flag},
    custom_completions::completer_input,
    piped::input_record,
    span_record, to_reedline_span,
};
use nu_protocol::{
    CommandWideCompleter, Completion, IntoValue, Record, ShellError, Span, SyntaxShape, Value,
    ast::Expr, engine::StateWorkingSet, record,
};
use std::{cell::RefCell, rc::Rc, time::Duration};

/// How many removed candidates are kept for the report; the rest are only counted.
const DROPPED_SAMPLE: usize = 50;

/// Where a source stands in the chain it was asked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoValue)]
#[nu_value(rename_all = "snake_case")]
pub(crate) enum Role {
    /// The first of its chain.
    Primary,
    /// Asked only because the sources before it left the slot open, such as files after nothing
    /// answered.
    Fallback,
}

/// What a source did when it was asked, reported in snake case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoValue)]
#[nu_value(rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// It offered candidates, possibly none: the chain stops here unless it asked for more.
    Answered,
    /// It had nothing to say, so the next source was asked.
    Declined,
    /// An earlier source answered, so it was never asked.
    Skipped,
    /// It is an `@interactive` completer, which only the line editor can run.
    NeedsTerminal,
    /// It ran the pipeline before the cursor and read its data.
    Ran,
    /// It read the data of a pipeline it ran a moment ago.
    Cached,
    /// It would not run the pipeline, which may write: the `error` says why.
    Refused,
    /// The pipeline ran past its time budget and was stopped.
    TimedOut,
}

/// What the cache said when a source asked it. A miss outranks a hit: a source that read one
/// directory from the cache and another from the disk did the work of the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Lookup {
    Hit,
    Miss,
}

/// A part of a custom completer's answer that was not used as a suggestion, or was used with
/// something ignored.
#[derive(Debug, Clone)]
pub(crate) struct Fault {
    /// The position in the returned list of the item at fault, if it is one.
    pub(crate) at: Option<usize>,
    pub(crate) reason: String,
}

/// One source, asked once.
struct Step {
    reading: usize,
    name: String,
    role: Role,
    outcome: Outcome,
    /// Set when the completer's block failed; the outcome is then `failed`.
    failed: Option<ShellError>,
    /// Why a step that did not run what it was asked to has the outcome it has.
    note: Option<String>,
    candidates: usize,
    kept: usize,
    cached: Option<Lookup>,
    duration: Duration,
    /// For a custom completer: the inputs it declares, what it returned, and the faults in it.
    handed: Option<DeclaredInputs>,
    raw: Option<Value>,
    faults: Vec<Fault>,
}

/// A candidate that matching removed.
#[derive(IntoValue)]
struct Dropped {
    source: String,
    text: String,
    reason: String,
}

/// One way the line was read at the cursor, with what a completer is handed there.
struct Reading {
    target: reedline::Span,
    site: Value,
    inputs: Value,
}

#[derive(Default)]
struct Log {
    readings: Vec<Reading>,
    steps: Vec<Step>,
    /// The steps being asked, innermost last: a delegated line is completed within the source
    /// that delegated it.
    open: Vec<usize>,
    dropped: Vec<Dropped>,
    dropped_total: usize,
}

impl Log {
    /// The step being asked.
    fn current(&mut self) -> Option<&mut Step> {
        let at = *self.open.last()?;
        self.steps.get_mut(at)
    }
}

/// What was found out while one line was completed. Clones share the log.
#[derive(Clone, Default)]
pub(crate) struct Trace(Rc<RefCell<Log>>);

impl Trace {
    /// Reads the site the cursor is at: the reading the sources asked next belong to.
    pub(crate) fn reading(&self, site: &CompletionSite, ctx: &Context) {
        let reading = Reading {
            target: to_reedline_span(site.span, ctx.offset),
            site: site_value(site, ctx),
            inputs: completer_input(ctx, DeclaredInputs::all()),
        };
        self.0.borrow_mut().readings.push(reading);
    }

    /// A source is about to be asked.
    pub(crate) fn open(&self, name: &str, role: Role) {
        let mut log = self.0.borrow_mut();
        let step = Step::new(log.readings.len().saturating_sub(1), name, role);
        let at = log.steps.len();
        log.open.push(at);
        log.steps.push(step);
    }

    /// The source being asked has answered, or declined.
    pub(crate) fn close(&self, outcome: Outcome, candidates: usize, kept: usize, took: Duration) {
        let mut log = self.0.borrow_mut();
        if let Some(step) = log.current() {
            step.outcome = outcome;
            step.candidates = candidates;
            step.kept = kept;
            step.duration = took;
        }
        log.open.pop();
    }

    /// A source that was never asked, since an earlier one answered.
    pub(crate) fn skipped(&self, name: &str, role: Role) {
        let mut log = self.0.borrow_mut();
        let mut step = Step::new(log.readings.len().saturating_sub(1), name, role);
        step.outcome = Outcome::Skipped;
        log.steps.push(step);
    }

    /// What the cache said to the source being asked.
    pub(crate) fn cached(&self, lookup: Lookup) {
        if let Some(step) = self.0.borrow_mut().current() {
            step.cached = step.cached.max(Some(lookup));
        }
    }

    /// The inputs the completer being asked declares.
    pub(crate) fn handed(&self, wanted: DeclaredInputs) {
        if let Some(step) = self.0.borrow_mut().current() {
            step.handed = Some(wanted);
        }
    }

    /// What the completer being asked returned, as it came back.
    pub(crate) fn raw(&self, value: Value) {
        if let Some(step) = self.0.borrow_mut().current() {
            step.raw = Some(value);
        }
    }

    /// The parts of the answer that were not used as suggestions.
    pub(crate) fn faults(&self, faults: Vec<Fault>) {
        if let Some(step) = self.0.borrow_mut().current() {
            step.faults = faults;
        }
    }

    /// Why the source being asked ended as it did, when its outcome does not say.
    pub(crate) fn note(&self, reason: impl Into<String>) {
        if let Some(step) = self.0.borrow_mut().current() {
            step.note = Some(reason.into());
        }
    }

    /// The completer being asked failed with `error`.
    pub(crate) fn fail(&self, error: ShellError) {
        if let Some(step) = self.0.borrow_mut().current() {
            step.failed = Some(error);
        }
    }

    /// `text` was removed for `reason`. `source` names who offered it; without it, the source
    /// being asked.
    pub(crate) fn dropped(&self, source: Option<&str>, text: String, reason: impl Into<String>) {
        let mut log = self.0.borrow_mut();
        log.dropped_total += 1;
        if log.dropped.len() >= DROPPED_SAMPLE {
            return;
        }
        let source = source
            .map(str::to_owned)
            .or_else(|| log.current().map(|step| step.name.clone()))
            .unwrap_or_default();
        log.dropped.push(Dropped {
            source,
            text,
            reason: reason.into(),
        });
    }

    /// The part of the report that is the trace: `target`, `site`, `inputs`, `other_sites`,
    /// `sources`, `dropped` and `dropped_total`.
    pub(crate) fn finish(&self, span: Span) -> Record {
        let mut log = self.0.borrow_mut();
        let dropped = std::mem::take(&mut log.dropped);
        let mut readings = log.readings.iter().enumerate();
        let first = readings.next().map(|(_, first)| first);

        let target = first.map_or_else(
            || Value::nothing(span),
            |reading| span_record(reading.target, span),
        );
        let part = |pick: fn(&Reading) -> &Value| {
            first.map_or_else(|| Value::nothing(span), |reading| pick(reading).clone())
        };
        let other_sites = readings
            .map(|(index, reading)| {
                Value::record(
                    record! {
                        "reading" => Value::int(index as i64, span),
                        "target" => span_record(reading.target, span),
                        "site" => reading.site.clone(),
                        "inputs" => reading.inputs.clone(),
                    },
                    span,
                )
            })
            .collect();

        record! {
            "target" => target,
            "site" => part(|reading| &reading.site),
            "inputs" => part(|reading| &reading.inputs),
            "other_sites" => Value::list(other_sites, span),
            "sources" => Value::list(log.steps.iter().map(|step| step.value(span)).collect(), span),
            "dropped" => dropped.into_value(span),
            "dropped_total" => Value::int(log.dropped_total as i64, span),
        }
    }
}

impl Step {
    fn new(reading: usize, name: &str, role: Role) -> Self {
        Self {
            reading,
            name: if name.is_empty() { "nothing" } else { name }.to_owned(),
            role,
            outcome: Outcome::Declined,
            failed: None,
            note: None,
            candidates: 0,
            kept: 0,
            cached: None,
            duration: Duration::ZERO,
            handed: None,
            raw: None,
            faults: Vec::new(),
        }
    }

    /// The record `sources` holds for this step.
    fn value(&self, span: Span) -> Value {
        let nothing = || Value::nothing(span);
        let custom = self.raw.is_some() || self.handed.is_some();
        let (outcome, error) = match (&self.failed, &self.note) {
            (Some(error), _) => (
                Value::string("failed", span),
                Value::error(error.clone(), span),
            ),
            (None, Some(note)) => (self.outcome.into_value(span), Value::string(note, span)),
            (None, None) => (self.outcome.into_value(span), nothing()),
        };
        let handed = self.handed.filter(|_| custom).map(|wanted| {
            let names = [
                ("token", wanted.token),
                ("place", wanted.place),
                ("buffer", wanted.buffer),
            ];
            Value::list(
                names
                    .into_iter()
                    .filter(|(_, declared)| *declared)
                    .map(|(name, _)| Value::string(name, span))
                    .collect(),
                span,
            )
        });
        let faults = custom.then(|| {
            let faults = self.faults.iter().map(|fault| {
                Value::record(
                    record! {
                        "at" => fault.at.map_or_else(nothing, |at| Value::int(at as i64, span)),
                        "reason" => Value::string(&fault.reason, span),
                    },
                    span,
                )
            });
            Value::list(faults.collect(), span)
        });

        Value::record(
            record! {
                "reading" => Value::int(self.reading as i64, span),
                "name" => Value::string(&self.name, span),
                "role" => self.role.into_value(span),
                "outcome" => outcome,
                "error" => error,
                "candidates" => Value::int(self.candidates as i64, span),
                "kept" => Value::int(self.kept as i64, span),
                "cached" => Value::string(match self.cached {
                    Some(Lookup::Hit) => "hit",
                    Some(Lookup::Miss) => "miss",
                    None => "none",
                }, span),
                "duration" => Value::duration(
                    i64::try_from(self.duration.as_nanos()).unwrap_or(i64::MAX),
                    span,
                ),
                "handed" => handed.unwrap_or_else(nothing),
                "raw" => self.raw.clone().unwrap_or_else(nothing),
                "faults" => faults.unwrap_or_else(nothing),
            },
            span,
        )
    }
}

/// A declared completer, by the name of the command it runs or what it lists.
fn completer_value(completion: &Completion, working_set: &StateWorkingSet, span: Span) -> Value {
    match completion {
        Completion::Command(id) if id.get() < working_set.num_decls() => {
            Value::string(working_set.get_decl(*id).name(), span)
        }
        Completion::Command(_) => Value::nothing(span),
        other => other.to_value(working_set.permanent_state, span),
    }
}

/// What is completed at `site`, as `debug complete` reports it:
///
/// - `kind`: what the cursor is at, as a completer reads it in `place.kind`;
/// - `command`: the command whose argument it is, with its kind and the completer it declares
///   for the whole call;
/// - `parameter`: the argument it is, with the shape and completer the command declares for it;
/// - `input`: what is piped into the command.
fn site_value(site: &CompletionSite, ctx: &Context) -> Value {
    let span = ctx.span;
    let working_set = ctx.working_set;
    let nothing = || Value::nothing(span);
    let text = |text: &str| Value::string(text, span);
    let completer = |completion: Option<&Completion>| {
        completion.map_or_else(nothing, |completion| {
            completer_value(completion, working_set, span)
        })
    };
    let argument = |name: Option<&str>,
                    index: Option<usize>,
                    flag: Option<&str>,
                    shape: Option<&SyntaxShape>,
                    completion: Option<&Completion>| {
        Value::record(
            record! {
                "name" => name.map_or_else(nothing, text),
                "index" => index.map_or_else(nothing, |index| Value::int(index as i64, span)),
                "flag" => flag.map_or_else(nothing, text),
                "shape" => shape.map_or_else(nothing, |shape| text(&shape.to_string())),
                "completer" => completer(completion),
            },
            span,
        )
    };
    let command = |name: &str, kind: &str, completer: Value| {
        Value::record(
            record! { "name" => text(name), "kind" => text(kind), "completer" => completer },
            span,
        )
    };

    let (command, parameter) = match (&site.kind, site.kind.call_site()) {
        (SiteKind::ExternalArg { call, index }, _) => {
            let head = match &call.expr {
                Expr::ExternalCall(head, _) => working_set.get_span_contents(head.span),
                _ => &[],
            };
            let config = ctx.stack.get_config(working_set.permanent_state);
            let has_completer = config.completions.external.completer.is_some();
            (
                command(
                    &String::from_utf8_lossy(head),
                    "external",
                    if has_completer {
                        text("external completer")
                    } else {
                        nothing()
                    },
                ),
                argument(None, Some(*index), None, None, None),
            )
        }
        (kind, Some(call_site)) => {
            let decl = working_set.get_decl(call_site.call.decl_id);
            let signature = decl.signature();
            let completes = match signature.complete {
                Some(CommandWideCompleter::Command(id)) => {
                    completer(Some(&Completion::Command(id)))
                }
                Some(CommandWideCompleter::External) => text("external completer"),
                None => nothing(),
            };
            let parameter = match kind {
                SiteKind::Positional { sig_positional, .. } => {
                    let positional = signature.get_positional(*sig_positional);
                    argument(
                        positional.map(|positional| positional.name.as_str()),
                        Some(*sig_positional),
                        None,
                        positional.map(|positional| &positional.shape),
                        positional.and_then(|positional| positional.completion.as_ref()),
                    )
                }
                SiteKind::FlagValue { flag, .. } => {
                    let declared = find_flag(&signature, *flag);
                    let declared = declared.as_ref();
                    argument(
                        None,
                        None,
                        Some(flag.name()),
                        declared.and_then(|declared| declared.arg.as_ref()),
                        declared.and_then(|declared| declared.completion.as_ref()),
                    )
                }
                _ => nothing(),
            };
            let kind = decl.command_type().to_string();
            (command(decl.name(), &kind, completes), parameter)
        }
        _ => (nothing(), nothing()),
    };

    Value::record(
        record! {
            "kind" => text(site.kind.resolved().kind()),
            "command" => command,
            "parameter" => parameter,
            "input" => input_record(ctx.piped, span),
        },
        span,
    )
}
