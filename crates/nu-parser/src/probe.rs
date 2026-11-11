//! Recording what the parser expects at a probed offset (see [`Probe`]).
//!
//! Each hook is called by the routine that knows the expectation, with what it has at hand, and
//! returns at once when the parse has no probe.

use crate::{Token, TokenContents, lite_parser::LiteCommand};
use nu_protocol::{
    DeclId, Flag, Signature, Span, Spanned, SyntaxShape, Type,
    ast::{Argument, Block, Call, Expr, Expression, PathMember},
    engine::{Expect, Probe, StateWorkingSet, probe::touches},
};
use std::sync::Arc;

/// Parses `contents` as the file `fname`, recording what may be written at byte `at` of it.
///
/// The parse is never served from the block cache, which would skip the hooks.
pub fn parse_probed(
    working_set: &mut StateWorkingSet,
    fname: Option<&str>,
    contents: &[u8],
    at: usize,
) -> (Arc<Block>, Probe) {
    let source = crate::parse_captures_compile::add_source(working_set, fname, contents);
    let at = source.start + at.min(contents.len());
    working_set.probe = Some(Box::new(Probe {
        at,
        source,
        ..Probe::default()
    }));
    let block = crate::parse_fresh(working_set, fname, contents, false);
    let probe = working_set
        .probe
        .take()
        .map(|probe| *probe)
        .unwrap_or_default();
    (block, probe)
}

/// Whether the text between `from` and the probe is only whitespace, so the probe is in the gap
/// after a construct that ends at `from`, and nothing else claims it.
fn trails(working_set: &StateWorkingSet, from: usize) -> Option<usize> {
    let at = working_set.probe_at()?;
    (at > from
        && working_set
            .get_span_contents(Span::new(from, at))
            .iter()
            .all(u8::is_ascii_whitespace))
    .then_some(at)
}

/// Whether `word` is the operator of a redirection (`out>`, `e>>`, `o+e>`), or a `>` as other
/// shells write one.
fn is_redirection(word: &[u8]) -> bool {
    let stream = word.strip_suffix(b">>").or_else(|| word.strip_suffix(b">"));
    stream.is_some_and(|stream| {
        matches!(
            stream,
            b"" | b"o" | b"out" | b"e" | b"err" | b"o+e" | b"out+err" | b"e+o" | b"err+out"
        )
    })
}

/// Whether a token is a flag being written: an argument that starts with `-` but is no number.
fn is_flag_text(text: &[u8]) -> bool {
    text.starts_with(b"-") && !text.get(1).is_some_and(u8::is_ascii_digit)
}

/// A command head: the probe is in the name of the command, or in a word after it that could
/// make it a longer name (`str tr⌶` is `str trim` being written).
pub(crate) fn head(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    name_end: usize,
    parent: Option<DeclId>,
    input: Option<&Type>,
) {
    let Some(at) = working_set.probe_at() else {
        return;
    };
    let Some(first) = spans.first() else {
        return;
    };
    // The words of the name.
    let name = &spans[..name_end.min(spans.len())];
    if let Some(last) = name.last()
        && name.iter().any(|&span| touches(span, at))
    {
        let span = Span::new(first.start, last.end);
        working_set.expect(span, span, input, || Expect::Command { parent: None });
        // A name of several words is also a shorter command and its arguments (`foo bar` as
        // `foo`'s argument), whose expectations its parse records.
        if name[1..].iter().any(|&span| touches(span, at)) {
            let errors = working_set.parse_errors.len();
            let reading = crate::parse_shorter_head_reading(working_set, span, input);
            working_set.parse_errors.truncate(errors);
            if let (Some(reading), Some(probe)) = (reading, working_set.probe.as_mut()) {
                probe.readings.push(reading);
            }
        }
        return;
    }
    // The word after the name, or the gap after it, when it may make a longer name: a word of
    // letters after a command that has subcommands.
    let word = match spans.get(name_end) {
        Some(&word) if touches(word, at) => Some(word),
        None => name
            .last()
            .and_then(|last| trails(working_set, last.end))
            .map(|at| Span::new(at, at)),
        Some(_) => None,
    };
    if let (Some(parent), Some(word)) = (parent, word)
        && (working_set.get_span_contents(word).first()).is_none_or(u8::is_ascii_alphabetic)
        && has_subcommands(working_set, parent)
    {
        let span = Span::new(first.start, word.end);
        working_set.expect(span, span, input, || Expect::Command {
            parent: Some(parent),
        });
    }
}

/// The flag a named argument is, by its long name, or its short one when it has no long one.
fn flag_of(
    signature: &Signature,
    name: &Spanned<String>,
    short: Option<&Spanned<String>>,
) -> Option<Flag> {
    signature.get_long_flag(&name.item).or_else(|| {
        let short = short?.item.chars().next()?;
        signature.get_short_flag(short)
    })
}

/// Whether some command is named `decl_id`'s name and more words.
fn has_subcommands(working_set: &StateWorkingSet, decl_id: DeclId) -> bool {
    let mut prefix = working_set.get_decl(decl_id).name().as_bytes().to_vec();
    prefix.push(b' ');
    let mut found = false;
    working_set.traverse_commands(|name, _| found |= name.starts_with(&prefix));
    found
}

/// The arguments of a finished internal call, and the gap after them.
pub(crate) fn call(
    working_set: &mut StateWorkingSet,
    head: Span,
    spans: &[Span],
    call: &Call,
    signature: &Signature,
    input: Option<&Type>,
) {
    let Some(at) = working_set.probe_at() else {
        return;
    };
    let decl_id = call.decl_id;
    let end = spans.last().map_or(head.end, |span| span.end);
    let within = Span::new(head.start, end.max(at));

    let mut positional = 0;
    for argument in &call.arguments {
        match argument {
            Argument::Named((name, short, value)) => {
                let flag_span = short.as_ref().map_or(name.span, |short| short.span);
                working_set.expect(flag_span, within, input, || Expect::Flag { decl_id });
                if let Some(value) = value
                    && let Some(flag) = flag_of(signature, name, short.as_ref())
                    && let Some(shape) = flag.arg
                {
                    working_set.expect(value.span, within, input, || Expect::FlagValue {
                        decl_id,
                        long: flag.long,
                        shape,
                    });
                }
            }
            Argument::Positional(expression) | Argument::Unknown(expression) => {
                let index = positional;
                if matches!(argument, Argument::Positional(_)) {
                    positional += 1;
                }
                if !touches(expression.span, at) {
                    continue;
                }
                // A flag still being written parses as a positional.
                if is_flag_text(working_set.get_span_contents(expression.span)) {
                    working_set.expect(expression.span, within, input, || Expect::Flag { decl_id });
                } else if let Some(parameter) = signature.get_positional(index) {
                    let shape = parameter.shape.clone();
                    working_set.expect(expression.span, within, input, || Expect::Positional {
                        decl_id,
                        index,
                        shape,
                    });
                }
            }
            Argument::Spread(_) => {}
        }
    }

    // A flag-like word that no argument holds, such as `--`, is a flag being written.
    if let Some(&word) = spans.iter().find(|&&span| touches(span, at))
        && !call
            .arguments
            .iter()
            .any(|argument| touches(argument.span(), at))
        && is_flag_text(working_set.get_span_contents(word))
    {
        working_set.expect(word, within, input, || Expect::Flag { decl_id });
    }

    // A gap between two arguments is where another one may be written.
    let previous = spans.iter().rfind(|span| span.end < at);
    if at < end
        && !spans.iter().any(|&span| touches(span, at))
        && trails(working_set, previous.map_or(head.end, |span| span.end)).is_some()
    {
        let index = (call.arguments.iter())
            .filter(|arg| matches!(arg, Argument::Positional(_)) && arg.span().end < at)
            .count();
        let point = Span::new(at, at);
        if let Some(parameter) = signature.get_positional(index) {
            let shape = parameter.shape.clone();
            working_set.expect(point, within, input, || Expect::Positional {
                decl_id,
                index,
                shape,
            });
        }
        return working_set.expect(point, within, input, || Expect::Flag { decl_id });
    }
    let Some(at) = trails(working_set, end) else {
        return;
    };
    let point = Span::new(at, at);
    // A redirection whose file is still to come is read as an argument until it has one.
    if let Some(last) = call.arguments.last()
        && !matches!(last.expr().map(|value| &value.expr), Some(Expr::Garbage))
        && is_redirection(working_set.get_span_contents(last.span()))
    {
        return redirection(working_set, last.span(), None);
    }
    // A flag whose value is still to come takes the gap for itself.
    if let Some(Argument::Named((name, short, None))) = call.arguments.last()
        && let Some(flag) = flag_of(signature, name, short.as_ref())
        && let Some(shape) = flag.arg
    {
        working_set.expect(point, within, input, || Expect::FlagValue {
            decl_id,
            long: flag.long,
            shape,
        });
        return;
    }
    // So does a keyword whose value is still to come (`for x in ⌶`).
    let waiting = matches!(call.arguments.last(), Some(Argument::Positional(Expression {
        expr: Expr::Keyword(keyword),
        ..
    })) if matches!(keyword.expr.expr, Expr::Garbage) && keyword.span.end == end);
    let index = positional.saturating_sub(usize::from(waiting));
    if let Some(parameter) = signature.get_positional(index) {
        let shape = match &parameter.shape {
            SyntaxShape::Keyword(_, shape) if waiting => (**shape).clone(),
            shape => shape.clone(),
        };
        working_set.expect(point, within, input, || Expect::Positional {
            decl_id,
            index,
            shape,
        });
    }
    working_set.expect(point, within, input, || Expect::Flag { decl_id });
}

/// The arguments of an external call, and the gap after them.
pub(crate) fn external(working_set: &mut StateWorkingSet, head: Span, arguments: &[Span]) {
    // `!` is a `not` of other languages, not a program to run.
    let Some(at) = working_set
        .probe_at()
        .filter(|_| working_set.get_span_contents(head) != b"!")
    else {
        return;
    };
    let end = arguments.last().map_or(head.end, |span| span.end);
    let within = Span::new(head.start, end.max(at));
    for (index, span) in arguments.iter().enumerate() {
        working_set.expect(*span, within, None, || Expect::ExternalArg { index });
    }
    if let Some(at) = trails(working_set, end) {
        let index = arguments.len();
        working_set.expect(Span::new(at, at), within, None, || Expect::ExternalArg {
            index,
        });
    }
}

/// A math expression whose operator has no right-hand side yet (`size > ⌶`), or whose operator
/// is still being written (`size >⌶`).
pub(crate) fn missing_operand(
    working_set: &mut StateWorkingSet,
    lhs: &Expression,
    operator: &Expression,
    input: Option<&Type>,
) {
    operator_word(working_set, lhs, operator.span, input);
    let Expr::Operator(op) = operator.expr else {
        return;
    };
    if let Some(at) = trails(working_set, operator.span.end) {
        let within = Span::new(lhs.span.start, at);
        working_set.expect(Span::new(at, at), within, input, || Expect::Operand {
            lhs: Box::new(lhs.clone()),
            operator: op,
        });
    }
}

/// An operator being written after `lhs`, in the word `span` (`size >⌶`, `1 bit-s⌶`).
pub(crate) fn operator_word(
    working_set: &mut StateWorkingSet,
    lhs: &Expression,
    span: Span,
    input: Option<&Type>,
) {
    let within = Span::new(lhs.span.start, span.end);
    working_set.expect(span, within, input, || Expect::Operator {
        lhs: Box::new(lhs.clone()),
    });
}

/// A right-hand operand being written (`size > 10⌶`).
pub(crate) fn operand(
    working_set: &mut StateWorkingSet,
    lhs: &Expression,
    operator: &Expression,
    rhs: &Expression,
    input: Option<&Type>,
) {
    let Expr::Operator(op) = operator.expr else {
        return;
    };
    let within = Span::new(lhs.span.start, rhs.span.end);
    working_set.expect(rhs.span, within, input, || Expect::Operand {
        lhs: Box::new(lhs.clone()),
        operator: op,
    });
}

/// The gap after a finished operand, where an operator may come (`size ⌶`, `$x == 1 ⌶`).
pub(crate) fn after_operand(
    working_set: &mut StateWorkingSet,
    operand: &Expression,
    input: Option<&Type>,
) {
    if let Some(at) = trails(working_set, operand.span.end) {
        let within = Span::new(operand.span.start, at);
        working_set.expect(Span::new(at, at), within, input, || Expect::Operator {
            lhs: Box::new(operand.clone()),
        });
    }
}

/// A cell path whose member at the probe is being written, or whose trailing `.` waits for one
/// (`$in.na⌶`, `$env.⌶`): what the path is up to that member says what the member can be.
pub(crate) fn members(
    working_set: &mut StateWorkingSet,
    span: Span,
    head: &Expression,
    tail: &[PathMember],
    input: Option<&Type>,
) {
    let Some(at) = working_set.probe_at().filter(|&at| touches(span, at)) else {
        return;
    };
    let up_to = |members: &[PathMember]| {
        head.ty
            .follow_cell_path(members)
            .map_or(Type::Any, |ty| ty.into_owned())
    };
    if let Some(index) = tail.iter().position(|member| touches(member.span(), at)) {
        let ty = up_to(&tail[..index]);
        working_set.expect(tail[index].span(), span, input, || Expect::Member {
            head: ty,
        });
    } else if at == span.end && working_set.get_span_contents(span).ends_with(b".") {
        let ty = up_to(tail);
        working_set.expect(Span::new(at, at), span, input, || Expect::Member {
            head: ty,
        });
    }
}

/// A variable name being written.
pub(crate) fn variable(working_set: &mut StateWorkingSet, span: Span, input: Option<&Type>) {
    working_set.expect(span, span, input, || Expect::Variable);
}

/// The operand of a `not` that is still to come (`not ⌶`).
pub(crate) fn negated(working_set: &mut StateWorkingSet, not: Span, input: Option<&Type>) {
    if let Some(at) = trails(working_set, not.end) {
        let within = Span::new(not.start, at);
        working_set.expect(Span::new(at, at), within, input, || Expect::Negated);
    }
}

/// The start of a pipeline element in a block, where a command is written: the start of the
/// block, past what opens it, or after a `|` or a `;`, in no word. `tokens` are the block's, and
/// `span` its extent, with the bracket that opens it if one does.
pub(crate) fn statement(
    working_set: &mut StateWorkingSet,
    tokens: &[Token],
    span: Span,
    input: Option<&Type>,
) {
    let Some(at) = working_set.probe_at().filter(|&at| touches(span, at)) else {
        return;
    };
    if (tokens.iter()).any(|token| token.contents == TokenContents::Item && touches(token.span, at))
    {
        return;
    }
    let last = (tokens.iter().filter(|token| token.span.start < at))
        .rfind(|token| !matches!(token.contents, TokenContents::Eol | TokenContents::Comment));
    let starts = match last {
        Some(token) => matches!(
            token.contents,
            TokenContents::Pipe | TokenContents::Semicolon
        ),
        None if at == span.start => !working_set.get_span_contents(span).starts_with(b"{"),
        None => (working_set.get_span_contents(Span::new(at - 1, at)))
            .first()
            .is_some_and(u8::is_ascii_whitespace),
    };
    if starts {
        let point = Span::new(at, at);
        working_set.expect(point, point, input, || Expect::Command { parent: None });
    }
}

/// Where a pattern of a `match` arm is written: in the word `pattern`, or in a gap where the next
/// arm begins, after `previous` (the last token of the arm before) or the opening brace.
pub(crate) fn match_pattern(
    working_set: &mut StateWorkingSet,
    previous: Option<Span>,
    pattern: Option<Span>,
    block: Span,
) {
    let Some(at) = working_set.probe_at().filter(|&at| touches(block, at)) else {
        return;
    };
    let from = previous.map_or(block.start + 1, |previous| previous.end + 1);
    let gap = at >= from
        && pattern.is_none_or(|pattern| at <= pattern.start)
        && (working_set.get_span_contents(Span::new(from, at)))
            .iter()
            .all(|&byte| byte.is_ascii_whitespace() || byte == b',');
    let span = match pattern.filter(|&pattern| touches(pattern, at)) {
        Some(pattern) => pattern,
        None if gap => Span::new(at, at),
        None => return,
    };
    working_set.expect(span, span, None, || Expect::MatchPattern);
}

/// The attributes of a command and the definition they decorate: an attribute's name being
/// written (`@ex⌶`), or the definition, in its first word or still to come after the last
/// attribute (`@example; ⌶`).
pub(crate) fn attributes(working_set: &mut StateWorkingSet, command: &LiteCommand) {
    let Some(at) = working_set.probe_at() else {
        return;
    };
    for attribute in command.attribute_commands() {
        if let Some(&word) = attribute.parts.first()
            && touches(word, at)
        {
            let name = Span::new((word.start + 1).min(word.end), word.end);
            working_set.expect(name, name, None, || Expect::AttributeName);
        }
    }
    let end = command.parts[..command.parts.len() - command.command_parts().len()]
        .last()
        .map_or(at, |last| last.end);
    let span = match command.command_parts().first() {
        Some(&item) if touches(item, at) => item,
        Some(_) => return,
        None => Span::new(at, at),
    };
    let gap = working_set.get_span_contents(Span::new(end.min(span.start), span.start));
    if at > end
        && gap
            .iter()
            .all(|&byte| byte.is_ascii_whitespace() || byte == b';')
    {
        working_set.expect(span, Span::new(at, at), None, || Expect::Attributable);
    }
}

/// The file a redirection at `connector` writes to: `file` being written, or the gap after a
/// connector that has none yet.
pub(crate) fn redirection(working_set: &mut StateWorkingSet, connector: Span, file: Option<Span>) {
    let span = match file {
        Some(file) => file,
        None => match trails(working_set, connector.end) {
            Some(at) => Span::new(at, at),
            None => return,
        },
    };
    let within = Span::new(connector.start, span.end);
    working_set.expect(span, within, None, || Expect::Redirection);
}

/// An expression positional cut short to leave words for the positionals after it (`if $x == ⌶`
/// before its block is written): the probe hears the expression as if it took them all.
pub(crate) fn cut_short(
    working_set: &mut StateWorkingSet,
    shape: &SyntaxShape,
    spans: &[Span],
    end: usize,
    input: Option<&Type>,
) {
    if *shape == SyntaxShape::MathExpression
        && let (Some(kept), Some(at)) = (
            end.checked_sub(1).and_then(|end| spans.get(end)),
            working_set.probe_at(),
        )
        && kept.end < at
        && end < spans.len()
    {
        let errors = working_set.parse_errors.len();
        let _ = crate::parse_expressions::parse_math_expression(working_set, spans, None, input);
        working_set.parse_errors.truncate(errors);
    }
}

/// A record being written at the probe (`{a: 1, ⌶`, `{a: ⌶`): the keys it has, and the key whose
/// value the probe is at, if it is at one. `tokens` are the record's keys, colons and values.
pub(crate) fn record(working_set: &mut StateWorkingSet, span: Span, tokens: &[Token]) {
    let Some(at) = working_set
        .probe_at()
        .filter(|&at| span.start < at && at <= span.end)
    else {
        return;
    };
    let name = |token: &Token| {
        let text = String::from_utf8_lossy(working_set.get_span_contents(token.span));
        text.trim_matches(['\'', '"', '`']).to_owned()
    };
    let mut keys = Vec::new();
    let mut pairs = tokens.chunks(3);
    let (place, value_of) = loop {
        match pairs.next() {
            None => break (Span::new(at, at), None),
            Some([key, ..]) if touches(key.span, at) => break (key.span, None),
            Some([key, _colon]) if at > key.span.end => break (Span::new(at, at), Some(name(key))),
            Some([key, _colon, value]) if touches(value.span, at) => {
                break (value.span, Some(name(key)));
            }
            Some([key, ..]) => keys.push(name(key)),
            Some([]) => return,
        }
    };
    let what = || Expect::RecordKey { keys, value_of };
    working_set.expect(place, span, None, what);
}
