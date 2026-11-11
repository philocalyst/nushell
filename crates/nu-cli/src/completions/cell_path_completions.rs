use std::borrow::Cow;

use crate::completions::{
    Candidates, Context, Origin, SemanticSuggestion,
    data::{column_summary, evaluate, rows},
    taking, to_reedline_span,
};
use nu_engine::{column::get_columns, eval_variable};
use nu_protocol::{
    CompletionSort, ShellError, Span, SuggestionKind, Type, Value,
    ast::{Expr, Expression, FullCellPath, PathMember},
    casing::Casing,
    engine::{Stack, StateWorkingSet},
    eval_const::eval_constant,
};
use reedline::Suggestion;

use super::quote::{Spelling, quote};

/// The typed portion of `member` up to the cursor, and the span that portion occupies.
fn prefix_from_path_member(member: &PathMember, cursor: usize) -> (String, Span) {
    let (val, start) = match member {
        PathMember::String { val, span, .. } => (val, span.start),
        PathMember::Int { val, span, .. } => (&val.to_string(), span.start),
    };
    let prefix_str = val.get(..cursor - start).unwrap_or(val);
    (prefix_str.to_string(), Span::new(start, cursor))
}

/// The members that may follow the ones of `full_cell_path` before `cursor` (absolute, in
/// working-set coordinates), matched against the member the cursor is in.
pub(crate) fn cell_path(full_cell_path: &FullCellPath, cursor: usize, ctx: &Context) -> Candidates {
    let mut prefix = String::new();
    // Completing at a dot with no partial member yet: empty span at the cursor.
    let mut span = Span::new(cursor, cursor);
    let mut members_before = 0;
    for member in full_cell_path.tail.iter() {
        if member.span().end < cursor {
            members_before += 1;
        } else if member.span().contains(cursor) || member.span().end == cursor {
            (prefix, span) = prefix_from_path_member(member, cursor);
            break;
        }
    }

    let current_span = to_reedline_span(span, ctx.offset);
    let mut candidates = Candidates {
        needle: Some(prefix),
        ..Candidates::default()
    };
    let path_members = full_cell_path
        .tail
        .get(0..members_before)
        .unwrap_or_default();
    // A head that stands for the rows piped in has no value to read, only a type.
    let by_type = |head: &Type| {
        head.follow_cell_path(path_members)
            .map(|ty| get_suggestions_by_type(&ty, current_span))
            .unwrap_or_default()
    };
    let found = if let Some(rows) = ctx.rows {
        keep_column_order(&mut candidates, ctx);
        // The rows are read when the pipeline they are from can be, else their type is all there is.
        read_rows(path_members, current_span, ctx).unwrap_or_else(|| by_type(rows))
    } else if let Ok(value) = eval_cell_path(
        ctx.working_set,
        ctx.stack,
        &full_cell_path.head,
        path_members,
        span,
    ) {
        let mut found = get_suggestions_by_value(&value, current_span);
        if matches!(full_cell_path.head.expr, Expr::Var(id) if id == nu_protocol::ENV_VARIABLE_ID) {
            documented(&mut found, path_members);
        }
        found
    } else if let Some(found) = read_head(full_cell_path, path_members, current_span, ctx) {
        found
    } else {
        by_type(&full_cell_path.head.ty)
    };
    for suggestion in found {
        candidates.push(suggestion);
    }
    candidates
}

/// Gives the members of `$env` after `path_members` that the shell documents, settings such as
/// `config.completions.quick`, what its documentation says: the first line after their type in
/// their description, for the hint line, and the rest as their preview.
fn documented(found: &mut [SemanticSuggestion], path_members: &[PathMember]) {
    let before: Vec<String> = path_members
        .iter()
        .map(|member| member.to_string())
        .collect();
    for member in found {
        let path = [&before[..], std::slice::from_ref(&member.suggestion.value)]
            .concat()
            .join(".");
        let Some((first, rest)) = super::config_docs::doc(&path) else {
            continue;
        };
        let ty = member.suggestion.description.take().unwrap_or_default();
        member.suggestion.description = Some(format!("{ty} · {first}"));
        if !rest.is_empty() {
            member.preview = Some(Value::string(rest, Span::unknown()));
        }
    }
}

/// The members that follow `path_members` in the rows of the data that `ctx.rows` stands for, if
/// the pipeline they are from can be run: see [`data`](super::data).
fn read_rows(
    path_members: &[PathMember],
    span: reedline::Span,
    ctx: &Context,
) -> Option<Vec<SemanticSuggestion>> {
    let data = evaluate(ctx, ctx.prior?)?;
    let members: Vec<Value> = (rows(&data).iter())
        .filter_map(|row| row.follow_cell_path(path_members).ok())
        .map(Cow::into_owned)
        .collect();
    Some(get_suggestions_by_data(
        &Value::list(members, data.span()),
        span,
    ))
    .filter(|found| !found.is_empty())
}

/// The members that follow `path_members` in the data of a head that is a subexpression, such as
/// `(open Cargo.toml).package.⌶`, if the subexpression can be run: see [`data`](super::data).
fn read_head(
    full_cell_path: &FullCellPath,
    path_members: &[PathMember],
    span: reedline::Span,
    ctx: &Context,
) -> Option<Vec<SemanticSuggestion>> {
    let head = &full_cell_path.head;
    if !matches!(head.expr, Expr::Subexpression(_)) {
        return None;
    }
    let data = evaluate(ctx, head.span)?;
    let value = data.follow_cell_path(path_members).ok()?;
    Some(get_suggestions_by_data(&value, span)).filter(|found| !found.is_empty())
}

/// Follow cell path to get the value
/// NOTE: This is a relatively lightweight implementation,
/// so it may fail to get the exact value when the expression is complicated.
/// One failing example would be `[$foo].0`
pub(crate) fn eval_cell_path(
    working_set: &StateWorkingSet,
    stack: &Stack,
    head: &Expression,
    path_members: &[PathMember],
    span: Span,
) -> Result<Value, ShellError> {
    // evaluate the head expression to get its value
    let head_value = if let Expr::Var(var_id) = head.expr {
        working_set
            .get_variable(var_id)
            .const_val
            .to_owned()
            .map_or_else(
                // Handles `$env` / `$nu` / `$ans` specials as well as stack vars.
                // Read `$ans` without deferring truncation warnings (completions are not
                // a user-facing display of the value).
                || {
                    if var_id == nu_protocol::LAST_VARIABLE_ID {
                        stack.get_var(var_id, span)
                    } else {
                        eval_variable(working_set.permanent_state, stack, var_id, span)
                    }
                },
                Ok,
            )
    } else {
        eval_constant(working_set, head)
    }?;
    head_value
        .follow_cell_path(path_members)
        .map(Cow::into_owned)
}

/// A member of a cell path of type `ty`, described by its type. A space follows it once it is
/// taken unless the path may go on into it.
fn member_suggestion(
    name: &str,
    description: Option<String>,
    ty: Option<&Type>,
    span: reedline::Span,
) -> SemanticSuggestion {
    // A member is bare only when it is a plain name; a number is an index.
    let value = quote(
        name,
        Spelling {
            member: true,
            ..Spelling::default()
        },
    )
    .into_owned();

    SemanticSuggestion {
        suggestion: Suggestion {
            value,
            span,
            description,
            append_whitespace: ty.is_none_or(|ty| !taking::has_members(ty)),
            ..Suggestion::default()
        },
        kind: Some(SuggestionKind::CellPath),
        ..Default::default()
    }
}

/// Offer the columns in the order the table has them until something is typed, unless the
/// completions are configured to be alphabetical.
fn keep_column_order(candidates: &mut Candidates, ctx: &Context) {
    let nothing_typed = candidates.needle.as_deref().is_none_or(str::is_empty);
    candidates.sort = !nothing_typed || ctx.options.sort == CompletionSort::Alphabetical;
}

/// The columns that may be named at the token, which is a cell path whose last member is the one
/// typed: `get a.b⌶` offers what follows `a`. It may also be a record, whose first key is the
/// column (`rename --column {na⌶`). `Context::rows` and `Context::prior` are set where an argument
/// names columns of what is piped in (`select`, `where`), so nothing is offered without them. The
/// data of the pipeline before the cursor, if it can be read, names them, with a sample of each;
/// else the type that flows does.
pub(crate) fn columns(ctx: &Context) -> Candidates {
    let mut candidates = Candidates::default();
    if ctx.rows.is_none() && ctx.prior.is_none() {
        return candidates;
    }
    let token = ctx.prefix_str();
    let mut typed: &str = &token;
    if let Some(keys) = typed.strip_prefix('{') {
        // A key is the text after the last `{` or `,`, and a value follows its `:`.
        let key = keys.rsplit(',').next().unwrap_or_default();
        if key.contains(':') {
            return candidates;
        }
        typed = key.trim_start();
    }
    let skipped = token.len() - typed.len();
    let (before, needle) = match typed.rsplit_once('.') {
        Some((before, needle)) => (Some(before), needle),
        None => (None, typed),
    };
    let members: Vec<PathMember> = before
        .into_iter()
        .flat_map(|before| before.split('.'))
        .map(|member| match member.parse() {
            Ok(index) => PathMember::int(index, false, Span::unknown()),
            Err(_) => {
                PathMember::string(member.to_owned(), false, Casing::Sensitive, Span::unknown())
            }
        })
        .collect();
    // Only the member typed last is replaced.
    let start = ctx.span.start + skipped + before.map_or(0, |before| before.len() + 1);
    let span = to_reedline_span(Span::new(start.min(ctx.span.end), ctx.span.end), ctx.offset);

    candidates.needle = Some(needle.to_owned());
    keep_column_order(&mut candidates, ctx);
    let read = ctx
        .prior
        .and_then(|prior| evaluate(ctx, prior))
        .and_then(|data| {
            let found = get_suggestions_by_data(&*data.follow_cell_path(&members).ok()?, span);
            Some(found).filter(|found| !found.is_empty())
        });
    let found = read.unwrap_or_else(|| {
        let ty = ctx.rows.and_then(|rows| rows.follow_cell_path(&members));
        ty.map(|ty| get_suggestions_by_type(&ty, span))
            .unwrap_or_default()
    });
    for suggestion in found {
        candidates.push(suggestion);
    }
    candidates
}

/// The members of `value` that is read from a pipeline, each described by its type and previewed
/// by what its cells hold: the keys of a record, or the columns the rows of a table have between
/// them.
fn get_suggestions_by_data(value: &Value, span: reedline::Span) -> Vec<SemanticSuggestion> {
    let mut found: Vec<(String, &Value)> = Vec::new();
    let records = match value {
        Value::Record { .. } => std::slice::from_ref(value),
        Value::List { .. } => rows(value),
        _ => return Vec::new(),
    };
    for record in records.iter().filter_map(|row| row.as_record().ok()) {
        for (key, cell) in record.iter() {
            if !found.iter().any(|(known, _)| known == key) {
                found.push((key.clone(), cell));
            }
        }
    }
    (found.into_iter())
        .map(|(key, cell)| {
            let ty = cell.get_type();
            let mut member =
                member_suggestion(&key, Some(ty.get_non_specified_string()), Some(&ty), span);
            member.preview = column_summary(records, &key);
            member.origin = Origin::Derived;
            member
        })
        .collect()
}

fn get_suggestions_by_value(
    value: &Value,
    current_span: reedline::Span,
) -> Vec<SemanticSuggestion> {
    let to_suggestion = |s: String, v: Option<&Value>| {
        let ty = v.map(Value::get_type);
        member_suggestion(
            &s,
            ty.as_ref().map(Type::to_string),
            ty.as_ref(),
            current_span,
        )
    };
    match value {
        Value::Record { val, .. } => val
            .columns()
            .map(|s| to_suggestion(s.to_string(), val.get(s)))
            .collect(),
        Value::List { vals, .. } => get_columns(vals.as_slice())
            .into_iter()
            .map(|s| {
                let sub_val = vals
                    .first()
                    .and_then(|v| v.as_record().ok())
                    .and_then(|rv| rv.get(&s));
                to_suggestion(s, sub_val)
            })
            .collect(),
        Value::Custom { val, .. } => match val.type_name().as_str() {
            "semver" => ["major", "minor", "patch", "pre", "build", "prefix"]
                .into_iter()
                .map(|s| to_suggestion(s.to_string(), None))
                .collect(),
            "matrix" => ["shape", "ndim", "size"]
                .into_iter()
                .map(|s| to_suggestion(s.to_string(), None))
                .collect(),
            _ => vec![],
        },
        _ => vec![],
    }
}

fn get_suggestions_by_type(ty: &Type, current_span: reedline::Span) -> Vec<SemanticSuggestion> {
    match ty {
        Type::Record(columns) | Type::Table(columns) => columns
            .iter()
            .map(|(name, ty)| member_suggestion(name, Some(ty.to_string()), Some(ty), current_span))
            .collect(),
        Type::List(inner) => get_suggestions_by_type(inner, current_span),
        _ => vec![],
    }
}
