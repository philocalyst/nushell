use crate::completions::{
    ArgValueCompletion, Candidates, Context,
    attribute_completions::{attributables, attributes},
    cell_path_completions::{cell_path, columns},
    command_completions::{CommandScope, commands},
    completion_common::paths,
    custom_completions::UserCompletion,
    data::operands,
    dotnu_completions::{module_files, modules},
    env_var_completions::env_vars,
    exportable_completions::exports,
    flag_completions::flags,
    operator_completions::operators,
    rank::Score,
    static_completions::list,
    system,
    trace::{Outcome, Role},
    variable_completions::variables,
};
use nu_color_config::NuStyle;
use nu_protocol::{
    DeclId, DynamicSuggestion, IntoValue, ModuleId, Record, Span, SuggestionKind, Value,
    ast::{Comparison, Expression, FullCellPath, PathMember},
    engine::StateWorkingSet,
    record,
};
use nu_utils::{NuCow, time::Instant};
use reedline::Suggestion;
use std::{borrow::Cow, sync::Arc};

/// Where candidates come from, with what the source needs to know about the site.
pub(crate) enum Source<'a> {
    /// Nothing: a slot with no completer to ask answers empty.
    Nothing,
    /// A completer of the user's, attached to one argument or run for an external call.
    Custom(UserCompletion),
    /// A completer of the user's that answers for the whole call, never narrowed against the
    /// token.
    CommandWide(UserCompletion),
    /// What a plugin offers for an argument.
    Dynamic(ArgValueCompletion<'a>),
    /// The commands of a scope. `quoted` quotes the internal ones whose name holds a space, and
    /// `parent` limits them to the subcommands of a command.
    Commands {
        scope: CommandScope,
        quoted: bool,
        parent: Option<&'a str>,
    },
    /// The files, or the directories alone.
    Paths { directories: bool },
    /// The modules in scope, and with `std_virtual_path` the virtual paths of the standard
    /// library. The module files come next.
    Modules { std_virtual_path: bool },
    /// The `.nu` files and the directories that may hold them.
    ModuleFiles { std_virtual_path: bool },
    /// The variables in scope.
    Variables,
    /// The environment variables.
    EnvVars,
    /// The running processes, by id; their names are matched too.
    Processes,
    /// The `attr` commands, after an `@`.
    Attributes,
    /// The definitions an attribute may precede.
    Attributables,
    /// The flags of a command.
    Flags(DeclId),
    /// The operators that may follow an expression.
    Operators(&'a Expression),
    /// Suggestions already made, narrowed against the token.
    Offered(Vec<SemanticSuggestion>),
    /// The spans of time a date is worked out with.
    Durations,
    /// The members of a cell path, which the cursor (absolute) is in.
    CellPath {
        path: &'a FullCellPath,
        cursor: usize,
    },
    /// The columns of the rows the context names (`Context::rows`), after the members of the
    /// token's cell path that come before the one typed.
    Columns,
    /// The values the column at `members` holds, to compare with `operator`: read from the data
    /// of the pipeline before the cursor.
    Operand {
        members: Vec<PathMember>,
        operator: Comparison,
    },
    /// The values a signature lists for an argument.
    List(NuCow<&'static [&'static str], Vec<String>>),
    /// What a module exports. `module_set` holds the module when it was parsed apart from the
    /// line, and `at` the span and typed text of the list item that is completed, if the token
    /// is not.
    Exports {
        module: ModuleId,
        module_set: Option<Box<StateWorkingSet<'a>>>,
        at: Option<(Span, &'a [u8])>,
    },
}

/// What the environment variables report as their source.
pub(crate) const ENV_VAR_SOURCE: &str = "environment variable";

impl Source<'_> {
    /// What suggestions from this source report as their source.
    fn label(&self) -> Cow<'static, str> {
        Cow::Borrowed(match self {
            Self::Nothing => "",
            Self::Custom(user) | Self::CommandWide(user) => return user.source.clone(),
            Self::Dynamic(_) => "dynamic",
            Self::Commands { .. } => "command",
            Self::Paths { directories: true } => "directory",
            Self::Paths { directories: false } => "file",
            Self::Modules { .. } | Self::ModuleFiles { .. } => "module",
            Self::Variables => "variable",
            Self::EnvVars => ENV_VAR_SOURCE,
            Self::Processes => "process",
            Self::Attributes | Self::Attributables => "attribute",
            Self::Flags(_) => "flag",
            Self::Operators(_) => "operator",
            Self::Offered(_) => "offered",
            Self::Durations => "duration",
            Self::CellPath { .. } => "cell path",
            Self::Columns => "column",
            Self::Operand { .. } => "value",
            Self::List(_) => "list",
            Self::Exports { .. } => "module export",
        })
    }
}

impl Source<'_> {
    /// Whether this is an `@interactive` completer, which asks the user in the terminal.
    fn needs_terminal(&self) -> bool {
        matches!(self, Self::Custom(user) | Self::CommandWide(user) if user.is_interactive())
    }
}

/// The candidates `source` offers for the token `ctx` describes; `None` where it declines and
/// the next source answers.
fn candidates(source: &Source, ctx: &Context) -> Option<Candidates> {
    Some(match source {
        Source::Nothing => Candidates::default(),
        Source::Custom(user) => return user.candidates(ctx).map(|found| user.or_files(found, ctx)),
        Source::CommandWide(user) => {
            // Answers for the whole call, never narrowed against the token prefix.
            let found = user.candidates(&Context {
                prefix: b"",
                ..*ctx
            });
            return found.map(|found| user.or_files(found, ctx));
        }
        Source::Dynamic(slot) => return slot.dynamic(ctx),
        Source::Commands {
            scope,
            quoted,
            parent,
        } => commands(*scope, *quoted, *parent, ctx),
        Source::Paths { directories } => paths(ctx, *directories),
        Source::Modules { std_virtual_path } => modules(*std_virtual_path, ctx),
        Source::ModuleFiles { std_virtual_path } => module_files(*std_virtual_path, ctx),
        Source::Variables => variables(ctx),
        Source::EnvVars => env_vars(ctx),
        Source::Processes => system::processes(ctx),
        Source::Attributes => attributes(ctx),
        Source::Attributables => attributables(ctx),
        Source::Flags(decl_id) => flags(*decl_id, ctx),
        Source::Operators(lhs) => operators(lhs, ctx),
        Source::Offered(items) => {
            let mut candidates: Candidates = Candidates {
                ordered: true,
                typos: false,
                ..Candidates::default()
            };
            let span = to_reedline_span(ctx.span, ctx.offset);
            for item in items.iter() {
                let mut item: SemanticSuggestion = item.clone();
                item.suggestion.span = span;
                let name = (item.suggestion.display_override.clone())
                    .unwrap_or_else(|| item.suggestion.value.clone());
                candidates.push_as(name, item);
            }
            candidates
        }
        Source::Durations => super::data::durations(ctx),
        Source::CellPath { path, cursor } => cell_path(path, *cursor, ctx),
        Source::Columns => columns(ctx),
        Source::Operand { members, operator } => operands(members, *operator, ctx),
        Source::List(values) => list(values, ctx),
        Source::Exports {
            module,
            module_set,
            at,
        } => {
            let item = at.map(|(span, prefix)| Context {
                span,
                prefix,
                ..*ctx
            });
            exports(*module, module_set.as_deref(), item.as_ref().unwrap_or(ctx))
        }
    })
}

/// Ask `sources` in turn, ranking what each offers: the suggestions of every source asked, up
/// to the first that answers.
pub(crate) fn ask<'a>(
    sources: impl IntoIterator<Item = Source<'a>>,
    ctx: &Context,
) -> Vec<SemanticSuggestion> {
    ask_as(Role::Primary, sources, ctx)
}

/// [`ask`], for sources that are asked only because the ones before them, in another call,
/// answered nothing: a trace reports them as fallbacks.
pub(crate) fn ask_fallback<'a>(
    sources: impl IntoIterator<Item = Source<'a>>,
    ctx: &Context,
) -> Vec<SemanticSuggestion> {
    ask_as(Role::Fallback, sources, ctx)
}

/// [`ask`], with the first source in the role `first`; those after it are fallbacks.
fn ask_as<'a>(
    first: Role,
    sources: impl IntoIterator<Item = Source<'a>>,
    ctx: &Context,
) -> Vec<SemanticSuggestion> {
    let mut found = Vec::new();
    let mut sources = sources.into_iter();
    let mut role = first;
    for source in sources.by_ref() {
        // A traced completion times each source, and does not run a completer that needs the
        // terminal: that is the line editor's to run.
        let traced = ctx.trace.map(|trace| {
            trace.open(&source.label(), role);
            Instant::now()
        });
        role = Role::Fallback;
        let close = |outcome, candidates, kept| {
            if let (Some(trace), Some(started)) = (ctx.trace, traced) {
                trace.close(outcome, candidates, kept, started.elapsed());
            }
        };

        if traced.is_some() && source.needs_terminal() {
            close(Outcome::NeedsTerminal, 0, 0);
            break;
        }
        let Some(offered) = candidates(&source, ctx) else {
            close(Outcome::Declined, 0, 0);
            continue;
        };
        let (partial, offered_count) = (offered.partial, offered.items.len());
        let label = source.label();
        let before = found.len();
        let suggestions = offered.suggestions(ctx).into_iter().map(|mut suggestion| {
            suggestion.source.get_or_insert_with(|| label.clone());
            suggestion
        });
        // What a source after a partial one offers again, such as a file the completer listed
        // before asking for files too, is offered once.
        for suggestion in suggestions {
            let offered = found[..before].iter().any(|earlier: &SemanticSuggestion| {
                earlier.suggestion.value == suggestion.suggestion.value
                    && earlier.suggestion.span == suggestion.suggestion.span
            });
            if !offered {
                found.push(suggestion);
            }
        }
        close(Outcome::Answered, offered_count, found.len() - before);
        if !partial {
            break;
        }
    }
    if let Some(trace) = ctx.trace {
        for source in sources {
            trace.skipped(&source.label(), Role::Fallback);
        }
    }
    found
}

/// An engine [`Span`] in reedline coordinates: subtract `offset`, saturating so spans
/// before it can't underflow into an index that would panic (`is_char_boundary`); callers
/// may pass untrusted spans.
pub(crate) fn to_reedline_span(span: Span, offset: usize) -> reedline::Span {
    reedline::Span::new(
        span.start.saturating_sub(offset),
        span.end.saturating_sub(offset),
    )
}

/// How far a suggestion can be trusted to describe things as they are now, as `debug complete`
/// reports it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, IntoValue)]
#[nu_value(rename_all = "snake_case")]
pub enum Origin {
    /// Read from the shell as it is: its commands, files and variables, or a completer that was
    /// just run.
    #[default]
    Live,
    /// Remembered by the completion cache from an earlier request, such as the answer of a custom
    /// completer, so what it listed may have changed since.
    Cached,
    /// Derived from the values of the data a pipeline gave, such as those a column holds, offered
    /// after `==`. Another row, or another run, may hold others.
    Derived,
}

/// What the completion panel and its previews know of one suggestion beyond what reedline's
/// `Suggestion` holds.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Detail {
    /// What the suggestion is, for the panel's colours and titles and for what it previews.
    pub(crate) kind: Option<SuggestionKind>,
    /// Which source offered it, such as `environment variable`.
    pub(crate) source: Option<Cow<'static, str>>,
    /// The fields a custom completer's record carried beyond the ones the engine reads.
    pub(crate) fields: Option<Record>,
    /// What a custom completer said to preview: a value, or a closure to call once the
    /// suggestion is selected.
    pub(crate) preview: Option<Value>,
    /// What a custom completer said to show once the preview is unfolded: a value, or a
    /// closure called along with the preview's.
    pub(crate) help: Option<Value>,
    /// Where in a command it sits, for the help that goes with its preview.
    pub(crate) help_at: Option<Arc<HelpAt>>,
    /// The pipeline a command would take its input from, as text: its preview may show the
    /// command at work on it.
    pub(crate) input: Option<Arc<str>>,
    /// A command's usage, as the status line writes it, for the hint line while it is
    /// selected.
    pub(crate) usage: Option<String>,
    /// What was typed that the suggestion corrects; see [`SemanticSuggestion::fixes`].
    pub(crate) fixes: Option<String>,
}

/// Where in a command a suggestion is offered, so that its preview can fold under it what the
/// command's help says of that place: the examples that use the flag, or the parameter.
#[derive(Debug, PartialEq)]
pub struct HelpAt {
    pub command: HelpFor,
    pub slot: Slot,
}

/// The command a [`HelpAt`] is in.
#[derive(Debug, PartialEq)]
pub enum HelpFor {
    /// One the shell has a declaration of, such as an `extern`.
    Decl(DeclId),
    /// A program with no declaration, which only a tldr page may say something of.
    External(String),
}

/// What a [`HelpAt`] is of the command.
#[derive(Debug, PartialEq)]
pub enum Slot {
    /// The name of a flag, or anything else that is offered where one is typed.
    Flag,
    /// The value of the flag with this name.
    FlagValue(String),
    /// The positional with this index in the signature.
    Positional(usize),
    /// An argument of a program with no signature to place it in.
    Argument,
}

/// The [`Detail`] of each suggestion of an answer, in the answer's order. It rides with the
/// answer as [`Completions::data`](reedline::Completions::data), for the completion panel to
/// tag and draw as extra columns, and for the previews.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Details(pub(crate) Vec<Detail>);

impl Details {
    /// The details of `suggestions`, or `None` when there are none.
    pub(crate) fn of(suggestions: &[SemanticSuggestion]) -> Option<Self> {
        (!suggestions.is_empty()).then(|| {
            Self(
                suggestions
                    .iter()
                    .map(|suggestion| Detail {
                        kind: suggestion.kind.clone(),
                        source: suggestion.source.clone(),
                        fields: suggestion.fields.clone(),
                        preview: suggestion.preview.clone(),
                        help: suggestion.help.clone(),
                        help_at: suggestion.help_at.clone(),
                        input: suggestion.input.clone(),
                        usage: None,
                        fixes: suggestion.fixes.clone(),
                    })
                    .collect(),
            )
        })
    }

    /// The names of the extra fields, each once, in the order they are first seen.
    pub(crate) fn columns(&self) -> Vec<&str> {
        let mut columns: Vec<&str> = Vec::new();
        let records = self.0.iter().filter_map(|detail| detail.fields.as_ref());
        for column in records.flat_map(Record::columns) {
            if !columns.contains(&column.as_str()) {
                columns.push(column);
            }
        }
        columns
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SemanticSuggestion {
    pub suggestion: Suggestion,
    pub kind: Option<SuggestionKind>,
    /// Why the fuzzy ranking put this suggestion where it did. `None` when the source did not
    /// rank it, or the algorithm does not score (`prefix`).
    pub score: Option<Score>,
    /// Which source produced the suggestion, such as `command`, `file` or `custom: <name>`.
    pub source: Option<Cow<'static, str>>,
    /// The fields a custom completer's record carried beyond the ones the engine reads, such as
    /// `{value: main, updated: 2hr}`'s `updated`, in the completer's order.
    pub fields: Option<Record>,
    /// What a custom completer's record said to preview, such as a value or a closure.
    pub preview: Option<Value>,
    /// What a custom completer's record said to show once the preview is unfolded.
    pub help: Option<Value>,
    /// Where in a command the suggestion is offered; set for the arguments of a call.
    pub help_at: Option<Arc<HelpAt>>,
    /// The pipeline a command would take its input from, as text, when it can be read.
    pub input: Option<Arc<str>>,
    /// Whether a completer said itself whether a space follows the suggestion, with
    /// `append_whitespace` or a value ending in one. A space the shell adds after a finished
    /// argument by default is the line editor's, which the language server leaves out.
    pub chose_spacing: bool,
    /// What was typed that the suggestion corrects, when it is offered as what was meant in
    /// place of a slip: a misspelt name, or a word run on into the next. The panel draws it as
    /// the correction it is.
    pub fixes: Option<String>,
    /// How far the suggestion can be trusted to describe things as they are now.
    pub origin: Origin,
}

impl SemanticSuggestion {
    pub fn from_dynamic_suggestion(
        suggestion: DynamicSuggestion,
        span: reedline::Span,
        style: Option<nu_ansi_term::Style>,
    ) -> Self {
        SemanticSuggestion {
            suggestion: Suggestion {
                value: suggestion.value,
                display_override: suggestion.display_override,
                description: suggestion.description,
                extra: suggestion.extra,
                append_whitespace: suggestion.append_whitespace,
                match_indices: suggestion.match_indices,
                style,
                span,
                cursor: None,
            },
            kind: suggestion.kind,
            ..Default::default()
        }
    }
}

impl IntoValue for SemanticSuggestion {
    fn into_value(self, span: Span) -> Value {
        let mut record = Record::new();
        record.insert("value", Value::string(self.suggestion.value, span));

        record.insert("span", span_record(self.suggestion.span, span));

        if let Some(display) = self.suggestion.display_override {
            record.insert("display_override", Value::string(display, span));
        }

        if let Some(style) = self.suggestion.style.map(NuStyle::from) {
            record.insert("style", style.into_value(span));
        }

        if let Some(description) = self.suggestion.description {
            record.insert("description", description.into_value(span));
        }

        if let Some(extra) = self.suggestion.extra {
            record.insert("extra", extra.into_value(span));
        }

        // Omit default fields to keep the common output compact.
        if self.suggestion.append_whitespace {
            record.insert("append_whitespace", Value::bool(true, span));
        }

        if let Some(match_indices) = self
            .suggestion
            .match_indices
            .filter(|indices| !indices.is_empty())
        {
            record.insert(
                "match_indices",
                Value::list(
                    match_indices
                        .into_iter()
                        .map(|index| Value::int(index as i64, span))
                        .collect(),
                    span,
                ),
            );
        }

        if let Some(kind) = self.kind {
            let (kind_str, ty) = kind_name(&kind);
            record.insert("kind", kind_str.into_value(span));

            // Always a column: kinds without a type report `null`.
            record.insert(
                "type",
                ty.map_or_else(|| Value::nothing(span), |ty| ty.into_value(span)),
            );
        }

        Value::record(record, span)
    }
}

/// A kind's name, as a completer spells it, and the type it carries, if any.
pub(crate) fn kind_name(kind: &SuggestionKind) -> (&'static str, Option<String>) {
    match kind {
        SuggestionKind::Command(ty, _) => ("command", Some(ty.to_string())),
        SuggestionKind::Value(ty) => ("value", Some(ty.to_string())),
        SuggestionKind::CellPath => ("cell-path", None),
        SuggestionKind::Directory => ("directory", None),
        SuggestionKind::File => ("file", None),
        SuggestionKind::Flag => ("flag", None),
        SuggestionKind::Module => ("module", None),
        SuggestionKind::Operator => ("operator", None),
        SuggestionKind::Variable => ("variable", None),
    }
}

impl SemanticSuggestion {
    /// The record `commandline complete --explain` reports: what is offered, what of the line it
    /// replaces and what follows it once taken, which source offered it, and how it ranked.
    /// `score` and `why` are `null` for a suggestion that was not ranked, such as the unfiltered
    /// list of a custom completer; `why` otherwise holds the terms of the [`Score`] that are not
    /// zero.
    pub fn explain(&self, span: Span) -> Value {
        let (score, why) = match self.score {
            Some(score) => {
                let terms = [
                    ("matched", score.matched),
                    ("prefix", score.prefix),
                    ("exact", score.exact),
                    ("basename", score.basename),
                    ("hidden", score.hidden),
                    ("typo", score.typo),
                    ("used", score.used),
                    ("git", score.git),
                    ("fits", score.fits),
                ];
                let why = terms
                    .into_iter()
                    .filter(|(_, points)| *points != 0)
                    .map(|(term, points)| (term.to_owned(), Value::int(points.into(), span)))
                    .collect();
                (
                    Value::int(score.total().into(), span),
                    Value::record(why, span),
                )
            }
            None => (Value::nothing(span), Value::nothing(span)),
        };
        let suggestion = &self.suggestion;
        Value::record(
            record! {
                "value" => Value::string(&suggestion.value, span),
                "display" => Value::string(suggestion.display_value(), span),
                "description" => suggestion.description.as_deref().into_value(span),
                "span" => span_record(suggestion.span, span),
                "append_whitespace" => Value::bool(suggestion.append_whitespace, span),
                "cursor" => suggestion.cursor.and_then(|at| i64::try_from(at).ok()).into_value(span),
                "kind" => self.kind.as_ref().map(|kind| kind_name(kind).0).into_value(span),
                "score" => score,
                "why" => why,
                "source" => self.source.as_deref().into_value(span),
                "origin" => self.origin.into_value(span),
            },
            span,
        )
    }
}

/// A reedline span as a `{start, end}` record of byte offsets into the line.
pub(crate) fn span_record(range: reedline::Span, span: Span) -> Value {
    Value::record(
        record! {
            "start" => Value::int(range.start as i64, span),
            "end" => Value::int(range.end as i64, span),
        },
        span,
    )
}

impl From<Suggestion> for SemanticSuggestion {
    fn from(suggestion: Suggestion) -> Self {
        Self {
            suggestion,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completions::CompletionEngine;
    use nu_protocol::engine::{EngineState, Stack};

    /// The values `f` offers for `typed`, over an engine that knows nothing.
    fn offered(typed: &str, f: impl FnOnce(&Context) -> Vec<SemanticSuggestion>) -> Vec<String> {
        let engine_state = EngineState::new();
        let engine = CompletionEngine::new(&engine_state, &Stack::new());
        let working_set = StateWorkingSet::new(&engine_state);
        let context = Context {
            prefix: typed.as_bytes(),
            ..engine.context(&working_set, typed, 0)
        };
        f(&context)
            .into_iter()
            .map(|found| found.suggestion.value)
            .collect()
    }

    fn named(value: &str) -> SemanticSuggestion {
        Suggestion {
            value: value.to_owned(),
            ..Suggestion::default()
        }
        .into()
    }

    #[test]
    fn a_candidate_is_matched_by_its_text_not_by_its_value() {
        let found = offered("sr", |ctx| {
            let mut candidates = Candidates::default();
            candidates.push_as("src", named("'src dir'"));
            candidates.push_as("lib", named("'sr lib'"));
            candidates.suggestions(ctx)
        });
        assert_eq!(found, ["'src dir'"]);
    }

    #[test]
    fn candidates_that_are_kept_are_neither_narrowed_nor_ordered() {
        let found = offered("a", |ctx| {
            let found = Candidates::kept(vec![named("b"), named("a")]).suggestions(ctx);
            assert!(found.iter().all(|found| found.score.is_none()));
            found
        });
        assert_eq!(found, ["b", "a"]);
    }

    #[test]
    fn the_needle_replaces_the_token() {
        let found = offered("ignored", |ctx| {
            let mut candidates = Candidates {
                needle: Some("fo".into()),
                ..Candidates::default()
            };
            candidates.push(named("foo"));
            candidates.push(named("bar"));
            candidates.suggestions(ctx)
        });
        assert_eq!(found, ["foo"]);
    }

    #[test]
    fn sources_are_asked_until_one_answers() {
        let std = || Source::Modules {
            std_virtual_path: true,
        };
        // A partial source leaves the slot open for the next.
        assert_eq!(
            offered("", |ctx| ask([std(), Source::Nothing, std()], ctx)),
            ["std", "std-rfc"]
        );
        // An answer, even an empty one, ends the chain.
        assert!(offered("", |ctx| ask([Source::Nothing, std()], ctx)).is_empty());
    }
}
