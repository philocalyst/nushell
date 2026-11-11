//! What a record being written can go on with. The keys come from what says what they are: the
//! records beside it in a list, the record its parameter declares, what a value being assigned
//! to already holds, and the rows that flow in. What a key already has beside it says what its
//! value can be. Where the cursor is in the record the parser's probe says.

use super::{Context, SemanticSuggestion, Source, ask, config_docs, evaluate_text};
use nu_protocol::{
    Span, SuggestionKind, SyntaxShape, Type, Value,
    ast::{Assignment, Block, Expr, Expression, Operator, RecordItem, Traverse},
    engine::{Expect, Probe},
};
use reedline::Suggestion;

/// The records finished before the one that starts at `start`, in the list it is an item of:
/// each key with the text of its value.
fn siblings(block: &Block, start: usize, ctx: &Context) -> Vec<Vec<(String, String)>> {
    let working_set = ctx.working_set;
    let text = |expression: &Expression| {
        String::from_utf8_lossy(working_set.get_span_contents(expression.span)).into_owned()
    };
    let mut lists: Vec<&Expression> = Vec::new();
    block.flat_map(
        working_set,
        &|expr| match &expr.expr {
            Expr::List(items) if items.iter().any(|item| item.expr().span.start == start) => {
                vec![expr]
            }
            _ => vec![],
        },
        &mut lists,
    );
    let Some(Expr::List(items)) = lists.last().map(|list| &list.expr) else {
        return Vec::new();
    };
    (items.iter())
        .map(|item| item.expr())
        .take_while(|item| item.span.start < start)
        .map(|item| match &item.expr {
            Expr::FullCellPath(path) if path.tail.is_empty() => &path.head,
            _ => item,
        })
        .filter_map(|item| match &item.expr {
            Expr::Record(pairs) => Some(
                (pairs.iter())
                    .filter_map(|pair| match pair {
                        RecordItem::Pair(key, value) => Some((
                            text(key).trim_matches(['\'', '"', '`']).to_owned(),
                            text(value),
                        )),
                        RecordItem::Spread(..) => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

/// A key offered for a record, `name: `, with what it holds.
fn key(name: &str, description: Option<String>) -> SemanticSuggestion {
    let needs_quotes = name
        .chars()
        .any(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-')));
    let written = if needs_quotes {
        format!("\"{name}\"")
    } else {
        name.to_owned()
    };
    SemanticSuggestion {
        suggestion: Suggestion {
            value: format!("{written}: "),
            display_override: Some(written),
            description,
            ..Suggestion::default()
        },
        kind: Some(SuggestionKind::CellPath),
        ..SemanticSuggestion::default()
    }
}

/// A value offered for a key.
fn value(text: &str, description: Option<String>) -> SemanticSuggestion {
    SemanticSuggestion {
        suggestion: Suggestion {
            value: text.to_owned(),
            description,
            ..Suggestion::default()
        },
        kind: Some(SuggestionKind::Value(Type::Any)),
        ..SemanticSuggestion::default()
    }
}

/// The columns of `ty`, with the type of each.
fn columns_of(ty: &Type) -> Vec<(String, Type)> {
    match ty {
        Type::Table(columns) | Type::Record(columns) => columns.iter().cloned().collect(),
        Type::List(inner) => columns_of(inner),
        _ => Vec::new(),
    }
}

/// The record the parameter of the call around the probe declares, as its fields.
fn declared(probe: &Probe) -> Vec<(String, SyntaxShape)> {
    match probe.narrowest(false).map(|found| &found.what) {
        Some(Expect::Positional {
            shape: SyntaxShape::Record(fields),
            ..
        }) => fields.iter().cloned().collect(),
        _ => Vec::new(),
    }
}

/// What the variable or cell path being assigned the record that starts at `start` holds now.
fn assigned_to(block: &Block, start: usize, ctx: &Context) -> Option<(String, Value)> {
    let working_set = ctx.working_set;
    let mut found: Vec<&Expression> = Vec::new();
    block.flat_map(
        working_set,
        &|expr| match &expr.expr {
            Expr::BinaryOp(lhs, operator, rhs)
                if matches!(
                    operator.expr,
                    Expr::Operator(Operator::Assignment(Assignment::Assign))
                ) && rhs.span.start == start =>
            {
                vec![lhs.as_ref()]
            }
            _ => vec![],
        },
        &mut found,
    );
    let lhs = found.first()?;
    let Expr::FullCellPath(path) = &lhs.expr else {
        return None;
    };
    let value = super::cell_path_completions::eval_cell_path(
        working_set,
        ctx.stack,
        &path.head,
        &path.tail,
        path.head.span,
    )
    .ok()?;
    let written = String::from_utf8_lossy(working_set.get_span_contents(lhs.span)).into_owned();
    Some((written, value))
}

/// What each row is for the closure that `before`, the line up to its `{`, hands rows to: `ls |
/// each `.
fn row_of_closure(before: &str, ctx: &Context) -> Option<Type> {
    let (input, command) = before.rsplit_once('|')?;
    if !matches!(command.trim(), "each" | "par-each" | "each while") {
        return None;
    }
    let rows = evaluate_text(ctx.working_set.permanent_state, ctx.stack, input.trim())?;
    Some(rows.get_type())
}

pub(crate) fn help(
    block: &Block,
    probe: &Probe,
    typed: &str,
    piped: Option<&Type>,
    ctx: &Context,
) -> Option<Vec<SemanticSuggestion>> {
    let (found, keys, value_of) = (probe.found.iter())
        .filter_map(|found| match &found.what {
            Expect::RecordKey { keys, value_of } => Some((found, keys, value_of)),
            _ => None,
        })
        .min_by_key(|(found, ..)| found.within.len())?;
    let record_start = found.within.start;
    // A record that opens a closure's body run per row is a row of what the call is given.
    let per_row = (ctx
        .working_set
        .get_span_contents(Span::new(probe.source.start, record_start)))
    .iter()
    .rposition(|c| *c == b'{')
    .and_then(|outer| typed.get(..outer))
    .filter(|before| {
        typed[before.len() + 1..record_start - probe.source.start]
            .trim_start_matches(|c: char| {
                c == '|' || c == ',' || c == '_' || c == ' ' || c.is_alphanumeric()
            })
            .is_empty()
    })
    .and_then(|before| row_of_closure(before, ctx));
    let row_body = per_row.is_some();
    let piped = piped.cloned().or(per_row);
    let piped = piped.as_ref();
    let beside = siblings(block, record_start, ctx);
    let has = |name: &str| keys.iter().any(|key| key == name);
    let offers = match value_of {
        None => {
            let mut offers: Vec<SemanticSuggestion> = Vec::new();
            let mut offered: Vec<String> = Vec::new();
            let mut offer = |name: &str, description: Option<String>| {
                if !has(name) && !offered.iter().any(|seen| seen == name) {
                    offered.push(name.to_owned());
                    offers.push(key(name, description));
                }
            };
            for (name, _) in beside.iter().flatten() {
                offer(name, None);
            }
            for (name, shape) in declared(probe) {
                offer(&name, Some(shape.to_string()));
            }
            if let Some((path, Value::Record { val, .. })) = assigned_to(block, record_start, ctx) {
                for (name, held) in val.iter() {
                    let setting = path.trim_start_matches("$env.");
                    let docs = config_docs::doc(&format!("{setting}.{name}"));
                    offer(
                        name,
                        Some(docs.map_or_else(|| held.get_type().to_string(), |(first, _)| first)),
                    );
                }
            }
            let merging = (probe.narrowest(false).and_then(|found| found.decl_id()))
                .is_some_and(|decl_id| ctx.working_set.get_decl(decl_id).name() == "merge");
            if merging || row_body || !keys.is_empty() {
                for (name, ty) in piped.map(columns_of).unwrap_or_default() {
                    offer(&name, Some(ty.to_string()));
                }
            }
            offers
        }
        Some(key) => {
            let mut offers: Vec<SemanticSuggestion> = Vec::new();
            let mut offered: Vec<&str> = Vec::new();
            if let Some((_, ty)) = (piped.map(columns_of).unwrap_or_default().into_iter())
                .find(|(name, _)| name == key)
            {
                offers.push(value(&format!("$in.{key}"), Some(ty.to_string())));
            }
            for (_, held) in beside.iter().flatten().filter(|(name, _)| name == key) {
                if !offered.contains(&held.as_str()) {
                    offered.push(held);
                    offers.push(value(held, None));
                }
            }
            offers
        }
    };
    if offers.is_empty() {
        return None;
    }
    let token = ctx.token(found.span);
    Some(ask([Source::Offered(offers)], &token)).filter(|found| !found.is_empty())
}
