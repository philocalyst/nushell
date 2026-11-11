use super::Narrowing;
use crate::completions::{
    Candidates, CompletionOptions, Context, Fault, MatchAlgorithm, SemanticSuggestion,
    delegation::{self, Sources},
    quote::Quote,
    taking, to_reedline_span,
    usage::Used,
};
use nu_color_config::{color_record_to_nustyle, lookup_ansi_color_style};
use nu_protocol::{
    FromValue, Record, ShellError, SuggestionKind, Type, Value,
    engine::{Closure, CommandType},
    levenshtein_distance,
};
use nu_utils::strip_ansi_string_unlikely;
use reedline::Suggestion;
use std::{
    cell::{Cell, RefCell},
    sync::Arc,
};

thread_local! {
    /// What [`report`] collects while a traced answer is read; see [`capturing`].
    static CAPTURED: RefCell<Option<Vec<Fault>>> = const { RefCell::new(None) };
    /// The position in the returned list of the item being read.
    static ITEM: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Log a non-fatal completer error without disrupting line editing.
pub(crate) fn report(message: impl AsRef<str>) {
    let message = message.as_ref();
    log::error!("nu::shell::completion: {message}");
    CAPTURED.with_borrow_mut(|captured| {
        if let Some(faults) = captured {
            faults.push(Fault {
                at: ITEM.get(),
                reason: message.to_owned(),
            });
        }
    });
}

/// Run `f`, which reads a completer's answer, and give back what it [`report`]ed, for a trace
/// to show as the faults of the answer.
pub(crate) fn capturing<T>(f: impl FnOnce() -> T) -> (T, Vec<Fault>) {
    let outer = CAPTURED.replace(Some(Vec::new()));
    let result = f();
    (result, CAPTURED.replace(outer).unwrap_or_default())
}

/// Convert a completer result field, logging conversion failures.
fn read_part<T: FromValue>(value: Value, what: &str) -> Option<T> {
    T::from_value(value)
        .map_err(|err| report(format!("a completer's {what} is not usable: {err}")))
        .ok()
}

/// A string field read leniently: anything that coerces to a string becomes one, so
/// `{description: 5}` reads as `"5"` rather than costing the suggestion it describes.
struct Text(String);

impl FromValue for Text {
    fn from_value(value: Value) -> Result<Self, ShellError> {
        value.coerce_into_string().map(Self)
    }

    fn expected_type() -> Type {
        Type::String
    }
}

impl From<Text> for String {
    fn from(text: Text) -> Self {
        text.0
    }
}

/// Warn on unknown keys in completer output.
fn lint_keys(value: &Value, what: &str, keys: &[&str]) {
    let Ok(record) = value.as_record() else {
        return;
    };

    for unknown in record
        .columns()
        .filter(|column| !keys.contains(&column.as_str()))
    {
        report(format!(
            "a {what} has no `{unknown}` field, so it is ignored; expected one of {}",
            keys.join(", ")
        ));
    }
}

/// Warn on keys of a suggestion that look like a misspelled known key (`descripton`). Any other
/// key is a field of the completer's own, kept as an extra column.
fn lint_misspelled_keys(value: &Value, what: &str, keys: &[&str]) {
    let Ok(record) = value.as_record() else {
        return;
    };

    for unknown in record
        .columns()
        .filter(|column| !keys.contains(&column.as_str()))
    {
        // One slip in a short name, two in a long one: `style` is not `stale` by accident,
        // but nothing else is as close as this to a name the engine reads.
        let allowed = if unknown.len() > 5 { 2 } else { 1 };
        if let Some(known) = keys
            .iter()
            .find(|known| levenshtein_distance(unknown, known) <= allowed)
        {
            report(format!(
                "a {what} has a `{unknown}` field, which looks like a misspelled `{known}`; it \
                 is kept as an extra column"
            ));
        }
    }
}

/// Record returned by completer with exact accepted keys.
trait ReturnedRecord: FromValue {
    /// What to call it in a message.
    const LABEL: &'static str;
    /// Every key accepted, whether read into a field or merely tolerated.
    const KEYS: &'static [&'static str];

    /// Read one; unknown keys linted and ignored.
    fn read(value: Value) -> Option<Self> {
        lint_keys(&value, Self::LABEL, Self::KEYS);
        read_part(value, Self::LABEL)
    }
}

/// Declare a record a completer returns: the struct [`FromValue`] reads, and the exact set
/// of keys it accepts. `accepting` names keys tolerated but not read.
macro_rules! returned_record {
    (
        $(#[$doc:meta])*
        struct $name:ident is $label:literal $(, accepting [$($extra:literal),* $(,)?])? {
            $( $(#[$field_doc:meta])* $field:ident : $ty:ty ),* $(,)?
        }
    ) => {
        $(#[$doc])*
        #[derive(FromValue)]
        #[nu_value(type_name = $label)]
        struct $name {
            $( $(#[$field_doc])* $field: $ty, )*
        }

        impl ReturnedRecord for $name {
            const LABEL: &'static str = $label;
            const KEYS: &'static [&'static str] =
                &[$(stringify!($field),)* $($($extra),*)?];
        }
    };
}

returned_record! {
    /// The `options` a completer may return beside its completions. Every key is optional;
    /// one left out keeps the engine's configured setting.
    struct ReturnedOptions is "completion options" {
        /// Whether the engine narrows the list against the typed text. Defaults by completer
        /// kind; see [`Narrowing`].
        filter: Option<bool>,
        sort: Option<bool>,
        case_sensitive: Option<bool>,
        match_description: Option<bool>,
        completion_algorithm: Option<String>,
    }
}

returned_record! {
    /// A suggestion returned by a custom completer.
    struct ReturnedSuggestion is "suggestion", accepting ["type"] {
        /// Value to insert, coerced to a string.
        value: Value,
        display_override: Option<Text>,
        /// What carapace and fish name `display_override`.
        display: Option<Text>,
        description: Option<Text>,
        /// Optional suggestion kind.
        kind: Option<Value>,
        /// A colour name or colour record.
        style: Option<Value>,
        /// Read on its own for the same reason as `style`.
        span: Option<Value>,
        /// Extra columns beside the value, in description menus.
        extra: Option<Vec<Text>>,
        /// Whether to append a space after the value.
        append_whitespace: Option<bool>,
        /// Character indices matching the typed text.
        match_indices: Option<Vec<usize>>,
        /// What to show while the suggestion is selected: a value, drawn as a table, or a
        /// closure that returns one, called only once the suggestion is selected.
        preview: Option<Value>,
        /// What shows once the preview is unfolded: a value or a closure, as `preview` is.
        help: Option<Value>,
    }
}

/// Convert a custom suggestion kind, defaulting to the value's type.
fn suggestion_kind(kind: Option<Value>, value: &Value) -> SuggestionKind {
    let by_type = || SuggestionKind::Value(value.get_type());
    let Some(kind) = kind else { return by_type() };

    let Ok(name) = kind.as_str() else {
        report(format!(
            "a suggestion's `kind` must be a name like `directory` or `command`; got {}",
            kind.get_type()
        ));
        return by_type();
    };

    match name {
        "value" => by_type(),
        "command" => SuggestionKind::Command(CommandType::Custom, None),
        "cell-path" => SuggestionKind::CellPath,
        "directory" => SuggestionKind::Directory,
        "file" => SuggestionKind::File,
        "flag" => SuggestionKind::Flag,
        "module" => SuggestionKind::Module,
        "operator" => SuggestionKind::Operator,
        "variable" => SuggestionKind::Variable,
        other => {
            report(format!(
                "a suggestion's `kind` is `{other}`; expected one of value, command, \
                 cell-path, directory, file, flag, module, operator, variable"
            ));
            by_type()
        }
    }
}

returned_record! {
    /// A `span` a suggestion returned. Each end is optional and falls back to the span the
    /// engine would have replaced, so `{end: 20}` moves only the end (#tested by
    /// `custom_completions_override_span`).
    struct ReturnedSpan is "suggestion span" {
        start: Option<i64>,
        end: Option<i64>,
    }
}

returned_record! {
    /// The `delegate` a completer may return: complete the rest of the line as a command line
    /// of its own, the way `sudo`'s arguments are.
    struct ReturnedDelegate is "delegate" {
        /// Byte offset into the line the completer was given, where the delegated line starts.
        from: i64,
        /// `{|dir| ...}`: the names in `dir`, directories ending in `/`, standing in for the
        /// local filesystem in path completion.
        paths: Option<Closure>,
        /// `{|| ...}`: command names, standing in for `PATH` when the line's command is completed.
        commands: Option<Closure>,
        /// The quote (`'`, `"` or a backtick) of the string that `from` is inside of, as in
        /// `ssh host 'cat f`, where `from` is the byte after the quote. What the delegated line
        /// inserts is then written to stay within the string.
        inside: Option<String>,
        /// Whether `from` starts one path of the other machine (`scp host:dir/f`) and not a
        /// command line; `paths` lists its directories.
        path: Option<bool>,
    }
}

impl ReturnedDelegate {
    /// What the delegated line answers. `None` declines: `from` is negative, out of range or
    /// inside a character.
    fn resolve(self, ctx: &Context) -> Option<Vec<SemanticSuggestion>> {
        let from = usize::try_from(self.from)
            .map_err(|_| report(format!("a delegate's `from` ({}) is negative", self.from)))
            .ok()?;
        let inside = self.inside.and_then(|quote| {
            let read = Quote::opened_by(&quote).filter(|_| quote.chars().count() == 1);
            if read.is_none() {
                report(format!(
                    "a delegate's `inside` ({quote:?}) must be a single quote character"
                ));
            }
            read
        });
        let sources = Sources {
            paths: self.paths.map(Arc::new),
            commands: self.commands.map(Arc::new),
            inside,
        };

        let fetched = if self.path == Some(true) {
            delegation::complete_path(ctx, from, sources)
        } else {
            delegation::complete(ctx, from, sources)
        };
        if fetched.is_none() {
            report(format!(
                "a delegate's `from` ({from}) is not a position on the {}-byte line it was given",
                ctx.buffer.len()
            ));
        }
        fetched
    }
}

/// Keys accepted by the completion envelope.
const ENVELOPE_KEYS: [&str; 5] = ["completions", "options", "fallback", "delegate", "message"];

/// What a completer or menu source returned, before any options are applied. See the
/// [module docs](self) for the shapes accepted.
pub(crate) struct Returned {
    pub(crate) completions: Vec<Value>,
    options: Option<ReturnedOptions>,
    /// Whether to continue with the next source.
    fallback: bool,
    /// A request to complete part of the line as a line of its own. It answers on its own;
    /// `options` and `fallback` do not apply to it.
    delegate: Option<ReturnedDelegate>,
    /// Why there is nothing to suggest, which the status line shows when nothing is.
    message: Option<String>,
}

impl Returned {
    /// `None` where the source declined with `null`, letting the next one answer.
    pub(crate) fn read(value: Value) -> Option<Self> {
        let bare = |completions| {
            Some(Self {
                completions,
                options: None,
                fallback: false,
                delegate: None,
                message: None,
            })
        };

        // Records without envelope keys are treated as lone suggestions.
        let is_envelope = match &value {
            Value::Record { val, .. } => ENVELOPE_KEYS.iter().any(|key| val.contains(key)),
            _ => false,
        };

        if is_envelope {
            lint_keys(&value, "completion envelope", &ENVELOPE_KEYS);
            let mut record = value.into_record().unwrap_or_default();

            // A delegate that cannot be read is no answer, unless the record has completions.
            let delegate = record.remove("delegate").map(ReturnedDelegate::read);
            if matches!(delegate, Some(None)) && !record.contains("completions") {
                return None;
            }

            // Read each part independently.
            return Some(Self {
                completions: record
                    .remove("completions")
                    .and_then(|completions| read_part(completions, "`completions`"))
                    .unwrap_or_default(),
                options: record.remove("options").and_then(ReturnedOptions::read),
                fallback: record
                    .remove("fallback")
                    .and_then(|fallback| read_part(fallback, "`fallback`"))
                    .unwrap_or(false),
                delegate: delegate.flatten(),
                message: record
                    .remove("message")
                    .and_then(|message| read_part::<Option<Text>>(message, "`message`"))
                    .flatten()
                    .map(String::from),
            });
        }

        match value {
            Value::Nothing { .. } => None,
            Value::List { vals, .. } => bare(vals.into_owned()),
            // A list is many suggestions and anything else is one. A picker returns the item
            // it was given, not a list of one: `input list` and `fzf` both answer with a bare
            // string, and rejecting it would cost the completion the user just chose.
            one => bare(vec![one]),
        }
    }
}

/// What a completer returned, as candidates for `ctx`'s token; `None` where it declined with
/// `null`. `narrowing` says who filters them unless the completer's `options` say otherwise.
pub(crate) fn read_candidates(
    value: Value,
    ctx: &Context,
    narrowing: Narrowing,
) -> Option<Candidates> {
    let mut returned = Returned::read(value)?;
    if let Some(message) = returned.message.take() {
        ctx.notice.say(message);
    }

    // A delegate answers for the whole slot, as it was ranked. One that cannot be honoured
    // declines, unless the record also carries completions.
    if let Some(delegate) = returned.delegate.take() {
        match delegate.resolve(ctx) {
            Some(delegated) => return Some(Candidates::kept(delegated)),
            None if returned.completions.is_empty() => return None,
            None => {}
        }
    }

    let mut options = ctx.options.clone();
    let mut candidates = Candidates {
        used: Some(Used::Argument),
        filter: narrowing.filters_by_default(),
        partial: returned.fallback,
        ordered: true,
        ..Candidates::default()
    };
    if let Some(returned) = returned.options {
        read_options(returned, &mut options, &mut candidates);
    }
    candidates.options = Some(options);

    let token = to_reedline_span(ctx.span, ctx.offset);
    for suggestion in map_value_completions(
        returned.completions.into_iter(),
        token,
        SpanClamp::within(ctx.buffer),
    ) {
        let text = strip_ansi_string_unlikely(suggestion.suggestion.display_value().to_string());
        candidates.push_as(text, suggestion);
    }
    // Suggestions for a part of the word, as the host after `user@`, are matched against what
    // is typed of that part.
    if candidates.needle.is_none()
        && let Some(part) = part_typed(&candidates, token.start, &ctx.prefix_str())
    {
        candidates.needle = Some(part);
    }
    Some(candidates)
}

/// What is typed of the part of the word `typed`, which starts at `token_start`, that every
/// suggestion of `candidates` replaces, when they all start at one place inside it.
fn part_typed(candidates: &Candidates, token_start: usize, typed: &str) -> Option<String> {
    let mut starts = candidates
        .items
        .iter()
        .map(|(_, item)| item.suggestion.span.start);
    let start = starts.next()?;
    if start <= token_start || starts.any(|other| other != start) {
        return None;
    }
    typed.get(start - token_start..).map(str::to_owned)
}

/// Apply the `options` record a completer returned alongside its completions.
fn read_options(
    returned: ReturnedOptions,
    options: &mut CompletionOptions,
    candidates: &mut Candidates,
) {
    if let Some(filter) = returned.filter {
        candidates.filter = filter;
    }

    if let Some(sort) = returned.sort {
        candidates.sort = sort;
        // Asking for a sort asks for the alphabetical order where nothing is typed.
        candidates.ordered = !sort;
        if sort && !candidates.filter {
            log::warn!("Sorting won't happen because filtering is disabled.");
        }
    }

    if let Some(case_sensitive) = returned.case_sensitive {
        options.case_sensitive = case_sensitive;
    }

    if let Some(match_description) = returned.match_description {
        options.match_description = match_description;
    }

    if let Some(algorithm) = returned
        .completion_algorithm
        .and_then(|algorithm| MatchAlgorithm::try_from(algorithm).ok())
    {
        options.match_algorithm = algorithm;
    }
}

/// The colour a suggestion asked for: a name (`"green"`, `"bg_red"`), or a record of
/// `{fg, bg, attr}`.
fn style_of(style: Value) -> Option<nu_ansi_term::Style> {
    match style {
        Value::String { val, .. } => Some(lookup_ansi_color_style(&val)),
        Value::Record { .. } => Some(color_record_to_nustyle(&style)),
        other => {
            report(format!(
                "a suggestion's `style` must be a colour name or a record of {{fg, bg, attr}}; \
                 got {}",
                other.get_type()
            ));
            None
        }
    }
}

impl ReturnedSuggestion {
    /// Read one. Unlike the other records, it may carry fields of the completer's own: see
    /// [`Self::extra_fields`]. Only a key that looks like a misspelled known one is reported.
    fn read(value: Value) -> Option<Self> {
        lint_misspelled_keys(&value, Self::LABEL, Self::KEYS);
        read_part(value, Self::LABEL)
    }

    /// The fields of `value` that the engine does not read, in the completer's order, such as
    /// `updated` and `remote` of `{value: main, updated: 2hr, remote: origin}`. `None` when
    /// there are none.
    fn extra_fields(value: &Value) -> Option<Record> {
        let record = value.as_record().ok()?;
        let fields: Record = record
            .iter()
            .filter(|(column, _)| !Self::KEYS.contains(&column.as_str()))
            .map(|(column, field)| (column.clone(), field.clone()))
            .collect();
        (!fields.is_empty()).then_some(fields)
    }

    /// Resolve against the span a suggestion replaces when it names none, carrying `fields`
    /// along.
    fn into_semantic(
        self,
        fields: Option<Record>,
        default_span: reedline::Span,
        clamp: SpanClamp<'_>,
    ) -> SemanticSuggestion {
        let kind = suggestion_kind(self.kind, &self.value);
        let value = self
            .value
            .coerce_into_string()
            .map(strip_ansi_string_unlikely)
            .unwrap_or_default();
        let chose_spacing = self.append_whitespace.is_some() || value.ends_with(' ');
        let (value, append_whitespace) = spaced(value, self.append_whitespace, &kind);

        SemanticSuggestion {
            suggestion: Suggestion {
                value,
                display_override: self.display_override.or(self.display).map(Into::into),
                description: self.description.map(Into::into),
                style: self.style.and_then(style_of),
                span: self
                    .span
                    .and_then(ReturnedSpan::read)
                    .map_or(default_span, |span| span.resolve(default_span, clamp)),
                extra: self
                    .extra
                    .map(|extra| extra.into_iter().map(Into::into).collect()),
                append_whitespace,
                match_indices: self.match_indices,
                cursor: None,
            },
            kind: Some(kind),
            fields,
            // `null` says there is nothing to preview.
            preview: self.preview.filter(|preview| !preview.is_nothing()),
            help: self.help.filter(|help| !help.is_nothing()),
            chose_spacing,
            ..Default::default()
        }
    }
}

impl ReturnedSpan {
    /// Clamp so a returned span can't index out of bounds or mid-character (#5127), nor
    /// replace text the completer was never shown.
    fn resolve(self, default: reedline::Span, clamp: SpanClamp<'_>) -> reedline::Span {
        let offset = |value: Option<i64>, fallback: usize| match value {
            // A negative offset names no position on the line. Keeping the default is the
            // safe reading; clamping it to `0` would silently replace from the line's start.
            Some(value) if value < 0 => {
                report(format!("a suggestion's span offset ({value}) is negative"));
                fallback
            }
            Some(value) => clamp.apply(value as usize),
            None => fallback,
        };

        let start = offset(self.start, default.start);
        let end = offset(self.end, default.end);

        if start > end {
            report(format!(
                "a suggestion's span start ({start}) is greater than its end ({end})"
            ));
            return reedline::Span::new(end, end);
        }

        reedline::Span::new(start, end)
    }
}

/// `value` and whether a space follows it once taken: as the completer says, or, when it says
/// nothing, after a value that ends an argument. A value that ends in a space itself, as
/// carapace's do, has it taken off and given back as the space that follows.
fn spaced(mut value: String, said: Option<bool>, kind: &SuggestionKind) -> (String, bool) {
    let trimmed = value.trim_end_matches(' ').len();
    if trimmed < value.len() && trimmed > 0 && !value[..trimmed].ends_with('\\') {
        value.truncate(trimmed);
        return (value, true);
    }
    let spaced = said.unwrap_or_else(|| taking::ends_an_argument(&value, Some(kind)));
    (value, spaced)
}

/// Parse one `Value` of a completer's list into a SemanticSuggestion.
pub(crate) fn map_value_completions(
    list: impl Iterator<Item = Value>,
    default_span: reedline::Span,
    clamp: SpanClamp<'_>,
) -> Vec<SemanticSuggestion> {
    let found = list
        .enumerate()
        .filter_map(move |(index, value)| {
            ITEM.set(Some(index));
            if matches!(value, Value::Record { .. }) {
                let fields = ReturnedSuggestion::extra_fields(&value);
                return ReturnedSuggestion::read(value)
                    .map(|suggestion| suggestion.into_semantic(fields, default_span, clamp));
            }

            let ty = value.get_type();
            value
                .coerce_into_string()
                .map_err(|_| {
                    report(format!(
                        "a suggestion must be a string or a record; got {ty}"
                    ))
                })
                .ok()
                .map(|string| {
                    let kind = nu_protocol::SuggestionKind::Value(ty);
                    let string = strip_ansi_string_unlikely(string);
                    let chose_spacing = string.ends_with(' ');
                    let (value, append_whitespace) = spaced(string, None, &kind);
                    SemanticSuggestion {
                        suggestion: Suggestion {
                            value,
                            span: default_span,
                            append_whitespace,
                            ..Suggestion::default()
                        },
                        kind: Some(kind),
                        chose_spacing,
                        ..Default::default()
                    }
                })
        })
        .collect();
    ITEM.set(None);
    found
}

/// Clamps returned spans to the text the source saw, snapping offsets to its char boundaries.
#[derive(Clone, Copy)]
pub(crate) struct SpanClamp<'a> {
    seen: &'a str,
}

impl<'a> SpanClamp<'a> {
    /// Clamped to `text`.
    pub(crate) fn within(text: &'a str) -> Self {
        Self { seen: text }
    }

    pub(crate) fn apply(&self, value: usize) -> usize {
        self.seen.floor_char_boundary(value.min(self.seen.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nu_protocol::{Span as NuSpan, record};

    fn suggestions(value: Value) -> Vec<SemanticSuggestion> {
        map_value_completions(
            std::iter::once(value),
            reedline::Span::new(0, 0),
            SpanClamp::within(""),
        )
    }

    #[test]
    fn fields_beyond_the_known_ones_are_kept_in_order() {
        let found = suggestions(Value::test_record(record! {
            "value" => Value::test_string("main"),
            "updated" => Value::test_string("2hr"),
            "description" => Value::test_string("default branch"),
            "remote" => Value::test_string("origin"),
        }));
        let [main] = found.as_slice() else {
            panic!("one suggestion, got {found:?}");
        };
        assert_eq!(main.suggestion.value, "main");
        assert_eq!(
            main.suggestion.description.as_deref(),
            Some("default branch")
        );
        let fields = main.fields.as_ref().expect("the extra fields");
        assert_eq!(fields.columns().collect::<Vec<_>>(), ["updated", "remote"]);
        assert_eq!(fields.get("updated"), Some(&Value::test_string("2hr")));
    }

    #[test]
    fn a_suggestion_of_known_fields_has_none() {
        let found = suggestions(Value::test_record(record! {
            "value" => Value::test_string("main"),
            "type" => Value::test_string("string"),
            "extra" => Value::test_list(vec![Value::test_string("x")]),
        }));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fields, None);
        // A bare value has nothing beyond itself either.
        let bare = suggestions(Value::string("main", NuSpan::test_data()));
        assert_eq!(bare[0].fields, None);
    }

    #[test]
    fn a_preview_is_kept_for_the_panel_and_is_no_extra_column() {
        let found = suggestions(Value::test_record(record! {
            "value" => Value::test_string("main"),
            "preview" => Value::test_record(record! { "commits" => Value::test_int(3) }),
        }));
        assert_eq!(found[0].fields, None);
        assert!(matches!(found[0].preview, Some(Value::Record { .. })));

        let nothing = suggestions(Value::test_record(record! {
            "value" => Value::test_string("topic"),
            "preview" => Value::test_nothing(),
        }));
        assert_eq!(
            nothing[0].preview, None,
            "null says there is nothing to preview"
        );
    }

    #[test]
    fn an_extra_field_does_not_cost_the_suggestion() {
        let found = suggestions(Value::test_record(record! {
            "value" => Value::test_string("main"),
            // Looks like a misspelled `description`: reported, and still kept as a column.
            "descripton" => Value::test_string("typo"),
        }));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].suggestion.description, None);
        assert_eq!(found[0].fields.as_ref().map(|fields| fields.len()), Some(1));
    }
}
