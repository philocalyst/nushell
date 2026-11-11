use crate::completions::{Candidates, Context, SemanticSuggestion, to_reedline_span};
use nu_protocol::{
    ENV_VARIABLE_ID, SuggestionKind, Type, Value,
    ast::{self, Comparison, Expr, Expression},
    engine::StateWorkingSet,
};
use reedline::Suggestion;
use strum::{EnumMessage, IntoEnumIterator};

use super::cell_path_completions::eval_cell_path;

struct OperatorItem {
    pub symbols: String,
    pub description: String,
}

fn operator_to_item<T: EnumMessage + AsRef<str>>(op: T) -> OperatorItem {
    OperatorItem {
        symbols: op.as_ref().into(),
        description: op.get_message().unwrap_or_default().into(),
    }
}

fn common_comparison_ops() -> Vec<OperatorItem> {
    vec![
        operator_to_item(Comparison::Equal),
        operator_to_item(Comparison::NotEqual),
        operator_to_item(Comparison::In),
        operator_to_item(Comparison::NotIn),
    ]
}

fn all_ops_for_immutable() -> Vec<OperatorItem> {
    ast::Comparison::iter()
        .map(operator_to_item)
        .chain(ast::Math::iter().map(operator_to_item))
        .chain(ast::Boolean::iter().map(operator_to_item))
        .chain(ast::Bits::iter().map(operator_to_item))
        .collect()
}

fn collection_comparison_ops() -> Vec<OperatorItem> {
    let mut ops = common_comparison_ops();
    ops.push(operator_to_item(Comparison::Has));
    ops.push(operator_to_item(Comparison::NotHas));
    ops
}

fn number_comparison_ops() -> Vec<OperatorItem> {
    Comparison::iter()
        .filter(|op| {
            !matches!(
                op,
                Comparison::RegexMatch
                    | Comparison::NotRegexMatch
                    | Comparison::StartsWith
                    | Comparison::NotStartsWith
                    | Comparison::EndsWith
                    | Comparison::NotEndsWith
                    | Comparison::Has
                    | Comparison::NotHas
                    | Comparison::OlderThan
                    | Comparison::NewerThan
            )
        })
        .map(operator_to_item)
        .collect()
}

/// Comparisons offered after a datetime: the numeric ones plus `older-than` and `newer-than`.
fn date_comparison_ops() -> Vec<OperatorItem> {
    let mut ops = number_comparison_ops();
    ops.push(operator_to_item(Comparison::OlderThan));
    ops.push(operator_to_item(Comparison::NewerThan));
    ops
}

fn math_ops() -> Vec<OperatorItem> {
    ast::Math::iter()
        .filter(|op| !matches!(op, ast::Math::Concatenate | ast::Math::Pow))
        .map(operator_to_item)
        .collect()
}

fn bit_ops() -> Vec<OperatorItem> {
    ast::Bits::iter().map(operator_to_item).collect()
}

fn all_assignment_ops() -> Vec<OperatorItem> {
    ast::Assignment::iter().map(operator_to_item).collect()
}

fn numeric_assignment_ops() -> Vec<OperatorItem> {
    ast::Assignment::iter()
        .filter(|op| !matches!(op, ast::Assignment::ConcatenateAssign))
        .map(operator_to_item)
        .collect()
}

fn concat_assignment_ops() -> Vec<OperatorItem> {
    vec![
        operator_to_item(ast::Assignment::Assign),
        operator_to_item(ast::Assignment::ConcatenateAssign),
    ]
}

fn valid_int_ops() -> Vec<OperatorItem> {
    let mut ops = valid_float_ops();
    ops.extend(bit_ops());
    ops
}

fn valid_float_ops() -> Vec<OperatorItem> {
    let mut ops = valid_value_with_unit_ops();
    ops.push(operator_to_item(ast::Math::Pow));
    ops
}

/// Comparisons offered after a string. Ordering a string (`<`, `>`) is legal but rarely meant, so
/// it is left for the user to type; dates have `older-than` and `newer-than` instead.
fn valid_string_ops() -> Vec<OperatorItem> {
    let mut ops: Vec<OperatorItem> = Comparison::iter()
        .filter(|op| {
            !matches!(
                op,
                Comparison::OlderThan
                    | Comparison::NewerThan
                    | Comparison::LessThan
                    | Comparison::GreaterThan
                    | Comparison::LessThanOrEqual
                    | Comparison::GreaterThanOrEqual
            )
        })
        .map(operator_to_item)
        .collect();
    ops.push(operator_to_item(ast::Math::Concatenate));
    ops.push(OperatorItem {
        symbols: "like".into(),
        description: Comparison::RegexMatch
            .get_message()
            .unwrap_or_default()
            .into(),
    });
    ops.push(OperatorItem {
        symbols: "not-like".into(),
        description: Comparison::NotRegexMatch
            .get_message()
            .unwrap_or_default()
            .into(),
    });
    ops
}

fn valid_list_ops() -> Vec<OperatorItem> {
    let mut ops = collection_comparison_ops();
    ops.push(operator_to_item(ast::Math::Concatenate));
    ops
}

fn valid_binary_ops() -> Vec<OperatorItem> {
    let mut ops = number_comparison_ops();
    ops.extend(bit_ops());
    ops.push(operator_to_item(ast::Math::Concatenate));
    ops
}

fn valid_bool_ops() -> Vec<OperatorItem> {
    let mut ops: Vec<OperatorItem> = ast::Boolean::iter().map(operator_to_item).collect();
    ops.extend(common_comparison_ops());
    ops
}

fn valid_value_with_unit_ops() -> Vec<OperatorItem> {
    let mut ops = number_comparison_ops();
    ops.extend(math_ops());
    ops
}

fn ops_by_value(value: &Value, mutable: bool) -> Vec<OperatorItem> {
    let mut ops = match value {
        Value::Int { .. } => valid_int_ops(),
        Value::Float { .. } => valid_float_ops(),
        Value::String { .. } => valid_string_ops(),
        Value::Binary { .. } => valid_binary_ops(),
        Value::Bool { .. } => valid_bool_ops(),
        Value::Date { .. } => date_comparison_ops(),
        Value::Filesize { .. } | Value::Duration { .. } => valid_value_with_unit_ops(),
        Value::Range { .. } | Value::Record { .. } => collection_comparison_ops(),
        Value::List { .. } => valid_list_ops(),
        _ => all_ops_for_immutable(),
    };
    if mutable {
        ops.extend(match value {
            Value::Int { .. }
            | Value::Float { .. }
            | Value::Filesize { .. }
            | Value::Duration { .. } => numeric_assignment_ops(),
            Value::String { .. } | Value::Binary { .. } | Value::List { .. } => {
                concat_assignment_ops()
            }
            Value::Bool { .. }
            | Value::Date { .. }
            | Value::Range { .. }
            | Value::Record { .. } => vec![operator_to_item(ast::Assignment::Assign)],
            _ => all_assignment_ops(),
        })
    }
    ops
}

fn is_expression_mutable(expr: &Expr, working_set: &StateWorkingSet) -> bool {
    // A cell path's head (`$foo.bar`) and a bare `$foo` left-hand side refer to
    // the same variable, so both count as mutable here.
    let id = match expr {
        Expr::Var(id) => *id,
        Expr::FullCellPath(path) => match path.head.expr {
            Expr::Var(id) => id,
            _ => return false,
        },
        _ => return false,
    };
    if id == ENV_VARIABLE_ID {
        return true;
    }
    let var = working_set.get_variable(id);
    var.mutable
}

/// What may join a finished comparison to the next.
pub(crate) fn connectors(ctx: &Context) -> Candidates {
    let mut candidates = Candidates {
        ordered: true,
        typos: false,
        needle: Some(ctx.prefix_str().into_owned()),
        ..Candidates::default()
    };
    let span = to_reedline_span(ctx.span, ctx.offset);
    use ast::Boolean::{And, Or, Xor};
    for op in [And, Or, Xor] {
        let OperatorItem {
            symbols,
            description,
        } = operator_to_item(op);
        candidates.push_as(
            symbols.clone(),
            SemanticSuggestion {
                suggestion: Suggestion {
                    value: symbols,
                    description: Some(description),
                    span,
                    append_whitespace: true,
                    ..Suggestion::default()
                },
                kind: Some(SuggestionKind::Operator),
                ..Default::default()
            },
        );
    }
    candidates
}

/// The operators that may follow `left_hand_side`, by its type, or by its value when the type is
/// not known.
pub(crate) fn operators(left_hand_side: &Expression, ctx: &Context) -> Candidates {
    // After a comparison a condition goes on with a connector.
    if let Expr::BinaryOp(_, op, _) = &left_hand_side.expr
        && matches!(op.expr, Expr::Operator(ast::Operator::Comparison(_)))
    {
        return connectors(ctx);
    }
    let working_set = ctx.working_set;
    let stack = ctx.stack;
    // Kept in the order they are listed, which puts the equality that is nearly always meant
    // first, rather than the alphabetical order that starts with `!=`.
    let mut candidates = Candidates {
        ordered: true,
        ..Candidates::default()
    };
    let mut needs_assignment_ops = true;
    // A column of the rows piped in (`size` in `ls | where size`) has the type the rows give it.
    let column_type = match (&left_hand_side.expr, ctx.rows) {
        (Expr::FullCellPath(path), Some(rows)) => {
            rows.follow_cell_path(&path.tail).map(|ty| ty.into_owned())
        }
        _ => None,
    };
    let left_hand_side_type = column_type.as_ref().unwrap_or(&left_hand_side.ty);
    // Complete according expression type
    // TODO: type inference on left_hand_side to get more accurate completions
    let mut possible_operations: Vec<OperatorItem> = match left_hand_side_type {
        Type::Int | Type::Number => valid_int_ops(),
        Type::Float => valid_float_ops(),
        Type::String => valid_string_ops(),
        Type::Binary => valid_binary_ops(),
        Type::Bool => valid_bool_ops(),
        Type::Date => date_comparison_ops(),
        Type::Filesize | Type::Duration => valid_value_with_unit_ops(),
        Type::Record(_) | Type::Range => collection_comparison_ops(),
        Type::List(_) | Type::Table(_) => valid_list_ops(),
        // Unknown type, resort to evaluated values
        Type::Any | Type::OneOf(_) => match &left_hand_side.expr {
            Expr::FullCellPath(path) => {
                // for `$ <tab>`
                if let Expr::Garbage = path.head.expr {
                    return candidates;
                }
                let value =
                    eval_cell_path(working_set, stack, &path.head, &path.tail, path.head.span)
                        .unwrap_or_default();
                let mutable = is_expression_mutable(&left_hand_side.expr, working_set);
                // to avoid duplication
                needs_assignment_ops = false;
                ops_by_value(&value, mutable)
            }
            _ => all_ops_for_immutable(),
        },
        _ => common_comparison_ops(),
    };
    // If the left hand side is a variable, add assignment operators if mutable
    if needs_assignment_ops && is_expression_mutable(&left_hand_side.expr, working_set) {
        possible_operations.extend(match left_hand_side_type {
            Type::Int | Type::Float | Type::Number => numeric_assignment_ops(),
            Type::Filesize | Type::Duration => numeric_assignment_ops(),
            Type::String | Type::Binary | Type::List(_) => concat_assignment_ops(),
            Type::Any => all_assignment_ops(),
            _ => vec![operator_to_item(ast::Assignment::Assign)],
        });
    }

    let span = to_reedline_span(ctx.span, ctx.offset);
    for OperatorItem {
        symbols,
        description,
    } in possible_operations
    {
        candidates.push(SemanticSuggestion {
            suggestion: Suggestion {
                // Already owned — moved, not cloned.
                value: symbols,
                description: Some(description),
                span,
                append_whitespace: true,
                ..Suggestion::default()
            },
            kind: Some(SuggestionKind::Operator),
            ..Default::default()
        });
    }
    candidates
}
