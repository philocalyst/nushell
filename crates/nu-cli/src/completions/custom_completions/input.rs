//! The record a custom completer is handed, built from only the inputs it declares.
//!
//! - `token`: the token being completed, `{text, kind, span}`.
//! - `place`: where in the line that is.
//!   - `cursor`: the byte offset of the cursor.
//!   - `target`: the `{start, end}` range a suggestion replaces.
//!   - `kind`: what is completed (`command`, `flag-name`, `flag-value`, `positional`, ...).
//!   - `flag` (long name, or the short name of a short-only flag) or `index` (position), and
//!     `shape` (the declared type, as a string), where a value is completed.
//!   - `arguments`: where an argument of a call is completed, the call's other arguments known
//!     before it runs, keyed by the parameter they bind.
//!   - `command`: the words of the command the cursor is in.
//! - `buffer`: the line up to the cursor.

use crate::completions::{
    Context,
    completer::{ResolvedCursor, is_flag_text, touches},
    span_record, to_reedline_span,
};
use nu_parser::{TokenContents, lex};
use nu_protocol::{
    Record, Signature, Span, SyntaxShape, Value,
    ast::{Argument, Call, Expr, Expression, FlagRef},
    eval_const::eval_constant,
    record,
};

/// Return the declared shape for the argument under the cursor.
fn expected_shape(signature: &Signature, cursor: ResolvedCursor) -> Option<SyntaxShape> {
    match cursor {
        ResolvedCursor::Positional { index } => signature
            .get_positional(index)
            .map(|positional| positional.shape.clone()),
        // `flag` may be either the long name or a short-only flag.
        ResolvedCursor::FlagValue { flag } => signature
            .get_long_flag(flag)
            .or_else(|| {
                flag.chars()
                    .next()
                    .and_then(|c| signature.get_short_flag(c))
            })
            .and_then(|flag| flag.arg),
        _ => None,
    }
}

/// The arguments of `call` known before it runs, by the parameter they bind.
///
/// - A positional maps to its value, and the rest parameter to a list of its values.
/// - A flag maps by long name, or by short name when it has only a short one; a switch maps to
///   `true`, and a flag with a value to that value.
/// - A value is its constant evaluation where there is one, else its source text.
/// - The argument under the cursor is left out, as it is what is being completed.
fn call_arguments(
    context: &Context,
    call: &Call,
    signature: &Signature,
    cursor: ResolvedCursor,
) -> Record {
    let span = context.span;
    let at = context.offset + context.buffer.len();
    let fixed = signature.required_positional.len() + signature.optional_positional.len();
    let value_of = |expression: &Expression| {
        eval_constant(context.working_set, expression).unwrap_or_else(|_| {
            let text = context.working_set.get_span_contents(expression.span);
            Value::string(String::from_utf8_lossy(text), span)
        })
    };

    let (mut arguments, mut rest, mut index) = (Record::new(), Vec::new(), 0);
    for argument in &call.arguments {
        // A flag still awaiting its value is the one completed, though the cursor is past it.
        let completed = touches(argument.span(), at)
            || matches!(
                (argument, cursor),
                (Argument::Named((long, short, None)), ResolvedCursor::FlagValue { flag })
                    if FlagRef::from_named(long, short.as_ref()).name() == flag
            );
        match argument {
            Argument::Positional(expression) | Argument::Unknown(expression) => {
                index += 1;
                if completed {
                    continue;
                }
                match signature.get_positional(index - 1) {
                    Some(positional) if index <= fixed => {
                        arguments.insert(positional.name.clone(), value_of(expression));
                    }
                    _ => rest.push(value_of(expression)),
                }
            }
            Argument::Spread(expression) if !completed => match value_of(expression) {
                Value::List { vals, .. } => rest.extend(vals),
                other => rest.push(other),
            },
            Argument::Named((long, short, value)) if !completed => {
                let flag = FlagRef::from_named(long, short.as_ref()).name();
                arguments.insert(
                    flag,
                    value
                        .as_ref()
                        .map_or_else(|| Value::bool(true, span), value_of),
                );
            }
            _ => {}
        }
    }

    if let Some(parameter) = signature
        .rest_positional
        .as_ref()
        .filter(|_| !rest.is_empty())
    {
        arguments.insert(parameter.name.clone(), Value::list(rest, span));
    }
    arguments
}

fn place_value(context: &Context, cursor: Value, target: Value) -> Value {
    let span = context.span;
    let mut place = record! {
        "cursor" => cursor,
        "target" => target,
    };

    if let Some(level) = &context.level {
        place.extend(level.cursor.into_record(span));
        if let Some(Expr::Call(call)) = level.element.map(|element| &element.expr) {
            let signature = context.working_set.get_decl(call.decl_id).signature();
            if let Some(shape) = expected_shape(&signature, level.cursor) {
                place.insert("shape", Value::string(shape.to_string(), span));
            }
            // Present, though maybe empty, wherever an argument is completed, so that
            // `$place.arguments.x?` needs no guard.
            if matches!(
                level.cursor,
                ResolvedCursor::FlagName
                    | ResolvedCursor::FlagValue { .. }
                    | ResolvedCursor::Positional { .. }
            ) {
                let arguments = call_arguments(context, call, &signature, level.cursor);
                place.insert("arguments", Value::record(arguments, span));
            }
        }
    }

    // The command the cursor is in, so completers need not re-parse `buffer`.
    // One syntactic token per argument (no flatten), so `token`'s filtered walk
    // is never paid for when only `place` is asked for.
    place.insert("command", command_tokens(context));
    place.insert(
        "input",
        crate::completions::piped::input_record(context.piped, span),
    );

    Value::record(place, span)
}

/// Which of `token`, `place`, and `buffer` a completer asked for. The input record is
/// overloaded on this: each field is computed and handed over only when it is declared, so a
/// completer is never given -- and never pays for -- a field it did not ask for.
#[derive(Debug, Clone, Copy)]
pub struct DeclaredInputs {
    pub token: bool,
    pub place: bool,
    pub buffer: bool,
}

impl DeclaredInputs {
    /// Every field; what `commandline complete --input` reports for inspection.
    pub fn all() -> Self {
        Self {
            token: true,
            place: true,
            buffer: true,
        }
    }

    /// The recognized input fields a completer's signature declares as positionals.
    pub(crate) fn from_signature(signature: &Signature) -> Self {
        let declares = |name: &str| {
            signature
                .required_positional
                .iter()
                .chain(&signature.optional_positional)
                .any(|positional| positional.name == name)
        };
        Self {
            token: declares("token"),
            place: declares("place"),
            buffer: declares("buffer"),
        }
    }
}

/// Token text/span/kind for replacement range: the kind is the `head` of a command, a `flag`
/// or a `value`.
fn token_value(context: &Context) -> Value {
    let span = context.span;
    let replacing = to_reedline_span(span, context.offset);
    let kind = match context.level.map(|level| level.cursor) {
        Some(ResolvedCursor::Command) => "head",
        Some(ResolvedCursor::FlagName) => "flag",
        _ if is_flag_text(context.prefix) => "flag",
        _ => "value",
    };

    let (start, end) = (replacing.start, replacing.end.min(context.buffer.len()));
    let text = context.buffer.get(start..end).unwrap_or_default();

    Value::record(
        record! {
            "text" => Value::string(text, span),
            "kind" => Value::string(kind, span),
            "span" => span_record(reedline::Span::new(start, end), span),
        },
        span,
    )
}

/// Place: cursor, target, and resolved site.
fn place_of(context: &Context) -> Value {
    let span = context.span;
    let replacing = to_reedline_span(span, context.offset);
    place_value(
        context,
        Value::int(context.buffer.len() as i64, span),
        span_record(reedline::Span::new(replacing.start, replacing.end), span),
    )
}

/// Record for completer with only declared fields.
pub(crate) fn completer_input(context: &Context, wanted: DeclaredInputs) -> Value {
    let span = context.span;
    let mut record = Record::new();

    if wanted.token {
        record.insert("token", token_value(context));
    }
    if wanted.place {
        record.insert("place", place_of(context));
    }
    if wanted.buffer {
        record.insert("buffer", Value::string(context.buffer, span));
    }

    Value::record(record, span)
}

/// our legacy compat shim!
pub(crate) fn legacy_context(context: &Context) -> Value {
    let cursor = context.offset + context.buffer.len();
    let start = (context.level)
        .and_then(|level| level.element)
        .map(|element| element.span.start)
        .unwrap_or(cursor)
        .clamp(context.offset, cursor);
    let text = String::from_utf8_lossy(
        context
            .working_set
            .get_span_contents(Span::new(start, cursor)),
    )
    .into_owned();
    Value::string(text, context.span)
}

/// Buffer-relative cursor, i.e. the old second positional (`pos`/`position`).
pub(crate) fn legacy_pos(context: &Context) -> Value {
    Value::int(context.buffer.len() as i64, context.span)
}

/// Tokens of the command the cursor is in, plus `""` for a trailing empty slot.
///
/// Backs `place.command`; also the legacy `spans` value. One shell word per
/// argument (`foo {|x| $x } fil` is `[foo, "{|x| $x }", fil]`, never empty.
pub(crate) fn command_tokens(context: &Context) -> Value {
    PlaceCommand::of(context).into_value(context.span)
}

/// argv of the command the cursor is in. Always non-empty, with a trailing `""`
/// iff the cursor sits on an empty slot, so `$place.command.0` never index-errors.
struct PlaceCommand(Vec<String>);

impl PlaceCommand {
    fn of(context: &Context) -> Self {
        let words = Words::of(context);
        let mut argv: Vec<String> = match context.level.and_then(|level| level.element) {
            Some(element) => match &element.expr {
                // A call's arguments are its words as typed: the parse splits a cluster such as
                // `-vp` into one flag per letter, and leaves out words that are no flag of its.
                // An alias that brings arguments of its own is read from the parse, which has
                // them.
                Expr::Call(call) if !words.spliced(call, element.span) => {
                    words.typed(call, element.span)
                }
                Expr::Call(call) => words.call(call),
                Expr::ExternalCall(head, args) => words.external(head, args),
                // A bare word, variable, or cell path is its own command.
                _ => words.word(element.span).into_iter().collect(),
            },
            None => Vec::new(),
        };
        if context.span.is_empty() && !matches!(argv.last(), Some(w) if w.is_empty()) {
            argv.push(String::new());
        }
        if argv.is_empty() {
            argv.push(String::new());
        }
        Self(argv)
    }

    fn into_value(self, span: Span) -> Value {
        Value::list(
            self.0
                .into_iter()
                .map(|word| Value::string(word, span))
                .collect(),
            span,
        )
    }
}

/// One argument as zero to two shell words: flags stay whole (`--flag=value`) or
/// split (`--flag value`); everything else is one word or skipped when empty.
fn arg_pair(words: &Words, arg: &nu_protocol::ast::Argument) -> [Option<String>; 2] {
    use nu_protocol::ast::Argument;
    match arg {
        Argument::Positional(e) | Argument::Unknown(e) => [words.word(e.span), None],
        Argument::Spread(e) => [words.spread(e.span), None],
        Argument::Named((long, short, value)) => {
            // Long and short cover the same source text; reading both would double it.
            let long_word = words.word(long.span);
            let (flag_span, flag) = match (long_word, short) {
                (Some(flag), _) => (long.span, flag),
                (None, Some(short)) => match words.word(short.span) {
                    Some(flag) => (short.span, flag),
                    None => return [None, value.as_ref().and_then(|v| words.word(v.span))],
                },
                (None, None) => return [None, value.as_ref().and_then(|v| words.word(v.span))],
            };
            let Some(value) = value else {
                return [Some(flag), None];
            };
            // Joined (`--flag=value`, gap `=`) is one word; spaced gaps are two,
            // so `--flag foo=bar` never collapses even though the value holds `=`.
            if words.spaced(flag_span.end, value.span.start) {
                [Some(flag), words.word(value.span)]
            } else {
                [words.word(arg.span()), None]
            }
        }
    }
}

/// Shell words read off working-set spans. Only the line up to the cursor is parsed,
/// so every span an element holds lies on it, except the arguments the parser splices
/// in from an alias definition, and those are part of the argv too: `gco ma` is
/// `git checkout ma` to a completer. `None` means an empty (synthetic) span.
struct Words<'a> {
    working_set: &'a nu_protocol::engine::StateWorkingSet<'a>,
}

impl<'a> Words<'a> {
    fn of(context: &'a Context) -> Self {
        Self {
            working_set: context.working_set,
        }
    }

    /// argv of an internal call. The parser resolved an alias head to its target
    /// declaration and spliced the definition's arguments in ahead of the typed ones,
    /// so the declared name plus every argument is the expansion: `ll x` is `ls -l x`.
    /// Nested aliases were flattened the same way when the definition was parsed.
    fn call(&self, call: &nu_protocol::ast::Call) -> Vec<String> {
        let head = self.working_set.get_decl(call.decl_id).name().to_string();
        std::iter::once(head)
            .chain(
                call.arguments
                    .iter()
                    .flat_map(|arg| arg_pair(self, arg).into_iter().flatten()),
            )
            .collect()
    }

    /// argv of an external call. An alias head keeps the target on the expression while
    /// its span still reads the alias name, so the expression wins; a `$var` or `(...)`
    /// head has no fixed name and reads as typed. `^name` never resolves an alias, and
    /// the sigil sits outside the head span.
    fn external(
        &self,
        head: &nu_protocol::ast::Expression,
        args: &[nu_protocol::ast::ExternalArgument],
    ) -> Vec<String> {
        use nu_protocol::ast::ExternalArgument;
        let head = match &head.expr {
            Expr::String(name) | Expr::GlobPattern(name, _) => Some(name.clone()),
            _ => self.word(head.span),
        };
        head.into_iter()
            .chain(args.iter().filter_map(|arg| match arg {
                ExternalArgument::Regular(e) => self.word(e.span),
                ExternalArgument::Spread(e) => self.spread(e.span),
            }))
            .collect()
    }

    /// Whether `call`, written at `span`, has arguments an alias brought in from elsewhere.
    fn spliced(&self, call: &nu_protocol::ast::Call, span: Span) -> bool {
        call.arguments
            .iter()
            .any(|arg| !span.contains_span(arg.span()))
    }

    /// argv of `call` as typed at `span`: its declared name as one word, as [`Self::call`] has
    /// it, then the shell words after its head, as the lexer splits them.
    fn typed(&self, call: &nu_protocol::ast::Call, span: Span) -> Vec<String> {
        let head = self.working_set.get_decl(call.decl_id).name().to_string();
        let after = Span::new(call.head.end.min(span.end), span.end);
        let text = self.working_set.get_span_contents(after);
        let (tokens, _) = lex(text, after.start, &[], &[], true);
        std::iter::once(head)
            .chain(
                tokens
                    .into_iter()
                    .filter(|token| token.contents == TokenContents::Item)
                    .filter_map(|token| self.word(token.span)),
            )
            .collect()
    }

    fn word(&self, span: Span) -> Option<String> {
        if span.start >= span.end {
            return None;
        }
        let text = String::from_utf8_lossy(self.working_set.get_span_contents(span)).into_owned();
        (!text.is_empty()).then_some(text)
    }

    /// Whether the gap between a flag and its value holds whitespace: the
    /// shell-word boundary (`=`-joined gaps hold only `=`).
    fn spaced(&self, flag_end: usize, value_start: usize) -> bool {
        flag_end < value_start
            && self
                .working_set
                .get_span_contents(Span::new(flag_end, value_start))
                .iter()
                .any(u8::is_ascii_whitespace)
    }

    /// A spread as one word, keeping the `...` prefix that lives outside the
    /// inner span (`...$args`, not `$args`).
    fn spread(&self, inner: Span) -> Option<String> {
        let mut word = self.word(inner)?;
        if inner.start >= 3
            && self
                .working_set
                .get_span_contents(Span::new(inner.start - 3, inner.start))
                == b"..."
        {
            word.insert_str(0, "...");
        }
        Some(word)
    }
}
