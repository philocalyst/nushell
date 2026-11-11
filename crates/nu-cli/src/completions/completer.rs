use crate::completions::{
    ArgValueCompletion, CommandScope, CompletionOptions, Details, HelpAt, HelpFor,
    SemanticSuggestion, Slot, Source, ask, ask_fallback,
    base::to_reedline_span,
    cache::{CacheEnv, CacheView, CompletionCache},
    command_completions::{self, executable_files},
    delegation::{self, Delegation, Sources, WRAPPERS},
    piped::{self, Flow, Nest},
    placeholder,
    trace::Trace,
    usage::{HistoryReader, UsageIndex},
};
use crate::syntax_highlight::{LineParseCache, NuHighlighter};
use nu_parser::{FlatShape, parse_probed};
use nu_protocol::{
    BlockId, BuiltinCompletion, CommandWideCompleter, Completion, Config, DeclId, Flag, Record,
    Signature, Span, SuggestionKind, SyntaxShape, Type, Value, VarId,
    ast::{
        Argument, Block, Call, Comparison, Expr, Expression, FlagRef, FullCellPath, Operator,
        PathMember, Traverse,
    },
    engine::{
        ArgType, Closure, Command, CommandType, EngineState, Expect, Expected, Probe, Stack,
        StateWorkingSet,
    },
};

use nu_utils::NuCow;
use reedline::{
    Completer as ReedlineCompleter, Completions, History, Opening, Preview, Reply, Request,
    Suggestion,
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

/// A shared list of suggestions, as the cache keeps it.
pub(crate) type Suggestions = Arc<[Suggestion]>;
use std::{borrow::Cow, ops::ControlFlow};

use super::custom_completions::{DeclaredInputs, UserCompletion, completer_input};
use super::taking;

/// `found` without what the call has been given already: the columns before the cursor in
/// `select name type ⌶`, and the flags in `ls -a -⌶` of a command that takes a flag once. A word
/// a rest parameter could take twice is still there to type.
fn leave_out_what_is_written(
    found: &mut Vec<SemanticSuggestion>,
    site: &CompletionSite,
    call: &Call,
    signature: &Signature,
    working_set: &StateWorkingSet,
    offset: usize,
) {
    // A completer for the whole command knows what the command's arguments can repeat, and
    // short of the end of the call the word at the cursor may be one being changed.
    if signature.complete.is_some() || site.span.end < call.span().end {
        return;
    }
    let before = |span: Span| span.end <= site.span.start;
    let text = |span: Span| String::from_utf8_lossy(working_set.get_span_contents(span));
    match &site.kind {
        SiteKind::Positional { sig_positional, .. }
            if *sig_positional
                >= signature.required_positional.len() + signature.optional_positional.len() =>
        {
            let given: Vec<_> = (call.arguments.iter())
                .filter(|argument| {
                    matches!(argument, Argument::Positional(_) | Argument::Unknown(_))
                })
                .map(Argument::span)
                .filter(|span| before(*span))
                .map(|span| text(span).trim_matches(['\'', '"', '`']).to_owned())
                .collect();
            // A completer that says what it replaces has placed its answer itself.
            let token = to_reedline_span(site.span, offset);
            found.retain(|item| {
                item.suggestion.span != token || !given.contains(&item.suggestion.value)
            });
        }
        SiteKind::FlagName { .. } if !signature.allows_unknown_args => {
            let given: Vec<_> = (call.arguments.iter())
                .filter_map(|argument| match argument {
                    Argument::Named(named) if before(argument.span()) => {
                        find_flag(signature, FlagRef::from_named(&named.0, named.1.as_ref()))
                    }
                    _ => None,
                })
                .collect();
            found.retain(|item| {
                let value = item.suggestion.value.trim_start_matches('-');
                !given.iter().any(|flag| {
                    flag.long == value || flag.short.is_some_and(|short| value == short.to_string())
                })
            });
        }
        _ => {}
    }
}

pub(crate) use nu_protocol::engine::probe::touches;

/// The block a nesting expression runs.
fn nested_block(expr: &Expression) -> Option<BlockId> {
    match expr.expr {
        Expr::RowCondition(block_id)
        | Expr::Subexpression(block_id)
        | Expr::Block(block_id)
        | Expr::Closure(block_id) => Some(block_id),
        _ => None,
    }
}

/// The nesting expression the cursor descends into from `element`.
fn descent_expression<'a>(
    element: &'a Expression,
    working_set: &'a StateWorkingSet,
    pos: usize,
) -> Option<&'a Expression> {
    element.find_map(working_set, &|expr: &'a Expression| {
        if !touches(expr.span, pos) {
            // Skip this subtree; the caller keeps searching its siblings.
            return ControlFlow::Break(None);
        }
        match nested_block(expr) {
            Some(_) => ControlFlow::Break(Some(expr)),
            None => ControlFlow::Continue(()),
        }
    })
}

/// The block the cursor descends into from `element`, and its span.
fn descent_from<'a>(
    element: &'a Expression,
    working_set: &'a StateWorkingSet,
    pos: usize,
) -> Option<(BlockId, Span)> {
    let expression = descent_expression(element, working_set, pos)?;
    Some((nested_block(expression)?, expression.span))
}

/// The element the cursor is on; a trailing gap belongs to the last.
fn element_at<'a>(
    block: &'a Block,
    working_set: &StateWorkingSet,
    pos: usize,
) -> Option<&'a Expression> {
    block
        .pipelines
        .iter()
        .flat_map(|pipeline| &pipeline.elements)
        .map(|element| &element.expr)
        .find(|expr| touches(expr.span, pos))
        .or_else(|| trailing_gap_element(block, working_set, pos))
}

/// An element enclosing the cursor, the span of the nesting expression the cursor descends into
/// from it, and the block that holds it.
type Enclosing<'a> = (&'a Expression, Option<Span>, &'a Block);

/// What flows into a command begun right after a pipe (`ls | ⌶`): what the last pipeline of
/// `block` leaves flowing, when only that pipe lies between its end and the cursor.
fn after_pipe(
    block: &Block,
    working_set: &StateWorkingSet,
    cursor: usize,
    flow: &Flow,
) -> Option<Type> {
    let pipeline = block.pipelines.last()?;
    let end = pipeline.elements.last()?.expr.span.end;
    let gap = working_set.get_span_contents(Span::new(end, cursor.max(end)));
    (gap.trim_ascii() == b"|")
        .then(|| flow.flowing_into(&pipeline.elements, pipeline.elements.len()))
        .flatten()
}

/// The nests of `chain`: each element with what flows into it.
fn nests_of<'a>(chain: &[Enclosing<'a>], flow: &Flow) -> Vec<Nest<'a>> {
    let mut nests = Vec::with_capacity(chain.len());
    for &(element, _, block) in chain {
        let nest = flow.nest(element, block, &nests);
        nests.push(nest);
    }
    nests
}

/// The elements enclosing the cursor, outermost first.
fn enclosing_elements<'a>(
    block: &'a Block,
    working_set: &'a StateWorkingSet,
    pos: usize,
) -> Vec<Enclosing<'a>> {
    let mut chain = Vec::new();
    let mut block = block;

    while let Some(element) = element_at(block, working_set, pos) {
        let descent = descent_from(element, working_set, pos);
        chain.push((element, descent.map(|(_, span)| span), block));

        let Some((block_id, _)) = descent else { break };
        block = working_set.get_block(block_id);
    }

    chain
}

/// The last element when the cursor trails it over whitespace.
fn trailing_gap_element<'a>(
    block: &'a Block,
    working_set: &StateWorkingSet,
    absolute_position: usize,
) -> Option<&'a Expression> {
    let expression = &block.pipelines.last()?.elements.last()?.expr;

    // The element can end past the cursor (whole files, later statements); a backwards
    // span would panic.
    if expression.span.end > absolute_position {
        return None;
    }

    let gap = working_set.get_span_contents(Span::new(expression.span.end, absolute_position));
    gap.iter()
        .all(u8::is_ascii_whitespace)
        .then_some(expression)
}

/// Adds `subcommands` to `found`. Each value in `found` that names a subcommand of `parent`
/// (`checkout`, after `git`) becomes that command, so it is tagged and previewed as one, and the
/// same command among `subcommands`, there under its full name, is not offered twice.
fn link_subcommands(
    found: &mut Vec<SemanticSuggestion>,
    mut subcommands: Vec<SemanticSuggestion>,
    parent: &str,
    working_set: &StateWorkingSet,
) {
    let mut linked = HashSet::new();
    for item in found.iter_mut() {
        if !matches!(item.kind, None | Some(SuggestionKind::Value(_))) {
            continue;
        }
        let name = format!("{parent} {}", item.suggestion.value);
        if let Some(decl_id) = working_set.find_decl(name.as_bytes()) {
            let command_type = working_set.get_decl(decl_id).command_type();
            item.kind = Some(SuggestionKind::Command(command_type, Some(decl_id)));
            linked.insert(name);
        }
    }
    subcommands.retain(|subcommand| !linked.contains(&subcommand.suggestion.value));
    found.extend(subcommands);
}

/// Whether the token starts a flag; in-progress flags parse as positionals.
pub(crate) fn is_flag_text(token: impl AsRef<[u8]>) -> bool {
    token.as_ref().starts_with(b"-")
}

/// The flag a [`FlagRef`] refers to, preserving the long/short distinction.
pub(crate) fn find_flag(signature: &Signature, flag: FlagRef<'_>) -> Option<Flag> {
    match flag {
        FlagRef::Long(n) => signature.get_long_flag(n),
        FlagRef::Short(s) => s.chars().next().and_then(|c| signature.get_short_flag(c)),
    }
}

/// Positional arguments before `before_index`.
fn count_positionals(call: &Call, before_index: usize) -> usize {
    call.arguments
        .iter()
        .take(before_index)
        .filter(|argument| !matches!(argument, Argument::Named(_)))
        .count()
}

/// A call and the pipeline element it lives in: the shared payload of every
/// call-bound [`SiteKind`]. Carrying them as one value keeps an element from
/// being paired with another call's arguments.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallSite<'a> {
    pub call: &'a Call,
    pub element: &'a Expression,
}

/// The first expression around `at`, outermost first, of `block` or of the probe's own readings,
/// that `is` picks.
fn find_expression<'a>(
    block: &'a Block,
    probe: &'a Probe,
    working_set: &'a StateWorkingSet,
    at: usize,
    is: impl Fn(&Expression) -> bool,
) -> Option<&'a Expression> {
    let visit = |expression: &'a Expression| {
        if is(expression) {
            ControlFlow::Break(Some(expression))
        } else if touches(expression.span, at) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(None)
        }
    };
    block.find_map(working_set, &visit).or_else(|| {
        (probe.readings.iter()).find_map(|reading| reading.find_map(working_set, &visit))
    })
}

/// The narrowest argument of a call around `inner` that the probe heard, if any.
fn argument_around<'a>(probe: &'a Probe, inner: &Expected) -> Option<&'a Expected> {
    // A parse the parser went back on comes before the one it kept.
    (probe.found.iter().rev())
        .filter(|outer| {
            matches!(
                outer.what,
                Expect::Positional { .. } | Expect::Flag { .. } | Expect::FlagValue { .. }
            ) && outer.within.contains_span(inner.within)
                && outer.within != inner.within
        })
        .min_by_key(|outer| outer.within.len())
}

/// The call an expectation `found` of the probe is an argument of, as a [`CallSite`]: the one
/// whose head starts where it does, of its command, or of another, which a keyword such as
/// `export` may make of the call once it is parsed.
fn call_of<'a>(
    found: &Expected,
    block: &'a Block,
    probe: &'a Probe,
    working_set: &'a StateWorkingSet,
) -> Option<CallSite<'a>> {
    let (Expect::Positional { decl_id, .. }
    | Expect::Flag { decl_id }
    | Expect::FlagValue { decl_id, .. }
    | Expect::Command {
        parent: Some(decl_id),
    }) = found.what
    else {
        return None;
    };
    let head = found.within.start;
    let call = |decl: Option<DeclId>| {
        find_expression(block, probe, working_set, head, move |expression| {
            matches!(&expression.expr, Expr::Call(call)
                if call.head.start == head && decl.is_none_or(|decl| decl == call.decl_id))
        })
    };
    let element = call(Some(decl_id)).or_else(|| call(None))?;
    match &element.expr {
        Expr::Call(call) => Some(CallSite { call, element }),
        _ => None,
    }
}

/// Whether a value of `shape` is a condition on a row.
/// Whether the parser read every part of `expression`.
fn readable(expression: &Expression) -> bool {
    match &expression.expr {
        Expr::Garbage => false,
        Expr::BinaryOp(lhs, op, rhs) => readable(lhs) && readable(op) && readable(rhs),
        _ => true,
    }
}

/// The keywords a parameter of `shape` starts with.
fn keywords_of(shape: Option<&SyntaxShape>) -> Vec<String> {
    match shape {
        Some(SyntaxShape::Keyword(keyword, _)) => vec![String::from_utf8_lossy(keyword).into()],
        Some(SyntaxShape::OneOf(shapes)) => {
            shapes.iter().flat_map(|s| keywords_of(Some(s))).collect()
        }
        _ => Vec::new(),
    }
}

fn is_condition(shape: &SyntaxShape) -> bool {
    match shape {
        SyntaxShape::RowCondition => true,
        SyntaxShape::OneOf(shapes) => shapes.iter().any(is_condition),
        _ => false,
    }
}

/// The call around `inner` whose row condition it is in, the condition's index among the call's
/// parameters, and among its arguments.
fn condition_around<'a>(
    inner: &Expected,
    probe: &'a Probe,
    block: &'a Block,
    working_set: &'a StateWorkingSet,
) -> Option<(CallSite<'a>, usize, usize)> {
    let outer = argument_around(probe, inner)?;
    let site = call_of(outer, block, probe, working_set)?;
    let signature = working_set.get_decl(site.call.decl_id).signature();
    let index = (signature.required_positional.iter())
        .chain(&signature.optional_positional)
        .position(|parameter| is_condition(&parameter.shape))?;
    let arg_slot = (site.call.arguments.iter()).position(|argument| {
        matches!(
            argument.expr().map(|e| &e.expr),
            Some(Expr::RowCondition(_))
        )
    })?;
    Some((site, index, arg_slot))
}

/// The parameter of a call whose condition or closure the cursor is in, which has help for what
/// is written in it.
fn help_around(
    probe: &Probe,
    block: &Block,
    working_set: &StateWorkingSet,
    cursor: usize,
) -> Option<Arc<HelpAt>> {
    let (_, decl_id, index) = (probe.found.iter())
        .filter(|outer| outer.span.start < cursor && touches(outer.span, cursor))
        .filter_map(|outer| {
            let Expect::Positional { decl_id, index, .. } = outer.what else {
                return None;
            };
            let call = call_of(outer, block, probe, working_set)?.call;
            (call.positional_iter())
                .any(|argument| {
                    argument.span == outer.span
                        && matches!(argument.expr, Expr::RowCondition(_) | Expr::Closure(_))
                })
                .then_some((outer.span.len(), decl_id, index))
        })
        .min()?;
    let (command, slot) = (HelpFor::Decl(decl_id), Slot::Positional(index));
    Some(Arc::new(HelpAt { command, slot }))
}

/// What the probe's expectations `found` (the innermost, of one reading of the line) make of the
/// cursor, and what a suggestion replaces. The most specific comes first: a construct inside an
/// argument, such as an operand, before the argument, and a positional before a flag where either
/// may be written. One the parse holds nothing for leaves it to the next.
fn kind_of<'a>(
    found: &[&'a Expected],
    probe: &'a Probe,
    block: &'a Block,
    working_set: &'a StateWorkingSet,
    cursor: usize,
) -> (SiteKind<'a>, Span) {
    let rank = |found: &&Expected| match found.what {
        Expect::Attributable => 0,
        Expect::AttributeName => 1,
        Expect::Redirection => 2,
        Expect::MatchPattern => 3,
        Expect::Operand { .. } | Expect::Negated => 4,
        Expect::Operator { .. } => 5,
        Expect::Member { .. } => 6,
        Expect::Variable => 7,
        Expect::FlagValue { .. } => 8,
        Expect::Positional { .. } => 9,
        Expect::Flag { .. } => 10,
        Expect::Command { parent: Some(_) } => 11,
        Expect::Command { parent: None } => 12,
        Expect::ExternalArg { .. } => 13,
        Expect::RecordKey { .. } => 14,
    };
    // A parse the parser went back on comes before the one it kept.
    let mut found: Vec<&Expected> = found.iter().rev().copied().collect();
    found.sort_by_key(rank);
    (found.into_iter())
        .find_map(|found| read(found, probe, block, working_set, cursor))
        .unwrap_or((SiteKind::Literal, Span::point(cursor)))
}

/// What one expectation of the probe makes of the cursor, if the parse holds what it is about.
fn read<'a>(
    found: &'a Expected,
    probe: &'a Probe,
    block: &'a Block,
    working_set: &'a StateWorkingSet,
    cursor: usize,
) -> Option<(SiteKind<'a>, Span)> {
    let span = found.span;
    // An expression the probe holds, as the parse kept it: an attempt the parser went back on
    // has variables of its own.
    let kept = |expression: &'a Expression| {
        let kind = std::mem::discriminant(&expression.expr);
        find_expression(block, probe, working_set, expression.span.start, |kept| {
            kept.span == expression.span && std::mem::discriminant(&kept.expr) == kind
        })
        .unwrap_or(expression)
    };
    // A new operand of a row condition, after `and` or `not`, may be any of its columns.
    let term = || {
        let (site, sig_positional, arg_slot) = condition_around(found, probe, block, working_set)?;
        Some(SiteKind::Positional {
            site,
            sig_positional,
            arg_slot,
        })
    };
    // What a cell path of a variable is compared with is among the values it holds.
    let compared = |subject: &'a Expression, comparison| match &subject.expr {
        Expr::FullCellPath(path) if let Expr::Var(head) = path.head.expr => SiteKind::Operand {
            head,
            members: path.tail.clone(),
            comparison,
        },
        _ => SiteKind::Literal,
    };
    let kind = match &found.what {
        // A record's keys are what `records::help` offers.
        Expect::RecordKey { .. } => return None,
        Expect::Attributable => SiteKind::AttributableItem,
        Expect::AttributeName => SiteKind::AttributeName,
        Expect::Redirection => SiteKind::File,
        Expect::Variable => SiteKind::Variable,
        // An operand with words the parser could not read takes no operator: it was read by an
        // attempt the parser went back on, as `if true { ls } else` once read as a condition.
        Expect::Operator { lhs } if readable(lhs) => {
            let lhs = kept(lhs);
            // An operator typed after a comparison's value goes on with the whole comparison.
            let compared = find_expression(block, probe, working_set, lhs.span.start, |whole| {
                matches!(&whole.expr, Expr::BinaryOp(_, op, rhs)
                    if rhs.span == lhs.span && matches!(op.expr, Expr::Operator(Operator::Comparison(_))))
            });
            SiteKind::Operator {
                lhs: compared.unwrap_or(lhs),
            }
        }
        Expect::Operator { .. } => return None,
        Expect::Member { .. } => {
            let path = find_expression(block, probe, working_set, cursor, |expression| {
                expression.span == found.within && matches!(expression.expr, Expr::FullCellPath(_))
            })?;
            let Expr::FullCellPath(full) = &path.expr else {
                return None;
            };
            return Some((SiteKind::CellPath { path: full }, path.span));
        }
        Expect::Operand {
            lhs,
            operator: Operator::Comparison(comparison),
        } => match compared(kept(lhs), *comparison) {
            SiteKind::Literal => SiteKind::Variable,
            kind => kind,
        },
        Expect::Operand {
            operator: Operator::Boolean(_),
            ..
        }
        | Expect::Negated => term().unwrap_or(SiteKind::Variable),
        Expect::Operand { .. } => SiteKind::Variable,
        // A pattern is compared with what is matched.
        Expect::MatchPattern => {
            let outer = argument_around(probe, found)?;
            let site = call_of(outer, block, probe, working_set)?;
            match site.call.positional_iter().next() {
                Some(subject) => compared(subject, Comparison::Equal),
                None => SiteKind::Literal,
            }
        }
        Expect::ExternalArg { index } => SiteKind::ExternalArg {
            call: find_expression(
                block,
                probe,
                working_set,
                cursor,
                |expression| matches!(&expression.expr, Expr::ExternalCall(head, _) if head.span.start == found.within.start),
            )?,
            index: *index,
        },
        Expect::Command { parent: None } => SiteKind::Command {
            node: find_expression(block, probe, working_set, span.start, |expression| {
                match &expression.expr {
                    Expr::Call(call) => call.head.start == span.start,
                    Expr::ExternalCall(head, _) => head.span.start == span.start,
                    Expr::Garbage => expression.span.start == span.start,
                    _ => false,
                }
            }),
            parent: None,
        },
        Expect::Command { parent } if call_of(found, block, probe, working_set).is_none() => {
            SiteKind::Command {
                node: None,
                parent: *parent,
            }
        }
        Expect::FlagValue { .. }
        | Expect::Positional { .. }
        | Expect::Flag { .. }
        | Expect::Command { .. } => {
            let site = call_of(found, block, probe, working_set)?;
            let arguments = &site.call.arguments;
            let before = (arguments.iter())
                .take_while(|argument| argument.span().end <= cursor)
                .count();
            let at = (arguments.iter()).position(|argument| touches(argument.span(), cursor));
            let arg_slot = at.filter(|_| !span.is_empty()).unwrap_or(before);
            match &found.what {
                Expect::FlagValue { long, .. } => SiteKind::FlagValue {
                    site,
                    flag: (site.call.named_iter())
                        .rev()
                        .find(|(name, ..)| name.item == *long)
                        .map(|(name, short, _)| FlagRef::from_named(name, short.as_ref()))?,
                    arg_slot,
                },
                Expect::Positional { index, .. } => SiteKind::Positional {
                    site,
                    sig_positional: *index,
                    arg_slot,
                },
                Expect::Flag { .. } if !span.is_empty() => SiteKind::FlagName {
                    site,
                    sig_positional: count_positionals(site.call, arg_slot),
                    arg_slot,
                },
                // A gap where no positional is declared, or a word after a command that may name
                // a subcommand, is the next positional all the same.
                _ => {
                    let span = at.map_or(span, |at| arguments[at].span());
                    return Some((
                        SiteKind::Positional {
                            site,
                            sig_positional: count_positionals(site.call, arg_slot),
                            arg_slot,
                        },
                        span,
                    ));
                }
            }
        }
    };
    Some((kind, span))
}

/// What is under the cursor at `pos` in `line`, as the one suggestion it would be, for its
/// preview: a command, a flag or an argument of the call it is in, a variable, or a path that
/// is there. `None` for anything with nothing to show.
fn hovered(engine_state: &EngineState, line: &str, pos: usize) -> Option<Completions> {
    let mut working_set = StateWorkingSet::new(engine_state);
    let offset = working_set.next_span_start();
    let at = line.floor_char_boundary(pos.min(line.len()));
    let (block, probe) = parse_probed(&mut working_set, None, line.as_bytes(), at);
    let at = offset + at;
    let shapes = nu_parser::flatten_block(&working_set, &block);
    let (span, shape) = shapes
        .into_iter()
        .find(|(span, _)| span.start <= at && at <= span.end)?;
    let text = String::from_utf8_lossy(working_set.get_span_contents(span)).into_owned();
    // What the line declares itself is not the engine's yet.
    let known = |decl_id: DeclId| decl_id.get() < engine_state.num_decls();
    // The call the cursor is in an argument of.
    let call = probe
        .narrowest(false)
        .filter(|found| found.decl_id().is_some_and(known));
    let in_call = |slot: Slot| {
        let command = HelpFor::Decl(call?.decl_id()?);
        Some(Arc::new(HelpAt { command, slot }))
    };
    let (value, kind, help_at) = match shape {
        FlatShape::InternalCall(decl_id) if known(decl_id) => {
            let command_type = engine_state.get_decl(decl_id).command_type();
            let name = engine_state.get_decl(decl_id).name().to_owned();
            (
                name,
                SuggestionKind::Command(command_type, Some(decl_id)),
                None,
            )
        }
        FlatShape::External(_) => (
            text,
            SuggestionKind::Command(CommandType::External, None),
            None,
        ),
        FlatShape::Flag => (text, SuggestionKind::Flag, in_call(Slot::Flag)),
        FlatShape::Variable(_) => (text, SuggestionKind::Variable, None),
        _ => {
            let unquoted = text.trim_matches(|c| matches!(c, '\'' | '"' | '`'));
            let path = engine_state
                .cwd(None)
                .ok()
                .map(|cwd| nu_path::expand_path_with(unquoted, cwd, true));
            match path {
                Some(path) if path.is_dir() => {
                    (unquoted.to_owned(), SuggestionKind::Directory, None)
                }
                Some(path) if path.exists() => (unquoted.to_owned(), SuggestionKind::File, None),
                _ => {
                    let Expect::Positional { index, .. } = call?.what else {
                        return None;
                    };
                    let help_at = in_call(Slot::Positional(index));
                    (text, SuggestionKind::Value(Type::Any), help_at)
                }
            }
        }
    };
    let found = SemanticSuggestion {
        suggestion: Suggestion {
            value,
            span: reedline::Span::new(span.start - offset, span.end - offset),
            ..Suggestion::default()
        },
        kind: Some(kind),
        help_at,
        ..SemanticSuggestion::default()
    };
    let details = Details::of(std::slice::from_ref(&found))?;
    Some(Completions::new(vec![found.suggestion]).with_data(details))
}

/// Whether `call` already has a value for `signature`'s rest parameter before `cursor`.
fn rest_has_begun(call: &Call, signature: &Signature, cursor: usize) -> bool {
    let named = signature.required_positional.len() + signature.optional_positional.len();
    let given = (call.arguments.iter())
        .filter(|arg| !matches!(arg, Argument::Named(..)))
        .filter(|arg| arg.span().end < cursor)
        .count();
    given > named
}

/// Whether `item` was found by completing part of the line as a command line of its own.
fn is_delegated(item: &SemanticSuggestion) -> bool {
    item.source
        .as_deref()
        .is_some_and(|source| source.starts_with("delegated"))
}

/// The text typed up to the cursor: what the worker is asked to complete.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CompletionQuery {
    typed: Arc<str>,
    /// What follows the cursor on its line: the rest of the word at the cursor, which a
    /// suggestion replaces too, and what comes after it.
    after: Arc<str>,
    /// Nothing follows the cursor, so `typed` is the whole line and the highlighter's parse of
    /// the line is a parse of it.
    at_end: bool,
    /// The placeholder (`<branch>`) the cursor is in, when it is in one: `typed` ends where it
    /// starts, so that it is an empty token of the argument it stands for.
    placeholder: Option<Placeholder>,
    /// Whether the user asked, as Tab does, rather than the suggestions following the typing.
    asked: bool,
}

/// A placeholder that is completed whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Placeholder {
    /// Where it ends, in the line.
    end: usize,
    /// How far past `end` the next placeholder of the line ends, if there is one.
    next_end: Option<usize>,
}

impl CompletionQuery {
    fn new(line: &str, cursor: usize) -> Self {
        let floored = line.floor_char_boundary(cursor.min(line.len()));
        if let Some(found) = placeholder::at(line, floored) {
            let next_end = placeholder::find(&line[found.end..])
                .first()
                .map(|next| next.end);
            return Self {
                typed: Arc::from(&line[..found.start]),
                after: Arc::from(""),
                at_end: false,
                placeholder: Some(Placeholder {
                    end: found.end,
                    next_end,
                }),
                asked: true,
            };
        }
        let rest = &line[floored..];
        Self {
            typed: Arc::from(&line[..floored]),
            after: Arc::from(rest.split('\n').next().unwrap_or_default()),
            at_end: floored == line.len(),
            placeholder: None,
            asked: true,
        }
    }

    fn typed(&self) -> &str {
        &self.typed
    }

    fn cursor(&self) -> usize {
        self.typed.len()
    }

    /// `found` without the files that cannot be where the cursor is: in `src/li⌶/mod.rs` the
    /// word goes on with a separator, so only a directory fits.
    fn fit_what_follows(&self, found: &mut Vec<SemanticSuggestion>) {
        if !self.after.starts_with(std::path::is_separator) {
            return;
        }
        let cursor = self.cursor();
        found.retain(|item| {
            item.suggestion.span.end != cursor || item.kind != Some(SuggestionKind::File)
        });
    }

    /// `found` made to replace the whole word at the cursor rather than only what is typed of
    /// it before the cursor: the rest of a name or a path's component, or a whole placeholder.
    fn widen(&self, found: &mut [SemanticSuggestion]) {
        if self.placeholder.is_some() {
            return self.fill_placeholder(found);
        }
        let cursor = self.cursor();
        for item in found {
            let span = item.suggestion.span;
            let Some(replaced) = self.typed.get(span.start..cursor) else {
                continue;
            };
            if span.end == cursor {
                item.suggestion.span.end += taking::rest_of_word(&self.after, replaced, item);
            }
        }
    }

    /// The word at the cursor is not offered back to itself while the suggestions follow the
    /// typing: a suggestion that is what is typed changes nothing. Only that one goes: the
    /// longer names the word starts (`Cargo.toml.bak` beside `Cargo.toml`) stay, and what
    /// merely resembles the word still ranks below them. Asked for, by Tab, everything is offered.
    fn settle_on_a_whole_word(&self, found: &mut Vec<SemanticSuggestion>) {
        if self.asked {
            return;
        }
        let cursor = self.cursor();
        let whole = |item: &SemanticSuggestion| {
            item.fixes.is_none()
                && self.typed.get(item.suggestion.span.start..cursor)
                    == Some(item.suggestion.value.as_str())
        };
        if found.iter().any(whole) {
            // Only continuations of the whole word are worth showing beside it.
            let typed_word = |item: &SemanticSuggestion| {
                self.typed
                    .get(item.suggestion.span.start..cursor)
                    .is_some_and(|word| item.suggestion.value.starts_with(word))
            };
            found.retain(|item| !whole(item) && typed_word(item));
        }
    }

    /// `found` made to open the closure a command or flag that takes only one needs, once taken:
    /// `each` puts `each { ⌶ }` in the line, the cursor in its body.
    fn open_closures(&self, found: &mut [SemanticSuggestion], engine_state: &EngineState) {
        let cursor = self.cursor();
        for item in found {
            let suggestion = &item.suggestion;
            let Some(follows) = suggestion
                .span
                .end
                .checked_sub(cursor)
                .and_then(|past| self.after.get(past..))
            else {
                continue;
            };
            let Some((opening, at)) = taking::closure_opening(item, engine_state, follows) else {
                continue;
            };
            let suggestion = &mut item.suggestion;
            let name = std::mem::take(&mut suggestion.value);
            suggestion.cursor = Some(suggestion.span.start + name.len() + at);
            suggestion.value = format!("{name}{opening}");
            suggestion.display_override.get_or_insert(name);
        }
    }

    /// `found` made to complete the placeholder the query is at, if it is at one: each
    /// suggestion for the empty token replaces the whole placeholder, and the cursor then goes
    /// to the end of the next one.
    fn fill_placeholder(&self, found: &mut [SemanticSuggestion]) {
        let Some(placeholder) = self.placeholder else {
            return;
        };
        for item in found {
            let suggestion = &mut item.suggestion;
            if suggestion.span.end != self.cursor() {
                continue;
            }
            suggestion.span.end = placeholder.end;
            suggestion.cursor = placeholder
                .next_end
                .map(|next_end| suggestion.span.start + suggestion.value.len() + next_end);
        }
    }
}

/// The completion behaviour configured in `$env.config.completions`.
fn configured_options(config: &Config) -> CompletionOptions {
    CompletionOptions {
        case_sensitive: config.completions.case_sensitive,
        match_algorithm: config.completions.algorithm.into(),
        sort: config.completions.sort,
        match_description: false,
    }
}

fn isolated_stack(parent: Arc<Stack>, suppress_stdin: bool) -> Arc<Stack> {
    let stack = Stack::with_parent(parent)
        .reset_out_dest()
        .suppress_output()
        .collect_value();
    Arc::new(if suppress_stdin {
        stack.suppress_stdin()
    } else {
        stack
    })
}

/// Whether `command` carries the marker attribute `@marker`, seeing through aliases as execution
/// does.
pub(crate) fn has_marker(command: &dyn Command, marker: &str) -> bool {
    command
        .attributes()
        .iter()
        .any(|(name, value)| name == marker && value.as_bool().unwrap_or(false))
        || command
            .as_alias()
            .and_then(|alias| alias.command.as_deref())
            .is_some_and(|wrapped| has_marker(wrapped, marker))
}

/// Whether `decl` names an `@interactive` command.
pub(crate) fn decl_is_interactive(working_set: &StateWorkingSet, decl_id: DeclId) -> bool {
    has_marker(working_set.get_decl(decl_id), "interactive")
}

/// Whether the configured external completer closure runs an `@interactive` command. A closure
/// cannot carry the attribute itself, so we see through it to the command producing its result
/// — the terminal call of its block — exactly as a named completer carries the attribute.
pub(crate) fn closure_is_interactive(working_set: &StateWorkingSet, closure: &Closure) -> bool {
    let block = working_set.get_block(closure.block_id);
    matches!(
        block
            .pipelines
            .last()
            .and_then(|pipeline| pipeline.elements.last())
            .map(|element| &element.expr.expr),
        Some(Expr::Call(call)) if decl_is_interactive(working_set, call.decl_id)
    )
}

/// What the cursor is completing; each variant carries exactly the AST it needs.
#[derive(Debug, Clone, Default)]
pub(crate) enum SiteKind<'a> {
    /// A command name. `node` is the call it heads, if the parse has one, and `parent` the
    /// command whose subcommands it names, where it is no call yet (`export d⌶`).
    Command {
        node: Option<&'a Expression>,
        parent: Option<DeclId>,
    },
    /// A flag name being typed (`--`, `-x`). `sig_positional` and `arg_slot` are the
    /// positional the token binds to instead when the command is `def --wrapped`.
    FlagName {
        site: CallSite<'a>,
        sig_positional: usize,
        arg_slot: usize,
    },
    /// The value of a flag; `flag` keeps long/short identity.
    FlagValue {
        site: CallSite<'a>,
        flag: FlagRef<'a>,
        arg_slot: usize,
    },
    /// A positional argument; `sig_positional` indexes the signature.
    Positional {
        site: CallSite<'a>,
        sig_positional: usize,
        arg_slot: usize,
    },
    /// A binary-operator position trailing `lhs`.
    Operator { lhs: &'a Expression },
    /// The value after a comparison, `$row.name == ⌶`: what the column `members` of the rows the
    /// variable `head` names holds may be offered. To a user completer it is a variable
    /// position, as any expression awaiting an operand is.
    Operand {
        head: VarId,
        members: Vec<PathMember>,
        comparison: Comparison,
    },
    /// A cell path into `path`.
    CellPath { path: &'a FullCellPath },
    /// A `$var` name.
    Variable,
    /// An attribute name.
    AttributeName,
    /// The item an attribute block decorates.
    AttributableItem,
    /// An argument of a bare external call.
    ExternalArg { call: &'a Expression, index: usize },
    /// A file path: a redirection's target.
    File,
    /// A literal, a block or the like being written: nothing completes it, and no path is meant.
    /// To a user completer it is a file position, as it always was.
    #[default]
    Literal,
}

impl<'a> SiteKind<'a> {
    /// The [`ResolvedCursor`] a user completer sees.
    pub(crate) fn resolved(&self) -> ResolvedCursor<'a> {
        match *self {
            Self::Command { .. } => ResolvedCursor::Command,
            Self::FlagName { .. } => ResolvedCursor::FlagName,
            Self::FlagValue { flag, .. } => ResolvedCursor::FlagValue { flag: flag.name() },
            Self::Positional { sig_positional, .. } => ResolvedCursor::Positional {
                index: sig_positional,
            },
            Self::Operator { .. } => ResolvedCursor::Operator,
            Self::CellPath { .. } => ResolvedCursor::CellPath,
            Self::Variable | Self::Operand { .. } => ResolvedCursor::Variable,
            Self::AttributeName => ResolvedCursor::AttributeName,
            Self::AttributableItem => ResolvedCursor::AttributableItem,
            Self::ExternalArg { index, .. } => ResolvedCursor::ExternalArg { index },
            Self::File | Self::Literal => ResolvedCursor::File,
        }
    }

    /// The element this kind is backed by, if any.
    fn element(&self) -> Option<&'a Expression> {
        match *self {
            Self::Command { node, .. } => node,
            Self::FlagName { site, .. }
            | Self::FlagValue { site, .. }
            | Self::Positional { site, .. } => Some(site.element),
            Self::ExternalArg { call, .. } => Some(call),
            _ => None,
        }
    }

    /// The call this kind completes arguments for, if any.
    pub(crate) fn call_site(&self) -> Option<CallSite<'a>> {
        match *self {
            Self::FlagName { site, .. }
            | Self::FlagValue { site, .. }
            | Self::Positional { site, .. } => Some(site),
            _ => None,
        }
    }
}

/// What the cursor completes, as a user completer sees it in `cursor.kind`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ResolvedCursor<'a> {
    /// A command head.
    Command,
    /// A flag name.
    FlagName,
    /// The value of a flag; `flag` is the long name.
    FlagValue { flag: &'a str },
    /// A positional argument.
    Positional { index: usize },
    /// A binary-operator position.
    Operator,
    /// A cell path.
    CellPath,
    /// A `$var` name.
    Variable,
    /// An attribute name.
    AttributeName,
    /// The item an attribute block decorates.
    AttributableItem,
    /// An argument of a bare external call; externals have no signature, so flags count.
    ExternalArg { index: usize },
    /// A file path.
    #[default]
    File,
}

impl ResolvedCursor<'_> {
    /// The `cursor.kind` discriminant.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::FlagName => "flag-name",
            Self::FlagValue { .. } => "flag-value",
            Self::Positional { .. } => "positional",
            Self::Operator => "operator",
            Self::CellPath => "cell-path",
            Self::Variable => "variable",
            Self::AttributeName => "attribute-name",
            Self::AttributableItem => "attributable-item",
            Self::ExternalArg { .. } => "external-arg",
            Self::File => "file",
        }
    }

    /// The record a completer's `cursor` gets; the caller adds `token`/`byte`.
    pub(crate) fn into_record(self, span: Span) -> Record {
        let mut record = Record::new();
        record.insert("kind", Value::string(self.kind(), span));

        match self {
            Self::FlagValue { flag } => record.insert("flag", Value::string(flag, span)),
            Self::Positional { index } | Self::ExternalArg { index } => {
                record.insert("index", Value::int(index as i64, span))
            }
            _ => None,
        };

        record
    }
}

/// What the cursor is at, as a completer sees it: what it completes and the element it is in.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Level<'a> {
    /// What the cursor completes.
    pub cursor: ResolvedCursor<'a>,
    /// The element the cursor is in; the source of its tokens.
    pub element: Option<&'a Expression>,
}

/// A resolved completion site: kind, span, typed prefix, cursor, and what flows into it.
#[derive(Debug, Clone, Default)]
pub(crate) struct CompletionSite<'a> {
    pub kind: SiteKind<'a>,
    pub span: Span,
    pub typed_prefix: Cow<'a, str>,
    /// The cursor, in absolute working-set (span) coordinates.
    pub cursor: usize,
    /// What a completer is told the cursor is at.
    pub level: Level<'a>,
    /// The parameter of a call around whose condition or closure the cursor is in.
    pub around: Option<Arc<HelpAt>>,
    /// The pipeline elements the cursor is nested in, outermost first, with what flows into
    /// each: see [`piped`].
    pub nests: Vec<Nest<'a>>,
    /// What flows into the element the cursor is in, when anything does.
    pub piped: Option<Type>,
    /// The span of the pipeline elements before the one the cursor is in, which can be run to
    /// read the data that flows into it: see [`data`](super::data).
    pub prior: Option<Span>,
    /// The pipeline that `$in` at the start of [`Self::prior`] stands for.
    pub feed: Option<Span>,
    /// What the variable heading the cell path under the cursor stands for, where it names the
    /// rows piped in: `$in`, a closure's row, a condition's `$it`.
    pub rows: Option<Type>,
    /// The `let` and `const` statements the pipeline at [`Self::prior`] may read the variables of.
    pub prelude: Vec<Span>,
}

impl<'a> CompletionSite<'a> {
    /// Site whose replacement span contains the cursor.
    fn at(cursor: usize, kind: SiteKind<'a>, span: Span) -> Self {
        debug_assert!(touches(span, cursor), "{kind:?} at {span:?}, off {cursor}");
        Self {
            level: Level {
                cursor: kind.resolved(),
                element: kind.element(),
            },
            kind,
            span,
            ..Self::default()
        }
    }
}

/// What a source said of a request: a custom completer's `message`, or why one failed, a
/// delegated source included. Clones share the text.
#[derive(Clone, Default)]
pub(crate) struct Notice(Rc<RefCell<Said>>);

#[derive(Default)]
struct Said {
    message: Option<String>,
    failure: Option<String>,
}

impl Notice {
    /// Records `message`, which a completer chose to say; the last one said stands.
    pub(crate) fn say(&self, message: impl Into<String>) {
        self.0.borrow_mut().message = Some(message.into());
    }

    /// Records why a source failed; the last failure stands.
    pub(crate) fn fail(&self, failure: impl Into<String>) {
        self.0.borrow_mut().failure = Some(failure.into());
    }

    /// What was said since the last take, forgotten: a message, or else a failure.
    pub(crate) fn take(&self) -> Option<String> {
        self.take_beside(false)
    }

    /// [`Self::take`] for an answer that `offers` something or not: a failure says why there is
    /// nothing, and beside what other sources offered it is noise.
    pub(crate) fn take_beside(&self, offers: bool) -> Option<String> {
        let said = std::mem::take(&mut *self.0.borrow_mut());
        said.message.or(said.failure.filter(|_| !offers))
    }
}

/// Completions for one commandline against borrowed state.
pub struct CompletionEngine<'a> {
    engine_state: &'a EngineState,
    /// An isolated child of the caller's stack.
    stack: Arc<Stack>,
    options: CompletionOptions,
    /// Set while completing a line delegated from another; see [`delegation`].
    delegation: Option<Arc<Delegation>>,
    /// How much the user has used each command and argument, when the history is known.
    usage: Option<Arc<UsageIndex>>,
    /// What earlier requests found out: the executables on `PATH`, the entries of directories,
    /// what custom completers returned.
    cache: CacheView,
    /// The highlighter's parse of the line being edited, shared when the cursor is at its end.
    parse_cache: Option<LineParseCache>,
    /// What is found out while completing, when it is wanted: see [`Self::traced`].
    trace: Option<Trace>,
    /// Why there is nothing to suggest, when a source said so.
    notice: Notice,
}

/// What one dispatch completes: the engine's state, the commandline up to the cursor, and
/// once a site is resolved (see [`Context::at_site`]) the token the cursor is in.
#[derive(Clone, Copy)]
pub(crate) struct Context<'a> {
    pub working_set: &'a StateWorkingSet<'a>,
    pub stack: &'a Stack,
    pub options: &'a CompletionOptions,
    /// What the answer replaces, in working-set coordinates.
    pub span: Span,
    /// The text of the token up to the cursor, which the candidates are matched against.
    pub prefix: &'a [u8],
    /// Start of [`Self::buffer`] in working-set coordinates; `span - offset` indexes it.
    pub offset: usize,
    /// The commandline (or file) up to the cursor.
    pub buffer: &'a str,
    /// What the cursor is at, as a completer is told.
    pub level: Option<Level<'a>>,
    /// Set while the line is one delegated from another: where it is from, and what stands
    /// in for the local machine's paths and commands.
    pub delegation: Option<&'a Delegation>,
    /// What the history says about the commands and arguments the user runs, to break
    /// near-ties between candidates.
    pub usage: Option<&'a Arc<UsageIndex>>,
    /// What earlier requests found out; see [`CacheView`].
    pub cache: &'a CacheView,
    /// The rows whose columns the token may name: what is piped into a command that takes
    /// columns, or what the variable heading the token's cell path stands for.
    pub rows: Option<&'a Type>,
    /// What is piped into the command the token is in, which a completer reads as `place.input`
    /// and the commands that take it rank by.
    pub piped: Option<&'a Type>,
    /// The span of the pipeline before the element the token is in, when the token may name the
    /// columns or keys of its data (`get ⌶`) or a value in it, so that it is worth reading: see
    /// [`data`](super::data).
    pub prior: Option<Span>,
    /// The `let` and `const` statements before the cursor, in the blocks that enclose it,
    /// outermost first: the variables the pipeline at [`Self::prior`] may read, which
    /// [`data`](super::data) declares first when it runs it.
    pub prelude: &'a [Span],
    /// The pipeline that `$in` at the start of [`Self::prior`] stands for, when it is one.
    pub feed: Option<Span>,
    /// Set when the completion is being traced; see [`Trace`].
    pub trace: Option<&'a Trace>,
    /// Where a source says why it has nothing to suggest.
    pub notice: &'a Notice,
    /// The variables in scope at the cursor, as the parser saw them while their blocks were
    /// open; empty when the probe recorded none.
    pub scope: &'a [(Vec<u8>, VarId)],
}

impl<'a> Context<'a> {
    pub(crate) fn prefix_str(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.prefix)
    }

    /// The cursor in working-set (span) coordinates.
    fn absolute_cursor(&self) -> usize {
        self.offset + self.buffer.len()
    }

    /// A working-set offset as a byte index into [`Self::buffer`], saturating so
    /// spans before the buffer can't underflow.
    fn relative(&self, absolute: usize) -> usize {
        absolute.saturating_sub(self.offset)
    }

    /// The command the token is an argument of: the call the cursor is innermost in, by the name
    /// the history knows it by (`git checkout`, or `cargo` for an external).
    pub(crate) fn command_head(&self) -> Option<String> {
        let call = self.level?.element?;
        match &call.expr {
            Expr::Call(call) => Some(self.working_set.get_decl(call.decl_id).name().to_owned()),
            Expr::ExternalCall(head, _) => {
                let name = String::from_utf8_lossy(self.working_set.get_span_contents(head.span));
                Some(name.trim_start_matches(['^', '%']).to_owned())
            }
            _ => None,
        }
    }

    /// Attach a resolved site: its token, what flows into it, and what a completer is told
    /// the cursor is at.
    pub(crate) fn at_site(self, site: &'a CompletionSite<'a>) -> Self {
        Self {
            span: site.span,
            prefix: site.typed_prefix.as_bytes(),
            level: Some(site.level),
            piped: site.piped.as_ref(),
            prior: site.prior,
            prelude: &site.prelude,
            feed: site.feed,
            ..self
        }
    }

    /// Replace the whole token at `span`, including an untyped suffix in a parsed file, but
    /// match only the text before the cursor.
    pub(crate) fn token(self, span: Span) -> Self {
        let typed = Span::new(span.start, span.end.min(self.absolute_cursor()));
        Self {
            span,
            prefix: self.working_set.get_span_contents(typed),
            ..self
        }
    }
}

impl<'engine> CompletionEngine<'engine> {
    /// A completer for one synchronous query; only the stack is copied.
    pub fn new(engine_state: &'engine EngineState, stack: &Stack) -> Self {
        Self::isolated(engine_state, Arc::new(stack.clone()), false)
    }

    /// Like [`Self::new`], on a shared, stdin-suppressed stack.
    fn isolated(
        engine_state: &'engine EngineState,
        stack: Arc<Stack>,
        suppress_stdin: bool,
    ) -> Self {
        let stack = isolated_stack(stack, suppress_stdin);
        let options = configured_options(&stack.get_config(engine_state));
        Self {
            engine_state,
            stack,
            options,
            delegation: None,
            usage: None,
            cache: CacheView::disabled(),
            parse_cache: None,
            trace: None,
            notice: Notice::default(),
        }
    }

    /// The same engine, recording how it answers in `trace`. A traced engine never runs an
    /// `@interactive` completer, which needs the terminal the line editor holds.
    #[must_use]
    pub(crate) fn traced(mut self, trace: Trace) -> Self {
        self.trace = Some(trace);
        self
    }

    /// The same engine, ranking candidates by how much `usage` says the user has used them.
    #[must_use]
    pub(crate) fn with_usage(mut self, usage: Option<Arc<UsageIndex>>) -> Self {
        self.usage = usage;
        self
    }

    /// The same engine, sharing `parse_cache` for a line with nothing after the cursor.
    #[must_use]
    fn with_parse_cache(mut self, parse_cache: Option<LineParseCache>) -> Self {
        self.parse_cache = parse_cache;
        self
    }

    /// The same engine, remembering what it finds out in `cache`.
    #[must_use]
    fn with_cache(mut self, cache: CacheView) -> Self {
        self.cache = cache;
        self
    }

    /// The same engine, remembering what it finds out in `cache`, which is sized as the
    /// configuration says and valid for the environment the engine runs in: how the line editor
    /// keeps its own.
    #[must_use]
    pub(crate) fn caching_in(self, cache: &CompletionCache) -> Self {
        let size = self
            .stack
            .get_config(self.engine_state)
            .completions
            .cache_size;
        cache.set_capacity(size.try_into().unwrap_or(0));
        let env = CacheEnv::of(self.engine_state, &self.stack);
        self.with_cache(CacheView::new(cache.clone(), env))
    }

    /// An engine for a line delegated from the one `ctx` completes. The stack is `ctx`'s own,
    /// which is already isolated, with stdin suppressed where the original's was.
    pub(crate) fn delegated(ctx: &Context<'engine>, delegation: Delegation) -> Self {
        Self {
            engine_state: ctx.working_set.permanent_state,
            stack: Arc::new(ctx.stack.clone()),
            options: ctx.options.clone(),
            delegation: Some(Arc::new(delegation)),
            usage: ctx.usage.cloned(),
            cache: ctx.cache.clone(),
            parse_cache: None,
            trace: ctx.trace.cloned(),
            notice: ctx.notice.clone(),
        }
    }

    /// Complete `line`, which began at byte `from` of the line it was cut from, as a command
    /// line of its own naming a program: `sudo l` is not `let` and `sudo ls` is not Nushell's
    /// `ls`. `untyped` is the rest of the word the cursor is in, which a parsed file has past
    /// the cursor. What comes back is placed on the original line.
    pub(crate) fn complete_program_line(
        &self,
        line: &str,
        untyped: &str,
        from: usize,
    ) -> Vec<SemanticSuggestion> {
        let mut working_set = StateWorkingSet::new(self.engine_state);

        // A leading `^` makes the head an external command: only programs complete, and
        // Nushell's own commands of the same name do not take the call. It is not on the
        // original line, so the spans are moved back by its width.
        let remote = self
            .delegation
            .as_deref()
            .is_some_and(Delegation::is_remote);
        let force_program = !line.starts_with(['^', '%'])
            && (remote
                || line
                    .split_once(char::is_whitespace)
                    .is_none_or(|(head, _)| delegation::names_builtin(&working_set, head)));
        let program = if force_program { "^" } else { "" };

        let text = format!("{program}{line}{untyped}");
        let cursor = program.len() + line.len();
        let offset = working_set.next_span_start();
        let (block, probe) =
            parse_probed(&mut working_set, Some("completer"), text.as_bytes(), cursor);

        let mut found = self.fetch_completions_by_block(
            &block,
            &probe,
            self.context(&working_set, &text[..cursor], offset),
            &text,
        );
        // A span that started `n` bytes in now starts at `n - program + from`. Spans that began
        // inside the program land on `from`.
        let remap = |at: usize| at.saturating_sub(program.len()).saturating_add(from);
        for item in &mut found {
            let span = &mut item.suggestion.span;
            *span = reedline::Span::new(remap(span.start), remap(span.end));
        }
        found
    }

    /// Answer one request.
    fn answer(&self, query: &CompletionQuery) -> Completions {
        let found = self.finished(query);
        let line = format!("{}{}", query.typed(), query.after);
        let details = Details::of(&found).map(|details| self.with_usages(details, &found, &line));
        let suggestions: Suggestions = found
            .into_iter()
            .map(|semantic_suggestion| semantic_suggestion.suggestion)
            .collect();

        // What a source said of the line, a reason when there is nothing or a word beside what
        // there is (a host misspelt), goes on the hint line while nothing is selected.
        let message = self.notice.take_beside(!suggestions.is_empty());
        let completions = Completions::new(suggestions);
        let completions = match details {
            Some(details) => completions.with_data(details),
            None => completions,
        };
        match message {
            Some(message) => completions.with_message(message),
            None => completions,
        }
    }

    /// `details` with the usage of each command among `found`, for the hint line while it is
    /// selected. A command declared on the line itself is not the engine's yet, and has none, and
    /// one already written out on `line` has its usage on the status line already.
    fn with_usages(
        &self,
        mut details: Details,
        found: &[SemanticSuggestion],
        line: &str,
    ) -> Details {
        for (detail, item) in details.0.iter_mut().zip(found) {
            let span = item.suggestion.span;
            let written = line.get(span.start..span.end) == Some(item.suggestion.value.as_str());
            if let Some(SuggestionKind::Command(_, Some(decl_id))) = detail.kind
                && decl_id.get() < self.engine_state.num_decls()
                && !written
            {
                // What it takes: its name is on the row and its flags are a `-` away.
                let signature = self.engine_state.get_decl(decl_id).signature();
                let label = signature.usage();
                detail.usage = label
                    .positional_part(0)
                    .map(|first| label.parts[first..].join(" "));
            }
        }
        details
    }

    /// [`Self::dispatch_query`], each suggestion replacing the whole word at the cursor.
    fn complete_query(&self, query: &CompletionQuery) -> Vec<SemanticSuggestion> {
        self.notice.take();
        let mut found = self.dispatch_query(query);
        query.fit_what_follows(&mut found);
        query.widen(&mut found);
        found
    }

    /// Completes `query`, from the parse the highlighter already made of the line when the
    /// cursor is at its end, which is how a line is nearly always completed. The highlighter
    /// parses without loading modules, so a line it could not parse in full is parsed again here.
    fn dispatch_query(&self, query: &CompletionQuery) -> Vec<SemanticSuggestion> {
        if query.at_end
            && let Some(parse_cache) = &self.parse_cache
        {
            let parsed = parse_cache.parsed(self.engine_state, query.typed());
            if !parsed.skipped_module_load {
                let working_set = parsed.working_set(self.engine_state);
                return self.fetch_completions_by_block(
                    &parsed.block,
                    &parsed.probe,
                    self.context(&working_set, query.typed(), parsed.global_span_offset),
                    query.typed(),
                );
            }
        }
        self.probed(query.typed(), query.cursor(), |block, probe, context| {
            self.fetch_completions_by_block(block, probe, context, context.buffer)
        })
    }

    /// `then` given the parse of `line` up to the cursor at `position`, with the probe at its
    /// end, and a context over it.
    fn probed<T>(
        &self,
        line: &str,
        position: usize,
        then: impl FnOnce(&Block, &Probe, Context) -> T,
    ) -> T {
        let line = &line[..line.floor_char_boundary(position.min(line.len()))];
        let mut working_set = StateWorkingSet::new(self.engine_state);
        let offset = working_set.next_span_start();
        let (block, probe) = parse_probed(
            &mut working_set,
            Some("completer"),
            line.as_bytes(),
            line.len(),
        );
        then(&block, &probe, self.context(&working_set, line, offset))
    }

    pub fn fetch_completions_at(&self, line: &str, position: usize) -> Vec<SemanticSuggestion> {
        self.complete_query(&CompletionQuery::new(line, position))
    }

    /// [`Self::fetch_completions_at`] as the line editor is answered: a command that takes only
    /// a closure opens it once taken. Editors with snippets of their own, as through the
    /// language server, are not given these.
    pub(crate) fn fetch_completions_for_editor(
        &self,
        line: &str,
        position: usize,
    ) -> Vec<SemanticSuggestion> {
        self.finished(&CompletionQuery::new(line, position))
    }

    /// The suggestions for `query` as they are offered: an alias folded into the command it
    /// renames, and a command that takes only a closure opening it.
    fn finished(&self, query: &CompletionQuery) -> Vec<SemanticSuggestion> {
        let mut found = self.complete_query(query);
        command_completions::fold_renames(&mut found, &StateWorkingSet::new(self.engine_state));
        query.settle_on_a_whole_word(&mut found);
        query.open_closures(&mut found, self.engine_state);
        found
    }

    /// Input record for a completer, menu source, or `commandline complete --input`.
    /// Site identity (`$token`/`$place`) is always the parsed site; reedline's
    /// replacement range never enters here.
    pub fn completer_input_at(&self, line: &str, position: usize, wanted: DeclaredInputs) -> Value {
        self.input_at(line, position, wanted)
    }

    fn input_at(&self, line: &str, position: usize, wanted: DeclaredInputs) -> Value {
        self.probed(line, position, |block, probe, context| {
            let (site, _) = Self::resolve_completion_site(block, probe, &context, context.buffer);
            completer_input(&context.at_site(&site), wanted)
        })
    }

    /// Whether the completer that would run at `position` must run inline (`@interactive`)
    /// rather than on the background worker. Parses and resolves the site; runs no completer.
    pub(crate) fn interactive_completer_at(&self, line: &str, position: usize) -> bool {
        self.probed(line, position, |block, probe, context| {
            let (site, shorter) =
                Self::resolve_completion_site(block, probe, &context, context.buffer);
            [Some(site), shorter]
                .iter()
                .flatten()
                .any(|site| self.site_is_interactive(site, context.working_set))
        })
    }

    /// Whether the user completer that would run first at `site`, mirroring dispatch order, is
    /// `@interactive`: an argument's own completer runs before the command-wide one.
    fn site_is_interactive(&self, site: &CompletionSite, working_set: &StateWorkingSet) -> bool {
        // The configured external closure is read through to the command it dispatches to.
        let external = || {
            (self.external_completer())
                .is_some_and(|closure| closure_is_interactive(working_set, &closure))
        };
        let call = match (&site.kind, site.kind.call_site()) {
            (SiteKind::ExternalArg { .. }, _) => return external(),
            (_, Some(call_site)) => call_site.call,
            (_, None) => return false,
        };
        let signature = working_set.get_decl(call.decl_id).signature();
        let argument_completer = match &site.kind {
            SiteKind::FlagValue { flag, .. } => {
                find_flag(&signature, *flag).and_then(|flag| flag.completion)
            }
            SiteKind::Positional { sig_positional, .. } => signature
                .get_positional(*sig_positional)
                .and_then(|positional| positional.completion.clone()),
            // Dispatch also runs the positional completer here for `def --wrapped`.
            SiteKind::FlagName { sig_positional, .. } if signature.allows_unknown_args => signature
                .get_positional(*sig_positional)
                .and_then(|positional| positional.completion.clone()),
            _ => None,
        };
        match (argument_completer, signature.complete) {
            (Some(Completion::Command(decl_id)), _)
            | (_, Some(CommandWideCompleter::Command(decl_id))) => {
                decl_is_interactive(working_set, decl_id)
            }
            (_, Some(CommandWideCompleter::External)) => external(),
            (_, None) => false,
        }
    }

    pub fn fetch_completions_within_file(
        &self,
        filename: &str,
        position: usize,
        contents: &str,
    ) -> Vec<SemanticSuggestion> {
        let mut working_set = StateWorkingSet::new(self.engine_state);

        let cursor = contents.floor_char_boundary(position.min(contents.len()));
        // The parse must run first: it registers the file and its spans in `working_set`.
        let (block, mut probe) = parse_probed(
            &mut working_set,
            Some(filename),
            contents.as_bytes(),
            cursor,
        );
        // In an editor what follows the cursor is not being written: after a space the cursor
        // inserts before the next word, and in a `$` word what follows it is left as it is.
        let before = &contents[..cursor];
        let gap = before.chars().next_back().is_none_or(char::is_whitespace);
        let variable = (before.rsplit(|c: char| c.is_whitespace() || "|(){}[]<>,:;".contains(c)))
            .next()
            .is_some_and(|word| word.starts_with('$'));
        let at = probe.at;
        for found in &mut probe.found {
            if gap && found.span.start == at {
                found.span = Span::new(at, at);
            } else if variable {
                found.span.end = found.span.end.clamp(found.span.start, at);
            }
        }

        let Some(file_span) = working_set.get_span_for_filename(filename) else {
            return Vec::new();
        };

        self.fetch_completions_by_block(
            &block,
            &probe,
            self.context(&working_set, &contents[..cursor], file_span.start),
            contents,
        )
    }

    /// `context` holds the commandline; `contents` is the text the block was parsed from, with
    /// `probe` at the cursor.
    fn fetch_completions_by_block(
        &self,
        block: &Block,
        probe: &Probe,
        context: Context,
        contents: &str,
    ) -> Vec<SemanticSuggestion> {
        let context = Context {
            scope: &probe.scope,
            ..context
        };
        let (site, shorter) = Self::resolve_completion_site(block, probe, &context, contents);
        if let Some(helped) =
            Self::help_from_parse(probe, &site, context).filter(|found| !found.is_empty())
        {
            return helped;
        }
        let flowing =
            (site.piped.clone()).or_else(|| piped::block_input(context.working_set, &site.nests));
        // In a file, what comes after the cursor is no part of what is open at it.
        let before = contents.get(..context.relative(site.cursor));
        if let Some(helped) = before.and_then(|typed| {
            super::records::help(
                block,
                probe,
                typed,
                flowing.as_ref(),
                &context.at_site(&site),
            )
        }) {
            return helped;
        }
        let dispatched = self.dispatch_completion_site(&site, context);
        // A name of several words is also a shorter command and its arguments: the argument it
        // reads as is offered too, ahead of the commands.
        let mut found = shorter.map_or_else(Vec::new, |shorter| {
            let mut found = self.dispatch_completion_site(&shorter, context);
            found.retain(|candidate| {
                let command = matches!(candidate.kind, Some(SuggestionKind::Command(..)));
                if let (true, Some(trace)) = (command, context.trace) {
                    trace.dropped(
                        candidate.source.as_deref(),
                        candidate.suggestion.value.clone(),
                        "a command: the shorter reading of the head completes an argument",
                    );
                }
                !command
            });
            found
        });
        found.extend(dispatched);
        found
    }

    /// What the parse says the cursor calls for beyond its site: the parser's fix for a word
    /// written as another language writes it, before the cursor in the construct the cursor is
    /// in, offered alone until it is put right (`where size = 1b ⌶` offers `==` for the `=`); and
    /// the spans of time a date is moved by (`(date now) - ⌶`).
    fn help_from_parse(
        probe: &Probe,
        site: &CompletionSite,
        context: Context,
    ) -> Option<Vec<SemanticSuggestion>> {
        let working_set = context.working_set;
        let in_construct =
            |span: Span| (probe.found.iter()).any(|found| found.within.contains_span(span));
        let fix = (working_set.parse_errors.iter())
            .filter_map(nu_protocol::ParseError::fix)
            .find(|fix| fix.span.end < site.cursor && in_construct(fix.span));
        if let Some((fix, meant)) =
            fix.and_then(|fix| Some((fix.clone(), *fix.replacements.first()?)))
        {
            let written = String::from_utf8_lossy(working_set.get_span_contents(fix.span));
            let grows = meant.len() as isize - written.len() as isize;
            return Some(vec![SemanticSuggestion {
                suggestion: Suggestion {
                    value: meant.to_owned(),
                    span: to_reedline_span(fix.span, context.offset),
                    cursor: context.relative(site.cursor).checked_add_signed(grows),
                    ..Suggestion::default()
                },
                kind: Some(SuggestionKind::Operator),
                source: Some(Cow::Borrowed("what is meant")),
                fixes: Some(written.into_owned()),
                ..SemanticSuggestion::default()
            }]);
        }
        // A number and its unit written apart (`1 kb`) are one value, as the parser reads it.
        let apart = (working_set.parse_errors.iter()).find_map(|error| {
            let unit = error.span();
            if !(in_construct(unit) && unit.end < site.cursor && unit.start > probe.source.start) {
                return None;
            }
            let before = working_set.get_span_contents(Span::new(probe.source.start, unit.start));
            let digits = before.strip_suffix(b" ")?;
            let digits = digits
                .iter()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .count();
            (digits > 0).then(|| Span::new(unit.start - 1 - digits, unit.end))
        });
        if let Some(span) = apart {
            let joined: String = String::from_utf8_lossy(working_set.get_span_contents(span))
                .split_whitespace()
                .collect();
            let mut parse = StateWorkingSet::new(working_set.permanent_state);
            let block = nu_parser::parse(&mut parse, None, joined.as_bytes(), false);
            let is_unit_value = (block.pipelines.first())
                .and_then(|pipeline| pipeline.elements.first())
                .is_some_and(|element| matches!(element.expr.expr, Expr::ValueWithUnit(_)));
            if is_unit_value && parse.parse_errors.is_empty() {
                return Some(vec![SemanticSuggestion {
                    suggestion: Suggestion {
                        value: joined,
                        span: to_reedline_span(span, context.offset),
                        cursor: context.relative(site.cursor).checked_sub(1),
                        ..Suggestion::default()
                    },
                    source: Some(Cow::Borrowed("what is meant")),
                    ..SemanticSuggestion::default()
                }]);
            }
        }
        let moves_a_date = probe.innermost().any(|found| {
            matches!(&found.what, Expect::Operand { lhs, operator: Operator::Math(nu_protocol::ast::Math::Add | nu_protocol::ast::Math::Subtract) } if lhs.ty == Type::Date)
        });
        moves_a_date.then(|| ask([Source::Durations], &context.at_site(site).token(site.span)))
    }

    /// The suggestions at `site`.
    fn dispatch_completion_site<'a>(
        &'a self,
        site: &'a CompletionSite<'a>,
        context: Context<'a>,
    ) -> Vec<SemanticSuggestion> {
        let context = context.at_site(site);
        if let Some(trace) = context.trace {
            trace.reading(site, &context);
        }

        match &site.kind {
            SiteKind::Command { node, parent } => {
                // A cursor before a leading `^`/`%` is outside the command name.
                // Inserting a name there would put it in front of the sigil.
                if node.is_some_and(|node| match &node.expr {
                    Expr::Call(call) => site.cursor < call.head.start,
                    Expr::ExternalCall(head, _) => site.cursor < head.span.start,
                    _ => false,
                }) {
                    return Vec::new();
                }
                let mut found = ask(
                    [Source::Commands {
                        scope: self.command_scope_for_head(*node, site.span, context.working_set),
                        quoted: false,
                        parent: parent.map(|parent| context.working_set.get_decl(parent).name()),
                    }],
                    &context.token(site.span),
                );
                // What is piped in, for a preview of each command at work on it.
                if let Some(prior) = site.prior {
                    let input: Arc<str> =
                        String::from_utf8_lossy(context.working_set.get_span_contents(prior))
                            .trim()
                            .into();
                    for item in &mut found {
                        item.input = Some(Arc::clone(&input));
                    }
                }

                // A command group (`export zzz`) has no files to fall back on, and a word that is
                // no path is no program to run from one: only `./run`, `~/bin/run` and `/bin/ls`
                // are, as the shell runs them.
                let group =
                    parent.is_some() || node.is_some_and(|node| matches!(node.expr, Expr::Garbage));
                let typed = String::from_utf8_lossy(context.prefix);
                let path_like = typed
                    .trim_start_matches(['"', '\'', '`', '^'])
                    .starts_with(['.', '~'])
                    || typed.contains(std::path::is_separator);
                if found.is_empty() && !group && path_like {
                    ask_fallback([Source::Paths { directories: false }], &context)
                } else {
                    found
                }
            }

            SiteKind::FlagName { .. }
            | SiteKind::FlagValue { .. }
            | SiteKind::Positional { .. } => self.dispatch_call_completion_site(site, &context),

            SiteKind::Operator { lhs } => ask(
                [Source::Operators(lhs)],
                &Context {
                    rows: site.rows.as_ref(),
                    ..context
                },
            ),

            SiteKind::Operand {
                members,
                comparison,
                ..
            } => ask(
                [Source::Operand {
                    members: members.clone(),
                    operator: *comparison,
                }],
                &context,
            ),

            SiteKind::CellPath { path } => {
                let mut found = ask(
                    [Source::CellPath {
                        path,
                        cursor: site.cursor,
                    }],
                    &Context {
                        rows: site.rows.as_ref(),
                        ..context
                    },
                );
                // A column named in a condition or closure of a call (`where size`) has the
                // call's help for that parameter.
                if let Some(at) = &site.around {
                    for item in &mut found {
                        item.help_at.get_or_insert_with(|| Arc::clone(at));
                    }
                }
                found
            }

            SiteKind::Variable if context.prefix.starts_with(b"$") => {
                ask([Source::Variables], &context)
            }
            SiteKind::Variable => Vec::new(),

            SiteKind::AttributeName => ask([Source::Attributes], &context),

            SiteKind::AttributableItem => ask([Source::Attributables], &context),

            SiteKind::ExternalArg { call, .. } => {
                // A program that is no command at all is most likely a slip: the command meant is
                // the answer, in its place, wherever the cursor is on its line. What its
                // arguments would complete to is for a line that runs.
                let meant = match &call.expr {
                    Expr::ExternalCall(head, _) => self.command_meant(head, &context),
                    _ => None,
                };
                match meant {
                    Some(meant) => vec![meant],
                    None => self.dispatch_external_arg(site, &context),
                }
            }

            SiteKind::File => ask([Source::Paths { directories: false }], &context),

            SiteKind::Literal => Vec::new(),
        }
    }

    /// Complete an external call argument: its wrapped command line, or else the configured
    /// external completer, then files unless that answers or subcommands extend the call.
    /// The command meant by the head `head` of an external call, when it names no command,
    /// internal or on `PATH`, nor a path: the best match for it, put in its place.
    fn command_meant(&self, head: &Expression, context: &Context) -> Option<SemanticSuggestion> {
        let program = String::from_utf8_lossy(context.working_set.get_span_contents(head.span));
        let program = program.trim_start_matches('^');
        // A line for another machine knows its commands only by asking it.
        let known = context.delegation.is_some()
            || program.contains(std::path::is_separator)
            || program.is_empty()
            || WRAPPERS.contains(&program)
            || context.working_set.find_decl(program.as_bytes()).is_some()
            || (executable_files(context).iter()).any(|(name, _)| name == program);
        if known || head.span.start >= head.span.end {
            return None;
        }
        let found = ask(
            [Source::Commands {
                scope: CommandScope::All,
                quoted: false,
                parent: None,
            }],
            &context.token(head.span),
        );
        // A slip is close to what was meant: two edits at most, a swap counting as one, case
        // aside.
        let letters = |text: &str| -> Vec<String> {
            use unicode_segmentation::UnicodeSegmentation;
            text.to_lowercase()
                .graphemes(true)
                .map(str::to_owned)
                .collect()
        };
        let typed = letters(program);
        let mut meant = found.into_iter().find(|item| {
            let value = &item.suggestion.value;
            value != program
                && !value.contains(' ')
                && super::rank::damerau_levenshtein(&typed, &letters(value)) <= 2
        })?;
        // The arguments after it are kept as they are, and the cursor stays where it is on them.
        meant.suggestion.append_whitespace = false;
        let grows = meant.suggestion.value.len() as isize - program.len() as isize;
        meant.suggestion.cursor = context
            .relative(context.absolute_cursor())
            .checked_add_signed(grows);
        meant.source = Some(Cow::Borrowed("the command meant"));
        meant.fixes = Some(program.to_owned());
        Some(meant)
    }

    fn dispatch_external_arg(
        &self,
        site: &CompletionSite,
        context: &Context,
    ) -> Vec<SemanticSuggestion> {
        let SiteKind::ExternalArg {
            call: external_call,
            index,
        } = &site.kind
        else {
            return Vec::new();
        };
        let Expr::ExternalCall(head, arguments) = &external_call.expr else {
            return Vec::new();
        };
        let working_set = context.working_set;

        // What `sudo` runs completes as a line of its own, from the first argument unless that
        // is one of its options.
        let program = String::from_utf8_lossy(working_set.get_span_contents(head.span));
        let first = match index {
            0 => site.span,
            _ => arguments
                .first()
                .map_or(site.span, |argument| argument.expr().span),
        };
        if WRAPPERS.contains(&program.trim_start_matches('^'))
            && !working_set.get_span_contents(first).starts_with(b"-")
            && let Some(wrapped) = delegation::complete(
                context,
                first.start.saturating_sub(context.offset),
                Sources::default(),
            )
        {
            return wrapped;
        }

        // Subcommands extending this call suppress the file fallback.
        let subcommands =
            self.subcommand_suggestions(context, external_call.span.start, site.cursor, None);

        let mut sources = Vec::new();
        // The user's configured external completer, which answers for this machine.
        if let Some(completer) = self
            .external_completer()
            .filter(|_| !context.delegation.is_some_and(Delegation::is_remote))
        {
            sources.push(Source::Custom(UserCompletion::closure(
                working_set,
                &completer,
            )));
        }
        // Files, when the source leaves the slot open.
        if subcommands.is_empty() {
            sources.push(Source::Paths { directories: false });
        }

        let mut found = ask(sources, context);
        // What a completer lists after the program's name may name its subcommands.
        match index {
            0 => link_subcommands(
                &mut found,
                subcommands,
                program.trim_start_matches('^'),
                working_set,
            ),
            _ => found.extend(subcommands),
        }
        let at = Arc::new(HelpAt {
            command: HelpFor::External(program.trim_start_matches('^').to_owned()),
            slot: Slot::Argument,
        });
        for item in &mut found {
            if !matches!(item.kind, Some(SuggestionKind::Command(..))) {
                item.help_at.get_or_insert_with(|| Arc::clone(&at));
            }
        }
        found
    }

    /// Dispatch completions for call-bound sites.
    fn dispatch_call_completion_site(
        &self,
        site: &CompletionSite,
        context: &Context,
    ) -> Vec<SemanticSuggestion> {
        // Only call-bound kinds carry a call; anything else is an error here.
        let call = match &site.kind {
            SiteKind::FlagName { site, .. }
            | SiteKind::FlagValue { site, .. }
            | SiteKind::Positional { site, .. } => site.call,
            _ => return Vec::new(),
        };

        let declaration = context.working_set.get_decl(call.decl_id);
        // An argument that names columns names those of what is piped into the call.
        let context = &Context {
            rows: (site.piped.as_ref()).filter(|ty| !piped::is_unknown(ty)),
            ..*context
        };

        let signature = declaration.signature();

        // Subcommands are offered and suppress the file-path fallback, but not in a gap the
        // command's own argument is still needed for: `each ⌶` takes a closure, though
        // `each while` begins the same way.
        let given = (call.positional_iter())
            .filter(|arg| arg.span.end <= site.cursor)
            .count();
        let needs_argument = site.span.is_empty() && given < signature.required_positional.len();
        let subcommands = match needs_argument {
            true => Vec::new(),
            false => self.subcommand_suggestions(
                context,
                call.head.start,
                site.cursor,
                Some(declaration.name()),
            ),
        };

        // Value kinds share one shape; only the `ArgType`, completer lookup, and the
        // `declared_shape` used for the empty-slot fallback differ.
        let argument_value = |arg_type, custom, arg_idx, declared_shape| {
            ask(
                self.value_sources(
                    custom,
                    ArgValueCompletion {
                        call,
                        arg_type,
                        arg_idx,
                        declared_shape,
                        cursor: site.cursor,
                    },
                    context,
                    &signature,
                    subcommands.is_empty(),
                ),
                context,
            )
        };

        let mut found = match &site.kind {
            // A word like a flag that comes after the rest of the arguments has begun belongs to
            // what they are, when the rest is a command line of its own: `ssh host ls -l`. The
            // rest's completer says so by delegating it.
            SiteKind::FlagName {
                sig_positional,
                arg_slot,
                ..
            } if let Some(rest) = signature
                .rest_positional
                .as_ref()
                .filter(|rest| rest.completion.is_some())
                .filter(|_| rest_has_begun(call, &signature, site.cursor))
                && let Some(delegated) = Some(argument_value(
                    ArgType::Positional(*sig_positional),
                    rest.completion.clone(),
                    *arg_slot,
                    Some(rest.shape.clone()),
                ))
                .filter(|answered| answered.iter().any(is_delegated)) =>
            {
                delegated
            }
            SiteKind::FlagName {
                sig_positional,
                arg_slot,
                ..
            } => match signature
                .get_positional(*sig_positional)
                .filter(|positional| {
                    signature.allows_unknown_args && positional.completion.is_some()
                }) {
                // `def --wrapped` binds an unknown `--x` to a positional, so its declared
                // completer answers next to the flags (#19097); it already falls back to
                // the command-wide one. An undeclared one would only list files.
                Some(positional) => {
                    let mut flags = ask([Source::Flags(call.decl_id)], context);
                    flags.extend(argument_value(
                        ArgType::Positional(*sig_positional),
                        positional.completion.clone(),
                        *arg_slot,
                        Some(positional.shape.clone()),
                    ));
                    flags
                }
                None => self.complete_flag_names(call.decl_id, context, &signature),
            },
            SiteKind::FlagValue { flag, arg_slot, .. } => {
                let resolved = find_flag(&signature, *flag);
                argument_value(
                    ArgType::Flag(Cow::Borrowed(flag.name())),
                    resolved.as_ref().and_then(|flag| flag.completion.clone()),
                    *arg_slot,
                    resolved.as_ref().and_then(|flag| flag.arg.clone()),
                )
            }
            SiteKind::Positional {
                sig_positional,
                arg_slot,
                ..
            } => {
                let positional = signature.get_positional(*sig_positional);
                argument_value(
                    ArgType::Positional(*sig_positional),
                    positional.and_then(|positional| positional.completion.clone()),
                    *arg_slot,
                    positional.map(|positional| positional.shape.clone()),
                )
            }
            _ => Vec::new(),
        };

        leave_out_what_is_written(
            &mut found,
            site,
            call,
            &signature,
            context.working_set,
            context.offset,
        );
        link_subcommands(
            &mut found,
            subcommands,
            declaration.name(),
            context.working_set,
        );
        // Whatever is offered here sits in a flag or a parameter of the call, which its
        // preview has help for. (A subcommand is a command, with help of its own.)
        let slot = match &site.kind {
            SiteKind::FlagName { .. } => Some(Slot::Flag),
            SiteKind::FlagValue { flag, .. } => Some(Slot::FlagValue(flag.name().to_owned())),
            SiteKind::Positional { sig_positional, .. } => Some(Slot::Positional(*sig_positional)),
            _ => None,
        };
        if let Some(slot) = slot {
            let at = Arc::new(HelpAt {
                command: HelpFor::Decl(call.decl_id),
                slot,
            });
            for item in &mut found {
                if !matches!(item.kind, Some(SuggestionKind::Command(_, Some(_)))) {
                    item.help_at.get_or_insert_with(|| Arc::clone(&at));
                }
            }
        }
        found
    }

    /// The site the cursor is at, as the parser's probe of the line reads it, with what flows
    /// into the element it is in; and the shorter reading of a name of several words, as an
    /// argument of a shorter command, if the name has one.
    pub(crate) fn resolve_completion_site<'a>(
        block: &'a Block,
        probe: &'a Probe,
        context: &Context<'a>,
        contents: &'a str,
    ) -> (CompletionSite<'a>, Option<CompletionSite<'a>>) {
        let working_set = context.working_set;
        let cursor = context.absolute_cursor();
        let (shorter, found): (Vec<_>, Vec<_>) = probe.innermost().partition(|found| {
            !probe.readings.is_empty() && !matches!(found.what, Expect::Command { .. })
        });

        let (kind, span) = kind_of(&found, probe, block, working_set, cursor);
        let mut site = CompletionSite::at(cursor, kind, span);
        if let SiteKind::CellPath { .. } = site.kind {
            site.around = help_around(probe, block, working_set, cursor);
        }
        // To a completer, a comparison in a row condition is the condition the call takes.
        if let SiteKind::Operand { .. } = site.kind
            && let Some((call, index, _)) =
                (found.first()).and_then(|inner| condition_around(inner, probe, block, working_set))
        {
            site.level = Level {
                cursor: ResolvedCursor::Positional { index },
                element: Some(call.element),
            };
        }

        // The closures and subexpressions the cursor is nested in, outermost first.
        let chain = enclosing_elements(block, working_set, cursor);
        let flow = Flow::new(working_set, context.stack);
        site.nests = nests_of(&chain, &flow);
        let own = site.kind.element().and_then(|element| {
            // An element that uses `$in` is collected, and its nest is the collect around it.
            (site.nests.iter()).position(|nest| {
                std::ptr::eq(nest.element, element)
                    || matches!(&nest.element.expr, Expr::Collect(_, inner) if std::ptr::eq(inner.as_ref(), element))
            })
        });
        if let Some(at) = own {
            let nest = &site.nests[at];
            // What flows into the first element of a block is what flows into the call that
            // runs it.
            site.piped =
                (nest.input.clone()).or_else(|| piped::block_input(working_set, &site.nests[..at]));
            (site.prior, site.feed) = (nest.prior.or(nest.feed), nest.feed);
        }
        site.rows = Self::head_rows(&site, working_set);
        // The row a cell path starts with is read from the data its condition or closure is run on.
        if let Some(source) = Self::head_prior(&site) {
            site.prior = Some(source.span);
            site.feed = source.feed;
        }
        site.prelude = Self::prelude_of(&chain, working_set);
        if let SiteKind::Command { node: None, .. } = site.kind {
            // A command begun in a closure, a block or a subexpression is the first element
            // of the block, or one after a pipe in it.
            let nested = chain.last().and_then(|&(element, ..)| {
                let nested = descent_expression(element, working_set, cursor)?;
                match nested.expr {
                    Expr::Closure(id) | Expr::Block(id) | Expr::Subexpression(id) => {
                        Some(working_set.get_block(id).as_ref())
                    }
                    _ => None,
                }
            });
            let flow = flow.within_nests(&site.nests);
            site.piped = after_pipe(nested.unwrap_or(block), working_set, cursor, &flow);
            // Nothing before the cursor in a block: what flows into the block flows into it.
            if nested.is_some() && site.piped.is_none() {
                site.piped = piped::block_input(working_set, &site.nests);
            }
        }
        let shorter = (!shorter.is_empty())
            .then(|| kind_of(&shorter, probe, block, working_set, cursor))
            .filter(|(kind, _)| !matches!(kind, SiteKind::Command { .. } | SiteKind::Literal))
            .map(|(kind, span)| {
                let mut shorter = CompletionSite::at(cursor, kind, span);
                shorter.piped = site.piped.clone();
                Self::finalize_site(shorter, context, contents)
            });
        (Self::finalize_site(site, context, contents), shorter)
    }

    /// The variable heading the cell path of `site`.
    fn head_var(site: &CompletionSite) -> Option<VarId> {
        let path = match &site.kind {
            SiteKind::Operand { head, .. } => return Some(*head),
            SiteKind::CellPath { path } => path,
            SiteKind::Operator { lhs } => match &lhs.expr {
                Expr::FullCellPath(path) => path.as_ref(),
                _ => return None,
            },
            _ => return None,
        };
        match path.head.expr {
            Expr::Var(var_id) => Some(var_id),
            _ => None,
        }
    }

    /// What the variable heading the cell path of `site` stands for, if it names the rows piped in.
    fn head_rows(site: &CompletionSite, working_set: &StateWorkingSet) -> Option<Type> {
        let var_id = Self::head_var(site)?;
        let rows = piped::rows_named(working_set, &site.nests, var_id)
            .filter(|rows| !piped::is_unknown(rows))?;
        // `$in` in the condition of `where` is a row of what is piped in, as a parameter is.
        let of_a_condition = site.nests.iter().any(|nest| {
            matches!(&nest.element.expr, Expr::Collect(id, inner)
                if (*id == var_id || var_id == nu_protocol::IN_VARIABLE_ID)
                    && matches!(&inner.expr, Expr::Call(call)
                        if working_set.get_decl(call.decl_id).name() == "where"))
        });
        Some(if of_a_condition {
            piped::row_of(&rows)
        } else {
            rows
        })
    }

    /// The pipeline whose rows the variable heading the cell path of `site` stands for, when the
    /// condition or closure it is a parameter of is run on each of them.
    fn head_prior(site: &CompletionSite) -> Option<piped::Source> {
        piped::rows_source(&site.nests, Self::head_var(site)?)
    }

    /// The `let` and `const` statements that come before the elements of `chain`, each in its own
    /// block, outermost first: the variables the pipelines the cursor is nested in may read.
    fn prelude_of(chain: &[Enclosing], working_set: &StateWorkingSet) -> Vec<Span> {
        /// How many statements are kept, the nearest ones.
        const KEPT: usize = 16;
        let mut statements = Vec::new();
        for &(element, _, block) in chain {
            for pipeline in &block.pipelines {
                if (pipeline.elements.iter())
                    .any(|candidate| std::ptr::eq(&candidate.expr, element))
                {
                    break;
                }
                if let [only] = pipeline.elements.as_slice()
                    && only.redirection.is_none()
                    && let Expr::Call(call) = &only.expr.expr
                    && piped::is_binding(working_set, call)
                {
                    statements.push(only.expr.span);
                }
            }
        }
        statements.split_off(statements.len().saturating_sub(KEPT))
    }

    /// Fill `typed_prefix`/`cursor` from the final span so they never disagree.
    fn finalize_site<'a>(
        mut site: CompletionSite<'a>,
        context: &Context,
        contents: &'a str,
    ) -> CompletionSite<'a> {
        let token_start = context.relative(site.span.start);
        site.typed_prefix = contents
            .get(token_start..context.buffer.len())
            .map(Cow::Borrowed)
            .unwrap_or(Cow::Borrowed(""));
        site.cursor = context.absolute_cursor();
        site
    }

    /// The sources that complete the value in `slot`, in the order they are asked: the completer
    /// the argument declares, the command-wide one, a plugin's, and the files its type calls for.
    /// `need_fallback` allows files for an untyped argument, see [`ArgValueCompletion::path_source`].
    fn value_sources<'a>(
        &self,
        custom: Option<Completion>,
        slot: ArgValueCompletion<'a>,
        context: &Context<'a>,
        signature: &Signature,
        need_fallback: bool,
    ) -> Vec<Source<'a>> {
        let working_set = context.working_set;
        let mut sources = Vec::new();

        match custom {
            Some(Completion::Builtin(kind)) => {
                self.builtin_sources(kind, &slot, context, &mut sources)
            }
            // A user-defined completer, called with the unified input record.
            Some(Completion::Command(decl_id)) => {
                sources.push(match UserCompletion::parameter(working_set, decl_id) {
                    Some(completer) => Source::Custom(completer),
                    None => {
                        // A builtin/plugin runs no block; empty beats dumping the directory.
                        log::error!(
                            "`{}` cannot be used as a completer: it runs no block",
                            working_set.get_decl(decl_id).name()
                        );
                        Source::Nothing
                    }
                });
            }
            Some(Completion::List(list)) => sources.push(Source::List(list)),
            None => {}
        }

        sources.extend(
            self.command_wide(signature, working_set)
                .map(Source::CommandWide),
        );

        // Command-specific completions come from the argument's `BuiltinCompletion` above;
        // here are the generic argument-value fallbacks.
        // An untyped slot of a command the user declared may hold a path (a builtin's `any` is a
        // value: `echo`, `append`, `http post`'s body); past the signature only a command that
        // takes unknown arguments has one. A column is never a path.
        let columns = slot.names_columns();
        let declared_by_user = matches!(
            working_set.get_decl(slot.call.decl_id).command_type(),
            CommandType::Custom | CommandType::External | CommandType::Alias
        );
        let files = slot.path_source(
            need_fallback
                && declared_by_user
                && (slot.declared_shape.is_some() || signature.allows_unknown_args),
        );
        // A call goes on with the keyword its next parameter starts with: `try`'s `catch` and
        // `finally`, `if`'s `else`.
        let given = (slot.call.positional_iter())
            .filter(|arg| arg.span.end < slot.cursor && !matches!(arg.expr, Expr::Garbage))
            .count();
        let keywords = keywords_of(signature.get_positional(given).map(|arg| &arg.shape));
        if !keywords.is_empty() {
            sources.push(Source::List(NuCow::Owned(keywords)));
        }
        sources.push(Source::Dynamic(slot));
        if columns {
            sources.push(Source::Columns);
        } else {
            sources.extend(files);
        }
        sources
    }

    /// The sources of a [`BuiltinCompletion`] a command declared for its argument.
    fn builtin_sources<'a>(
        &self,
        kind: BuiltinCompletion,
        slot: &ArgValueCompletion<'a>,
        context: &Context<'a>,
        sources: &mut Vec<Source<'a>>,
    ) {
        match kind {
            BuiltinCompletion::NuFile {
                std_virtual_path, ..
            } => sources.extend([
                Source::Modules { std_virtual_path },
                Source::ModuleFiles { std_virtual_path },
            ]),
            BuiltinCompletion::ModuleExports => {
                sources.push(slot.module_exports(context));
            }
            BuiltinCompletion::EnvVar => sources.push(Source::EnvVars),
            BuiltinCompletion::Column => sources.push(Source::Columns),
            BuiltinCompletion::Process => sources.push(Source::Processes),
            BuiltinCompletion::Command { internal_only } => sources.push(Source::Commands {
                scope: if internal_only {
                    CommandScope::InternalsOnly
                } else {
                    CommandScope::All
                },
                quoted: true,
                parent: None,
            }),
            BuiltinCompletion::Path { dirs, .. } => {
                sources.push(Source::Paths { directories: dirs })
            }
        }
    }

    /// The flags of the command, then what its command-wide completer, such as an external one,
    /// adds: it often lists the declared flags too.
    fn complete_flag_names(
        &self,
        decl_id: DeclId,
        context: &Context,
        signature: &Signature,
    ) -> Vec<SemanticSuggestion> {
        let mut found = ask([Source::Flags(decl_id)], context);
        let flags = found.len();
        let wide = self.command_wide(signature, context.working_set);
        for new in ask(wide.map(Source::CommandWide), context) {
            let value = &new.suggestion.value;
            if !found[..flags]
                .iter()
                .any(|flag| flag.suggestion.value == *value)
            {
                found.push(new);
            } else if let Some(trace) = context.trace {
                trace.dropped(new.source.as_deref(), value.clone(), "duplicate");
            }
        }
        found
    }

    /// Command scope for a head, honouring a leading sigil: `^` → externals only, `%` →
    /// builtins only, otherwise everything. The sigil is the byte between the call's own
    /// span and its head span.
    fn command_scope_for_head(
        &self,
        node: Option<&Expression>,
        span: Span,
        working_set: &StateWorkingSet,
    ) -> CommandScope {
        let sigil = node
            .filter(|node| node.span.start < span.start)
            .and_then(|node| working_set.get_span_contents(node.span).first().copied());

        match sigil {
            Some(b'^') => CommandScope::ExternalsOnly,
            Some(b'%') => CommandScope::BuiltinsOnly,
            _ => CommandScope::All,
        }
    }

    /// Internal commands whose name extends the line so far (`foo test⌶` also offers
    /// `foo test bar`); externals are excluded, since a multi-word line names only
    /// internal subcommands. When the line already names the command `parent`, only its
    /// subcommands are offered.
    fn subcommand_suggestions(
        &self,
        context: &Context,
        command_start: usize,
        cursor: usize,
        parent: Option<&str>,
    ) -> Vec<SemanticSuggestion> {
        if cursor <= command_start {
            return Vec::new();
        }
        ask(
            [Source::Commands {
                scope: CommandScope::InternalsOnly,
                quoted: false,
                parent,
            }],
            &context.token(Span::new(command_start, cursor)),
        )
    }

    /// The command-wide completer of `signature`, which answers for the whole call.
    fn command_wide(
        &self,
        signature: &Signature,
        working_set: &StateWorkingSet,
    ) -> Option<UserCompletion> {
        match signature.complete {
            Some(CommandWideCompleter::Command(decl_id)) => {
                UserCompletion::command(working_set, decl_id)
            }
            Some(CommandWideCompleter::External) => (self.external_completer())
                .map(|closure| UserCompletion::closure(working_set, &closure)),
            None => None,
        }
    }

    /// The external completer the configuration names, if it names one.
    fn external_completer(&self) -> Option<Closure> {
        let config = self.stack.get_config(self.engine_state);
        config.completions.external.completer.clone()
    }

    /// A context over `text`, the commandline up to the cursor, which starts at `offset` in the
    /// working set. It has no token until [`Context::at_site`] gives it one.
    pub(crate) fn context<'a>(
        &'a self,
        working_set: &'a StateWorkingSet,
        text: &'a str,
        offset: usize,
    ) -> Context<'a> {
        Context {
            working_set,
            stack: self.stack.as_ref(),
            options: &self.options,
            span: Span::unknown(),
            prefix: &[],
            offset,
            buffer: text,
            level: None,
            delegation: self.delegation.as_deref(),
            usage: self.usage.as_ref(),
            cache: &self.cache,
            rows: None,
            piped: None,
            prior: None,
            prelude: &[],
            feed: None,
            trace: self.trace.as_ref(),
            notice: &self.notice,
            scope: &[],
        }
    }

    /// Why the last completion offered nothing, when a source said so.
    pub fn message(&self) -> Option<String> {
        self.notice.take()
    }
}

/// What the completer answered last, which the history hint checks the next word of a history
/// line against instead of running a completer itself.
#[derive(Clone, Default)]
pub struct LatestAnswer(Arc<Mutex<Option<Arc<Latest>>>>);

/// One answer, as the history hint reads it.
pub(crate) struct Latest {
    /// The line up to the cursor the answer was for.
    pub(crate) line: String,
    /// Where the word the answer completes starts.
    pub(crate) start: usize,
    /// The values offered, when they are a closed set the word must be among: values of a
    /// completer, all for one word. `None` for paths, commands or a mix, which say nothing.
    pub(crate) values: Option<Vec<String>>,
    /// The parameter the word binds, as the parser reads the line: another word of a history line
    /// that binds it too must be among the same values.
    pub(crate) slot: Option<(DeclId, usize)>,
}

/// The positional parameter the word at byte `at` of `line` binds, as the parser's probe reads
/// it: the call and the index of the parameter. Modules named in the line are not loaded.
pub(crate) fn positional_at(
    engine_state: &EngineState,
    line: &str,
    at: usize,
) -> Option<(DeclId, usize)> {
    let mut working_set = StateWorkingSet::new(engine_state);
    working_set.skip_module_load = true;
    let (_, probe) = nu_parser::parse_probed(&mut working_set, None, line.as_bytes(), at);
    probe.innermost().find_map(|found| match found.what {
        nu_protocol::engine::Expect::Positional { decl_id, index, .. } => Some((decl_id, index)),
        _ => None,
    })
}

impl LatestAnswer {
    pub(crate) fn get(&self) -> Option<Arc<Latest>> {
        self.0.lock().ok()?.clone()
    }

    fn remember(&self, engine_state: &EngineState, line: &str, answer: &Completions) {
        let start = answer
            .items
            .first()
            .map_or(line.len(), |item| item.span.start);
        let one_word = answer.items.iter().all(|item| item.span.start == start);
        let values_only = answer.data::<Details>().is_some_and(|details| {
            details
                .0
                .iter()
                .all(|detail| matches!(detail.kind, None | Some(SuggestionKind::Value(_))))
        });
        let values = (!answer.items.is_empty() && one_word && values_only)
            .then(|| answer.items.iter().map(|item| item.value.clone()).collect());
        if let Ok(mut latest) = self.0.lock() {
            let slot = values
                .as_ref()
                .and_then(|_| positional_at(engine_state, line, line.len()));
            *latest = Some(Arc::new(Latest {
                line: line.to_owned(),
                start,
                values,
                slot,
            }));
        }
    }
}

/// Work done off the line editor's thread for a preview or a hover: one thread runs them, the
/// newest first, and drops those whose answer is no longer wanted. A thread for each would leave
/// a pile of them running for every cursor move over a slow pipeline.
type PreviewJob = Box<dyn FnOnce() -> bool + Send>;

/// A thread that runs the newest of the preview jobs sent to it. A job returns whether it was
/// still wanted; the stale ones are dropped without running.
fn spawn_previewer() -> mpsc::Sender<PreviewJob> {
    let (jobs, queue) = mpsc::channel::<PreviewJob>();
    thread::spawn(move || {
        while let Ok(mut job) = queue.recv() {
            while let Ok(newer) = queue.try_recv() {
                job = newer;
            }
            let _ = job();
        }
    });
    jobs
}

pub struct NuCompleter {
    /// The latest answer, shared with the history hint.
    latest: LatestAnswer,
    /// The thread previews and hovers run on, started on the first.
    previewer: Option<mpsc::Sender<PreviewJob>>,
    engine_state: Arc<EngineState>,
    stack: Arc<Stack>,
    /// What completion remembers, shared with the completers of the next prompts.
    cache: CompletionCache,
    /// `cache`, for the environment this completer runs in.
    view: CacheView,
    /// Opens the history for reading off the editor's thread, when it can be opened again.
    history: Option<HistoryReader>,
    /// Whether the history was checked for new lines: once per completer, at the first request
    /// that has it.
    learned: bool,
    /// The highlighter's parse of the line being edited, if it is shared; see
    /// [`Self::share_parse_of`].
    parse_cache: Option<LineParseCache>,
    /// The background worker's queue, started on the first request it needs.
    worker: Option<mpsc::Sender<Job>>,
    /// The line and the cursor of the last request, which a click on an example in a preview
    /// takes the example over.
    last_request: Option<(String, usize)>,
    /// The width the last preview was asked for, to lay it out the same for a click.
    preview_width: u16,
}

/// A request the background worker answers.
struct Job {
    query: CompletionQuery,
    reply: Reply<Completions>,
    /// What the history said when the request was made.
    usage: Option<Arc<UsageIndex>>,
}

impl NuCompleter {
    pub fn new(engine_state: Arc<EngineState>, stack: Arc<Stack>) -> Self {
        Self::with_cache(engine_state, stack, CompletionCache::default())
    }

    pub(crate) fn with_cache(
        engine_state: Arc<EngineState>,
        stack: Arc<Stack>,
        cache: CompletionCache,
    ) -> Self {
        let env = CacheEnv::of(&engine_state, &stack);
        // Read fresh each prompt so `cache_size` config changes take effect.
        let cache_size = stack.get_config(&engine_state).completions.cache_size;
        cache.set_capacity(cache_size.try_into().unwrap_or(0));
        Self {
            latest: LatestAnswer::default(),
            previewer: None,
            history: None,
            engine_state,
            stack,
            view: CacheView::new(cache.clone(), env),
            cache,
            learned: false,
            parse_cache: None,
            worker: None,
            last_request: None,
            preview_width: 0,
        }
    }

    /// Completes from the parse that `highlighter` makes of the line being edited when the
    /// cursor is at its end, so a keystroke parses the line once for painting and completing.
    /// Completes with its own parse otherwise.
    #[must_use]
    pub fn share_parse_of(self, highlighter: &NuHighlighter) -> Self {
        self.with_parse_cache(highlighter.parse_cache())
    }

    /// Runs `job` on the preview thread, starting it the first time. A thread that died (a
    /// previewer closure panicked past its guard) is started again.
    fn preview_off_the_editor(&mut self, job: PreviewJob) {
        let previewer = self.previewer.get_or_insert_with(spawn_previewer);
        if let Err(mpsc::SendError(job)) = previewer.send(job) {
            let previewer = spawn_previewer();
            let _ = previewer.send(job);
            self.previewer = Some(previewer);
        }
    }

    /// Shares the latest answer through `latest`, which the history hint reads.
    #[must_use]
    pub fn with_latest(mut self, latest: LatestAnswer) -> Self {
        self.latest = latest;
        self
    }

    /// Reads the history for [`Self::learn_usage`] with `reader`, on a thread of its own: the
    /// line editor's history belongs to its thread.
    #[must_use]
    pub(crate) fn with_history_reader(mut self, reader: Option<HistoryReader>) -> Self {
        self.history = reader;
        self
    }

    /// [`Self::share_parse_of`], for the cache itself.
    #[must_use]
    pub(crate) fn with_parse_cache(mut self, parse_cache: LineParseCache) -> Self {
        self.parse_cache = Some(parse_cache);
        self
    }

    /// Ranks candidates the user runs often and lately higher, from the `history`. The index is
    /// shared with the next prompt's completer, and read again only when the history has grown,
    /// off the calling thread where the history can be opened again. It is checked once per
    /// completer; later calls change nothing.
    pub fn learn_usage(&mut self, history: &dyn History) {
        if !self.learned {
            self.learned = true;
            let engine_state = Arc::clone(&self.engine_state);
            self.cache
                .learn_usage(history, self.history.as_ref(), move |name| {
                    engine_state.find_decl(name.as_bytes(), &[]).is_some()
                });
        }
    }

    /// An engine over this completer's state, on an isolated stack with stdin suppressed
    /// where `suppress_stdin`.
    fn engine(&self, suppress_stdin: bool) -> CompletionEngine<'_> {
        CompletionEngine::isolated(&self.engine_state, Arc::clone(&self.stack), suppress_stdin)
            .with_usage(self.cache.usage())
            .with_cache(self.view.clone())
            .with_parse_cache(self.parse_cache.clone())
    }

    /// A worker that answers the newest of the requests sent to it and drops the rest.
    fn spawn_worker(
        engine_state: Arc<EngineState>,
        stack: Arc<Stack>,
        view: CacheView,
        parse_cache: Option<LineParseCache>,
        latest: LatestAnswer,
    ) -> mpsc::Sender<Job> {
        let (jobs, queue) = mpsc::channel::<Job>();
        thread::spawn(move || {
            // The thread owns the state; nothing is cloned per query.
            let mut engine = CompletionEngine::isolated(&engine_state, stack, true)
                .with_cache(view)
                .with_parse_cache(parse_cache);
            while let Ok(mut job) = queue.recv() {
                while let Ok(newer) = queue.try_recv() {
                    job = newer;
                }
                if !job.reply.is_current() {
                    continue;
                }
                engine.usage = job.usage.take();
                let answer = engine.answer(&job.query);
                latest.remember(&engine_state, job.query.typed(), &answer);
                job.reply.send(answer);
            }
        });
        jobs
    }

    /// Whether the completer that would run at `pos` is `@interactive`, so
    /// [`Completer::complete`] answers it on the calling thread, and only when asked,
    /// rather than on the background worker.
    pub fn is_interactive_at(&self, line: &str, pos: usize) -> bool {
        CompletionEngine::isolated(&self.engine_state, Arc::clone(&self.stack), false)
            .interactive_completer_at(line, pos)
    }

    /// Completes `line` at `pos` on this thread, for tests and callers that
    /// cannot wait on a reply. Like [`Completer::complete`], it runs an
    /// `@interactive` completer every time: a cancelled picker must run again.
    pub fn complete_blocking(&mut self, line: &str, pos: usize) -> Suggestions {
        self.complete_blocking_with_message(line, pos).0
    }

    /// [`Self::complete_blocking`], and why there is nothing to suggest when a source said so.
    pub fn complete_blocking_with_message(
        &mut self,
        line: &str,
        pos: usize,
    ) -> (Suggestions, Option<String>) {
        let query = CompletionQuery::new(line, pos);
        let interactive = self.is_interactive_at(query.typed(), query.cursor());
        let answer = self.engine(!interactive).answer(&query);
        self.latest
            .remember(&self.engine_state, query.typed(), &answer);
        (answer.items, answer.message)
    }
}

impl ReedlineCompleter for NuCompleter {
    fn hover(&mut self, line: &str, pos: usize, width: u16, reply: Reply<Preview>) {
        let engine_state = Arc::clone(&self.engine_state);
        let stack = Arc::clone(&self.stack);
        let line = line.to_owned();
        // A slow disk, or a pipeline run for a preview, must not hold up the key that moved.
        self.preview_off_the_editor(Box::new(move || {
            if !reply.is_current() {
                return false;
            }
            let hovered = hovered(&engine_state, &line, pos);
            let preview =
                super::preview::hover(&engine_state, &stack, hovered.as_ref(), width, &line, pos);
            reply.send(preview);
            true
        }));
    }

    fn preview(
        &mut self,
        completions: &Completions,
        index: usize,
        width: u16,
        reply: Reply<Preview>,
    ) {
        self.preview_width = width;
        let engine_state = Arc::clone(&self.engine_state);
        let stack = Arc::clone(&self.stack);
        let completions = completions.clone();
        let place = self.last_request.clone();
        // A slow disk, or a closure, must not hold up the keystroke that asked.
        self.preview_off_the_editor(Box::new(move || {
            if !reply.is_current() {
                return false;
            }
            reply.send(super::preview::preview(
                &engine_state,
                &stack,
                &completions,
                index,
                width,
                place
                    .as_ref()
                    .map(|(line, cursor)| (line.as_str(), *cursor)),
            ));
            true
        }));
    }

    fn preview_clicked(
        &mut self,
        completions: &Completions,
        index: usize,
        row: usize,
    ) -> Option<Suggestion> {
        let (line, cursor) = self.last_request.as_ref()?;
        // The same layout as the preview that was drawn, which says which rows hold examples.
        let (_, examples) = super::preview::preview_with_examples(
            &self.engine_state,
            &self.stack,
            completions,
            index,
            self.preview_width,
            Some((line, *cursor)),
        );
        let (_, code) = examples.iter().find(|(rows, _)| rows.contains(&row))?;
        Some(super::examples::take(code, line, *cursor))
    }

    /// What the line can only go on with after the space just typed: a closure for `each`,
    /// `= ` for `let x`, the parameters of a `def`.
    fn opening(&mut self, line: &str, pos: usize) -> Option<Opening> {
        if !self
            .stack
            .get_config(&self.engine_state)
            .completions
            .opening
        {
            return None;
        }
        let (text, cursor) = taking::what_must_follow(&self.engine_state, line, pos)?;
        Some(Opening { text, cursor })
    }

    fn complete(&mut self, request: Request<'_>, reply: Reply<Completions>) {
        self.last_request = Some((request.line.to_owned(), request.cursor));
        // An empty line is the prompt waiting: every command there is would be offered to it
        // unasked.
        if !request.asked && request.line.trim().is_empty() {
            reply.send(Completions::default());
            return;
        }
        self.learn_usage(request.history);
        let mut query = CompletionQuery::new(request.line, request.cursor);
        query.asked = request.asked;

        // Interactive completers run on the UI thread, which owns the
        // terminal, and only when asked: typing never opens a picker.
        if self
            .engine(false)
            .interactive_completer_at(query.typed(), query.cursor())
        {
            if request.asked {
                reply.send(self.engine(false).answer(&query));
            }
            return;
        }

        let worker = self.worker.get_or_insert_with(|| {
            Self::spawn_worker(
                Arc::clone(&self.engine_state),
                Arc::clone(&self.stack),
                self.view.clone(),
                self.parse_cache.clone(),
                self.latest.clone(),
            )
        });
        let job = Job {
            query,
            reply,
            usage: self.cache.usage(),
        };
        if let Err(mpsc::SendError(job)) = worker.send(job) {
            // The worker died (a completer closure panicked past its guard):
            // answer here, and start another next time.
            self.worker = None;
            job.reply.send(self.engine(false).answer(&job.query));
        }
    }
}

#[cfg(test)]
mod completer_tests {
    use super::*;
    use nu_parser::parse;

    fn test_engine() -> Arc<EngineState> {
        let mut engine =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        let delta = StateWorkingSet::new(&engine).render();
        engine.merge_delta(delta).expect("merge_delta");
        Arc::new(engine)
    }

    /// Corpus covering resolver branches.
    const CORPUS: &[&str] = &[
        "ls",
        "ls foo",
        "ls foo bar",
        "ls --long",
        "ls --long foo",
        "ls -a foo",
        "^ls foo",
        "sudo ls foo",
        "ls | where size > 1",
        "ls | where size > 1 ",
        "ls foo ",
        "1 + 2 ",
        "$env.PATH ",
        "ls | where size > ",
        "ls | where size >",
        "ls | where name == foo and size > 2",
        "1 + 2",
        "1 + ",
        "1 ",
        "$env.PATH",
        "$env.",
        "$nu",
        "each { |x| $x.name }",
        "let x = (ls | where name == foo)",
        "def foo [] { ls }",
        "@example\ndef foo [] {}",
        "use std/util [",
        "ls é foo",
        "",
        " ",
        // In a whole file a gap before later arguments is not the call's trailing slot.
        "str trim -- foo",
        "ls  foo",
        "str trim  --char x bar",
        "echo a  | str trim  -c x",
    ];

    /// `commandline complete --input` place.kind/index for the HackMD where-size cases.
    fn place_of(engine: &CompletionEngine, line: &str) -> (String, Option<i64>) {
        let input = engine.completer_input_at(line, line.len(), DeclaredInputs::all());
        let place = input.get_data_by_key("place").expect("place field");
        let kind = place
            .get_data_by_key("kind")
            .expect("kind")
            .as_str()
            .expect("kind string")
            .to_string();
        let index = place.get_data_by_key("index").and_then(|v| v.as_int().ok());
        (kind, index)
    }

    /// Token text/span equals target shown to completer, at every cursor (a site that does not
    /// hold the cursor fails a debug assertion), and the whole line completes there as a file.
    #[test]
    fn the_token_is_what_the_answer_replaces() {
        let engine_state = test_engine();
        let engine = CompletionEngine::new(&engine_state, &Stack::new());

        let range = |record: &Value, field: &str| {
            let span = record.get_data_by_key(field).expect("a span field");
            let at = |end| {
                span.get_data_by_key(end)
                    .and_then(|value| value.as_int().ok())
                    .expect("an offset") as usize
            };
            (at("start"), at("end"))
        };

        for line in CORPUS {
            for cursor in (0..=line.len()).filter(|at| line.is_char_boundary(*at)) {
                let _ = engine.fetch_completions_within_file("sweep.nu", cursor, line);
                let contents = &line[..cursor];
                let input = engine.completer_input_at(contents, cursor, DeclaredInputs::all());

                let token = input.get_data_by_key("token").expect("a token");
                let place = input.get_data_by_key("place").expect("a place");
                let text = token
                    .get_data_by_key("text")
                    .and_then(|value| value.coerce_into_string().ok())
                    .expect("the token's text");

                let at = range(&token, "span");
                let target = range(&place, "target");
                assert_eq!(
                    at,
                    (target.0, target.1.min(contents.len())),
                    "{line:?} at {cursor}: the token is not the target the completer saw"
                );
                assert_eq!(
                    Some(text.as_str()),
                    contents.get(at.0..at.1),
                    "{line:?} at {cursor}: the token is not the text it spans"
                );
            }
        }
    }

    /// RowConditionSlot prefers the signature's condition index, not raw arg counting —
    /// a leading path arg must not steal index 0 from the condition.
    #[test]
    fn row_condition_index_follows_signature_slot_not_arg_order() {
        use nu_protocol::{Signature, SyntaxShape};

        let mut engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        let mut working_set = StateWorkingSet::new(&engine_state);
        working_set.add_decl(
            Signature::build("mywhere")
                .required("path", SyntaxShape::Filepath, "path")
                .required("cond", SyntaxShape::RowCondition, "condition")
                .predeclare(),
        );
        let delta = working_set.render();
        engine_state.merge_delta(delta).expect("merge mywhere");
        let engine_state = Arc::new(engine_state);
        let engine = CompletionEngine::new(&engine_state, &Stack::new());

        assert_eq!(
            place_of(&engine, "mywhere ./data size > 1"),
            ("positional".into(), Some(1)),
            "condition after a path arg is signature index 1"
        );
        assert_eq!(
            place_of(&engine, "mywhere ./data size > "),
            ("positional".into(), Some(1)),
            "trailing gap after operator stays condition index 1"
        );
        assert_eq!(
            place_of(&engine, "mywhere ./da"),
            ("positional".into(), Some(0)),
            "path arg stays index 0"
        );
    }

    /// A cursor mid-char or past the end must not panic or split a char; `typed()` slices
    /// the buffer directly.
    #[test]
    fn the_cursor_is_clamped_to_a_char_boundary() {
        // "é" occupies bytes 3..5, so a cursor at 4 is mid-character.
        assert_eq!(CompletionQuery::new("ls é", 4).typed(), "ls ");

        let past_end = CompletionQuery::new("ls", 99);
        assert_eq!(past_end.typed(), "ls");
        assert_eq!(past_end.cursor(), 2);

        let empty = CompletionQuery::new("", 7);
        assert_eq!(empty.typed(), "");
        assert_eq!(empty.cursor(), 0);
    }

    /// Runs `source` in `engine` and `stack`, and merges the environment it leaves.
    fn apply_source(engine: &mut EngineState, stack: &mut Stack, source: &[u8]) {
        apply_stack_local_source(engine, stack, source);
        engine.merge_env(stack).expect("merge_env");
    }

    /// Runs `source` in `engine` and `stack`, leaving the environment on the stack.
    fn apply_stack_local_source(engine: &mut EngineState, stack: &mut Stack, source: &[u8]) {
        use nu_engine::eval_block;
        use nu_protocol::{PipelineData, debugger::WithoutDebug};

        let mut working_set = StateWorkingSet::new(engine);
        let block = parse(&mut working_set, None, source, false);
        assert!(
            working_set.parse_errors.is_empty(),
            "{:?}",
            working_set.parse_errors
        );
        engine
            .merge_delta(working_set.render())
            .expect("merge_delta");
        eval_block::<WithoutDebug>(engine, stack, &block, PipelineData::empty())
            .expect("eval source");
    }

    /// A line whose completion the cache keeps: `t ` asks a parameter completer that takes no
    /// input, whose list does not depend on the token.
    const KEPT_LINE: &str = "t ";

    /// `engine` and `stack` with the `t` command and its completer defined.
    fn with_t(engine: &EngineState, stack: &mut Stack) -> EngineState {
        let mut engine = engine.clone();
        apply_source(
            &mut engine,
            stack,
            b"def comp [] { [apple banana] }\ndef t [x: string@comp] {}",
        );
        engine
    }

    /// How many custom answers this completer's cache holds in its environment.
    fn kept(completer: &NuCompleter) -> usize {
        completer.view.answers_kept()
    }

    /// The worker runs on an isolated stack and must still produce identical results.
    #[test]
    fn background_result_matches_the_synchronous_engine() {
        let engine = test_engine();
        let mut completer = NuCompleter::new(engine.clone(), Arc::new(Stack::new()));

        let mut expected: Vec<_> = CompletionEngine::new(&engine, &Stack::new())
            .fetch_completions_for_editor("ls | c", 6)
            .into_iter()
            .map(|s| s.suggestion.value)
            .collect();
        expected.sort();
        assert!(expected.iter().any(|value| value == "cd"));
        let mut settled = values(&mut completer, "ls | c");
        settled.sort();
        assert_eq!(expected, settled);
    }

    /// The values `completer` offers at the end of `line`.
    fn values(completer: &mut NuCompleter, line: &str) -> Vec<String> {
        completer
            .complete_blocking(line, line.len())
            .iter()
            .map(|suggestion| suggestion.value.clone())
            .collect()
    }

    /// What a custom completer returned is kept, and narrowing the token ranks it again.
    #[test]
    fn a_custom_answer_is_kept_and_narrowed_again() {
        let mut stack = Stack::new();
        let engine = Arc::new(with_t(&test_engine(), &mut stack));
        let mut completer = NuCompleter::new(engine, Arc::new(stack));
        assert_eq!(kept(&completer), 0);

        // Only the completer's own: `t` names a command, so `to csv` is not a reading of `t `.
        assert_eq!(values(&mut completer, "t "), ["apple", "banana"]);
        assert_eq!(kept(&completer), 1);
        assert_eq!(values(&mut completer, "t ba"), ["banana"]);
        assert_eq!(values(&mut completer, "t a"), ["apple", "banana"]);
        // One answer served all three tokens.
        assert_eq!(kept(&completer), 1);
    }

    /// A completer that is handed the token can answer differently for each, so it runs
    /// every time.
    #[test]
    fn a_completer_that_reads_the_token_is_not_kept() {
        let mut stack = Stack::new();
        let mut engine = (*test_engine()).clone();
        apply_source(
            &mut engine,
            &mut stack,
            b"def comp [token: record] { [$\"($token.text)1\" $\"($token.text)2\"] }\n\
              def t [x: string@comp] {}",
        );
        let mut completer = NuCompleter::new(Arc::new(engine), Arc::new(stack));
        assert_eq!(values(&mut completer, "t x"), ["x1", "x2"]);
        assert_eq!(kept(&completer), 0);
    }

    /// A cache handed to a new per-prompt completer must still answer the previous prompt's
    /// questions.
    #[test]
    fn cache_outlives_the_completer_that_filled_it() {
        let mut stack = Stack::new();
        let engine = Arc::new(with_t(&test_engine(), &mut stack));
        let stack = Arc::new(stack);
        let cache = CompletionCache::default();

        let mut filling_prompt =
            NuCompleter::with_cache(engine.clone(), stack.clone(), cache.clone());
        assert!(!filling_prompt.complete_blocking(KEPT_LINE, 2).is_empty());
        drop(filling_prompt);

        let next_prompt = NuCompleter::with_cache(engine, stack, cache);
        assert_eq!(
            kept(&next_prompt),
            1,
            "a carried-over answer should be there"
        );
    }

    /// ...but not across a `cd`/`$env.PATH` change, the reason [`CacheEnv`] exists.
    #[test]
    fn cache_is_not_reused_in_a_different_environment() {
        use nu_protocol::Value;

        let mut stack = Stack::new();
        let engine = Arc::new(with_t(&test_engine(), &mut stack));
        let cache = CompletionCache::default();

        let mut filling_prompt =
            NuCompleter::with_cache(engine.clone(), Arc::new(stack.clone()), cache.clone());
        assert!(!filling_prompt.complete_blocking(KEPT_LINE, 2).is_empty());

        let mut moved = stack;
        moved.add_env_var(
            "PATH".into(),
            Value::string("/somewhere/else", Span::unknown()),
        );
        let next_prompt = NuCompleter::with_cache(engine, Arc::new(moved), cache);
        assert_eq!(
            kept(&next_prompt),
            0,
            "answers from another environment must not be served"
        );
    }

    /// What the cache answers may be out of date, which `debug complete` reports as its
    /// `origin`; the completer's first answer is live.
    #[test]
    fn what_the_cache_answers_is_marked_as_cached() {
        use crate::completions::Origin;

        let mut stack = Stack::new();
        let engine = with_t(&test_engine(), &mut stack);
        let cache = CompletionCache::default();
        let origins = || -> Vec<_> {
            (CompletionEngine::new(&engine, &stack).caching_in(&cache))
                .fetch_completions_at(KEPT_LINE, 2)
                .into_iter()
                .map(|found| found.origin)
                .collect()
        };
        assert_eq!(origins(), [Origin::Live; 2]);
        assert_eq!(origins(), [Origin::Cached; 2]);
    }

    /// Any `$env.config` assignment, including ones that are not completion
    /// settings, must not keep serving the previous prompt's answers.
    #[rstest::rstest]
    #[case::external_completer(
        b"$env.config.completions.external.completer = {|spans| [from-second]}",
        Some("from-second")
    )]
    #[case::unrelated_config(b"$env.config.float_precision = 5", None)]
    fn cache_is_not_reused_after_config_changes(
        #[case] later: &[u8],
        #[case] expected_external: Option<&str>,
    ) {
        let mut engine = (*test_engine()).clone();
        let mut stack = Stack::new();
        apply_source(
            &mut engine,
            &mut stack,
            b"$env.config.completions.external.completer = {|spans| [from-first]}",
        );
        let engine = with_t(&engine, &mut stack);
        let first_env = CacheEnv::of(&engine, &stack);
        let engine = Arc::new(engine);
        let stack = Arc::new(stack);
        let cache = CompletionCache::default();

        let mut filling_prompt =
            NuCompleter::with_cache(engine.clone(), stack.clone(), cache.clone());
        assert!(!filling_prompt.complete_blocking(KEPT_LINE, 2).is_empty());
        drop(filling_prompt);

        let mut engine = (*engine).clone();
        let mut stack = (*stack).clone();
        apply_source(&mut engine, &mut stack, later);
        let second_env = CacheEnv::of(&engine, &stack);
        assert_ne!(
            first_env, second_env,
            "a config assignment must change the cache fingerprint"
        );

        let mut next_prompt = NuCompleter::with_cache(Arc::new(engine), Arc::new(stack), cache);
        assert_eq!(
            kept(&next_prompt),
            0,
            "answers computed under a previous config must not be served"
        );

        if let Some(expected) = expected_external {
            let values = values(&mut next_prompt, "extcommand x");
            assert_eq!(values, [expected], "the newly assigned completer must run");
        }
    }

    /// Stack-local config updates have not reached `engine_state.config_epoch()` yet, but
    /// they still shape completion behavior and must invalidate shared cache entries.
    #[test]
    fn cache_is_not_reused_after_stack_local_config_changes() {
        let engine = test_engine();
        let cache = CompletionCache::default();

        let mut first_stack = Stack::new();
        let engine = Arc::new(with_t(&engine, &mut first_stack));
        let mut second_stack = first_stack.clone();
        first_stack.config = Some({
            let mut config = engine.get_config().as_ref().clone();
            config.float_precision = 5;
            Arc::new(config)
        });

        let mut filling_prompt =
            NuCompleter::with_cache(engine.clone(), Arc::new(first_stack), cache.clone());
        assert!(!filling_prompt.complete_blocking(KEPT_LINE, 2).is_empty());
        drop(filling_prompt);

        second_stack.config = Some({
            let mut config = engine.get_config().as_ref().clone();
            config.float_precision = 6;
            Arc::new(config)
        });

        let next_prompt = NuCompleter::with_cache(engine, Arc::new(second_stack), cache);
        assert_eq!(
            kept(&next_prompt),
            0,
            "answers from another stack-local config must not be served"
        );
    }

    /// A stack-local external completer change must run the new completer.
    #[test]
    fn cache_is_not_reused_after_stack_local_external_completer_changes() {
        let mut engine = (*test_engine()).clone();
        let mut first_stack = Stack::new();
        apply_stack_local_source(
            &mut engine,
            &mut first_stack,
            b"$env.config.completions.external.completer = {|buffer| [from-first]}",
        );
        let mut second_stack = Stack::new();
        apply_stack_local_source(
            &mut engine,
            &mut second_stack,
            b"$env.config.completions.external.completer = {|buffer| [from-second]}",
        );
        let engine = Arc::new(engine);
        let cache = CompletionCache::default();

        let mut filling_prompt =
            NuCompleter::with_cache(engine.clone(), Arc::new(first_stack), cache.clone());
        assert_eq!(values(&mut filling_prompt, "extcommand x"), ["from-first"]);
        drop(filling_prompt);

        let mut next_prompt = NuCompleter::with_cache(engine, Arc::new(second_stack), cache);
        assert_eq!(values(&mut next_prompt, "extcommand x"), ["from-second"]);
    }

    /// `cache_size = 0` must keep nothing, even in a cache that was carried over.
    #[test]
    fn cache_size_zero_disables_the_cache() {
        let mut stack = Stack::new();
        let mut engine = with_t(&test_engine(), &mut stack);
        let mut config = engine.get_config().as_ref().clone();
        config.completions.cache_size = 0;
        engine.set_config(config);
        let engine = Arc::new(engine);
        let stack = Arc::new(stack);
        let cache = CompletionCache::default();

        let mut filling_prompt =
            NuCompleter::with_cache(engine.clone(), stack.clone(), cache.clone());
        assert!(!filling_prompt.complete_blocking(KEPT_LINE, 2).is_empty());
        assert_eq!(kept(&filling_prompt), 0, "a disabled cache keeps nothing");
    }

    /// Stack-local `cache_size` changes should take effect before the config is merged
    /// into `EngineState`.
    #[test]
    fn stack_local_cache_size_zero_disables_the_cache() {
        let mut stack = Stack::new();
        let engine = Arc::new(with_t(&test_engine(), &mut stack));
        stack.config = Some({
            let mut config = engine.get_config().as_ref().clone();
            config.completions.cache_size = 0;
            Arc::new(config)
        });
        let cache = CompletionCache::default();

        let mut disabled_prompt =
            NuCompleter::with_cache(engine.clone(), Arc::new(stack), cache.clone());
        assert!(!disabled_prompt.complete_blocking(KEPT_LINE, 2).is_empty());
        assert_eq!(kept(&disabled_prompt), 0);
    }

    /// An answer is kept only for the line before the token: another line is another question.
    #[test]
    fn another_line_before_the_token_is_another_question() {
        let mut stack = Stack::new();
        let engine = Arc::new(with_t(&test_engine(), &mut stack));
        let mut completer = NuCompleter::new(engine, Arc::new(stack));
        assert!(!completer.complete_blocking("t ", 2).is_empty());
        assert!(!completer.complete_blocking("echo (t ", 8).is_empty());
        assert_eq!(kept(&completer), 2);
    }

    /// With the cursor at the end of the line, completing reads the parse that the highlighter
    /// makes of it, and the highlighter then finds it already made; elsewhere in the line it
    /// parses for itself. Either way the answer is the same.
    #[test]
    fn a_line_is_completed_from_the_highlighters_parse() {
        let engine = test_engine();
        let stack = Arc::new(Stack::new());
        let highlighter = NuHighlighter::new(engine.clone(), stack.clone());
        let mut sharing =
            NuCompleter::new(engine.clone(), stack.clone()).share_parse_of(&highlighter);
        let mut alone = NuCompleter::new(engine, stack);

        let line = "ls | c";
        assert!(!highlighter.parse_cache().holds(line));
        let answer = sharing.complete_blocking(line, line.len());
        assert!(highlighter.parse_cache().holds(line));
        assert!(!answer.is_empty());
        assert_eq!(answer, alone.complete_blocking(line, line.len()));

        // A cursor with text after it parses the whole line differently from what is before it.
        let line = "ls | cd x";
        let answer = sharing.complete_blocking(line, 6);
        assert!(!highlighter.parse_cache().holds(line));
        assert_eq!(answer, alone.complete_blocking(line, 6));
    }

    /// The highlighter parses without loading modules; a line that has a `use` it did not load
    /// is completed from a parse of its own.
    #[test]
    fn a_line_the_highlighter_could_not_parse_in_full_is_parsed_again() {
        let engine = test_engine();
        let stack = Arc::new(Stack::new());
        let highlighter = NuHighlighter::new(engine.clone(), stack.clone());
        let mut sharing =
            NuCompleter::new(engine.clone(), stack.clone()).share_parse_of(&highlighter);
        let mut alone = NuCompleter::new(engine, stack);

        let line = "use std/assert; assert e";
        let answer = sharing.complete_blocking(line, line.len());
        assert!(highlighter.parse_cache().holds(line));
        assert_eq!(answer, alone.complete_blocking(line, line.len()));
    }

    /// What the completer knows of each suggestion rides with its answer, by index, and the
    /// panel draws it: kinds in their colours, paths under the directory they share, a
    /// correction as what was typed and what was meant, the types of columns, and the fields a
    /// custom completer returns beyond the known ones.
    #[test]
    fn the_panel_draws_what_the_completer_knows_of_each_suggestion() {
        use crate::completion_panel::{nu_panel, tests::view};
        use nu_ansi_term::{Color, Style};
        use reedline::{Panel, View};
        use std::path::MAIN_SEPARATOR as SEP;

        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir_all(dir.path().join("src/completions")).expect("a directory");
        std::fs::write(dir.path().join("src/repl.rs"), "").expect("a file");
        let mut engine = (*test_engine()).clone();
        let mut stack = Stack::new();
        let cwd = Value::test_string(dir.path().to_string_lossy());
        stack.add_env_var("PWD".into(), cwd);
        apply_source(
            &mut engine,
            &mut stack,
            b"def comp [] { [{value: main, remote: origin}, {value: topic}, {value: trunk, remote: fork, age: 2hr}] }
              def t [x: string@comp] {}",
        );
        let completer = CompletionEngine::isolated(&engine, Arc::new(stack), true);
        let answer = |line: &str| completer.answer(&CompletionQuery::new(line, line.len()));
        let red = [("shape_internalcall".to_owned(), Value::test_string("red"))];
        let config = Config {
            color_config: red.into_iter().collect(),
            ..Config::default()
        };
        let mut panel = nu_panel(&Arc::new(config), Style::new());
        fn coloured(completions: &Completions) -> View<'_> {
            View {
                use_ansi_coloring: true,
                ..view(completions, None)
            }
        }

        // A command in the colour the config gives it, a keyword in its own; the hint says what a
        // command takes before what it is for.
        let commands = answer("wher");
        let rows = panel.render(&view(&commands, None));
        assert_eq!(rows[1..3], ["│ where        │", "│ record where │"]);
        let rows = panel.render(&coloured(&commands));
        assert!(!rows[1].contains("\x1b[31m"), "{rows:?}");
        assert!(
            rows[2].contains(&Color::Red.paint("d").to_string()),
            "{rows:?}"
        );
        let hint = panel.hint(&view(&commands, Some(0))).unwrap_or_default();
        assert!(hint.starts_with("  <condition>  Filter values"), "{hint:?}");

        let rows = panel.render(&view(&answer(&format!("ls src{SEP}")), None));
        let top = format!("╭─ src{SEP} ───────╮");
        let directory = format!("│ completions{SEP} │");
        let below = ["│ repl.rs      │", "╰──────────────╯"];
        assert_eq!(rows, [top.as_str(), directory.as_str(), below[0], below[1]]);

        let fixed = answer("ls | wehre");
        assert_eq!(panel.render(&view(&fixed, None))[1], "│ wehre → where │");
        let struck = Style::new().strikethrough().paint("wehre").to_string();
        assert!(panel.render(&coloured(&fixed))[1].contains(&struck));

        let rows = panel.render(&view(&answer("{name: a, size: 160b} | get "), None));
        assert_eq!(rows[1..3], ["│ name  string   │", "│ size  filesize │"]);

        // The extra fields are columns in the order they were first seen.
        let rows = panel.render(&view(&answer("t "), None));
        let fields = [
            "│ main   origin      │",
            "│ topic              │",
            "│ trunk  fork    2hr │",
        ];
        assert_eq!(rows[1..4], fields);
        // An answer with no suggestions carries no data.
        assert!(answer("t zzzz").data.is_none());
    }

    /// A completer over an engine that has `git checkout`, three examples of it, and a flag;
    /// it has seen `git ch` completed with the cursor at its end and a preview drawn.
    fn completer_with_examples() -> (NuCompleter, DeclId) {
        let mut engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        let mut working_set = StateWorkingSet::new(&engine_state);
        parse(
            &mut working_set,
            None,
            br#"
            # Switch branches.
            @example "Create a branch" "git checkout -b <branch>"
            @example "Create a branch from a ref" "git checkout -b <branch> <ref>"
            @example "Go back" "git checkout -"
            export extern "git checkout" [
                --branch(-b): string # The new branch.
                target?: string # What to check out.
            ]"#,
            false,
        );
        assert!(working_set.parse_errors.is_empty());
        engine_state
            .merge_delta(working_set.render())
            .expect("merge_delta");
        let decl_id = engine_state
            .find_decl(b"git checkout", &[])
            .expect("the command");
        let mut stack = Stack::new();
        let cwd = std::env::current_dir().expect("a current directory");
        stack.add_env_var(
            "PWD".into(),
            Value::string(cwd.to_string_lossy(), Span::unknown()),
        );
        let mut completer = NuCompleter::new(Arc::new(engine_state), Arc::new(stack));
        completer.last_request = Some(("git ch".into(), 6));
        completer.preview_width = 70;
        (completer, decl_id)
    }

    /// The answer with one suggestion, `value` as the details say.
    fn one_suggestion(value: &str, detail: crate::completions::Detail) -> Completions {
        Completions::new(vec![Suggestion {
            value: value.into(),
            ..Suggestion::default()
        }])
        .with_data(Details(vec![detail]))
    }

    #[test]
    fn a_click_on_an_example_of_a_command_preview_takes_it_over_the_command() {
        use crate::completions::Detail;
        let (mut completer, decl_id) = completer_with_examples();
        let completions = one_suggestion(
            "git checkout",
            Detail {
                kind: Some(SuggestionKind::Command(
                    CommandType::External,
                    Some(decl_id),
                )),
                ..Detail::default()
            },
        );
        let (shown, examples) = crate::completions::preview::preview_with_examples(
            &completer.engine_state,
            &completer.stack,
            &completions,
            0,
            70,
            None,
        );
        // The first two examples are folded in `rows`, the third is behind the fold.
        assert_eq!(examples.len(), 3, "{shown:#?}");
        assert!(examples[2].0.start >= shown.rows.len());

        // The last row of an example block is its code.
        for (rows, code) in &examples {
            let taken = completer
                .preview_clicked(&completions, 0, rows.end - 1)
                .expect("an example");
            assert_eq!(&taken.value, code);
            assert_eq!((taken.span.start, taken.span.end), (0, 6));
        }
        let taken = completer
            .preview_clicked(&completions, 0, examples[1].0.end - 1)
            .expect("the second example");
        assert_eq!(taken.value, "git checkout -b <branch> <ref>");
        // The cursor is on the first placeholder, the end of `<branch>`.
        assert_eq!(taken.cursor, Some("git checkout -b <branch>".len()));

        // A row that is not an example, such as the blank between two, selects as before.
        let between = (0..shown.rows.len() + shown.more.len())
            .find(|row| examples.iter().all(|(rows, _)| !rows.contains(row)))
            .expect("a row between examples");
        assert!(
            completer
                .preview_clicked(&completions, 0, between)
                .is_none()
        );
        assert!(completer.preview_clicked(&completions, 0, 999).is_none());
    }

    #[test]
    fn a_click_under_a_flags_help_takes_an_example_that_uses_it() {
        use crate::completions::Detail;
        let (mut completer, decl_id) = completer_with_examples();
        let completions = one_suggestion(
            "--branch",
            Detail {
                kind: Some(SuggestionKind::Flag),
                help_at: Some(Arc::new(HelpAt {
                    command: HelpFor::Decl(decl_id),
                    slot: Slot::Flag,
                })),
                ..Detail::default()
            },
        );
        let (_, examples) = crate::completions::preview::preview_with_examples(
            &completer.engine_state,
            &completer.stack,
            &completions,
            0,
            70,
            None,
        );
        assert_eq!(examples.len(), 2);
        for (rows, _) in &examples {
            let taken = completer
                .preview_clicked(&completions, 0, rows.end - 1)
                .expect("an example");
            assert!(taken.value.starts_with("git checkout -b "), "{taken:?}");
        }
        // The blank row between them is not one.
        assert!(
            completer
                .preview_clicked(&completions, 0, examples[0].0.end)
                .is_none()
        );
    }
}
