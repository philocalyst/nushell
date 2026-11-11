//! The type that flows down a pipeline to the cursor.
//!
//! Completion reads it to name the columns of what is piped (`ls | where ⌶`, `$in.⌶`, a closure's
//! `$row.⌶`) and to prefer the commands that take it (`ls | f⌶`). The entry point is
//! [`Flow::nest`]: an element the cursor is in, with what the elements of its pipeline before it
//! leave flowing. For one element:
//!
//! * a builtin's output is the one `input_output_types` pairs with the type flowing in;
//! * a filter that only drops, reorders or adds rows passes the type flowing in through,
//!   columns included ([`KEEPS_ROWS`]);
//! * `select`, `reject` and `get` narrow the columns when their arguments are literal, and `rename`
//!   renames them when the names are;
//! * a custom command that declares no output has the output of the last expression of its body,
//!   read as far as [`BODIES`] commands deep;
//! * a literal, a typed variable, or a value that can be read now has its own type;
//! * the first element of a block is given what flows into the call that runs it (a row of it
//!   where the call runs the block per row, [`PER_ROW`]), and `for x in rows` binds `x` to a row
//!   of `rows`.
//!
//! Where the data itself is read ([`data`](super::data)), [`rows_source`] says which pipeline a
//! variable's rows are from.
//!
//! What is unknown stays [`Type::Any`], which nothing is offered for.

use crate::completions::cell_path_completions::eval_cell_path;
use nu_protocol::{
    CompareTypes, IN_VARIABLE_ID, Span, Type, Value, VarId,
    ast::{
        Block, Call, Expr, Expression, PathMember, PipelineElement, PipelineRedirection, RecordItem,
    },
    engine::{CommandType, Stack, StateWorkingSet},
    record,
};

/// How many custom commands deep an output is read off a body.
const BODIES: usize = 3;

/// Builtin commands whose output rows are those of their input, only fewer, reordered or with
/// more: what they pass on keeps the columns. A command is listed only when it leaves the rows
/// alone; `each`, which has the same declared types, is not.
const KEEPS_ROWS: [&str; 27] = [
    "append",
    "compact",
    "default",
    "drop",
    "drop nth",
    "find",
    "first",
    "inspect",
    "last",
    "move",
    "prepend",
    "reverse",
    "shuffle",
    "skip",
    "skip until",
    "skip while",
    "slice",
    "sort",
    "sort-by",
    "take",
    "take until",
    "take while",
    "tee",
    "uniq",
    "uniq-by",
    "update",
    "where",
];

/// Builtin commands that run their closure or condition once for each row they are given: what
/// the closure's parameter, `$it` and `$in` stand for there is a row.
const PER_ROW: [&str; 17] = [
    "all",
    "any",
    "chunk-by",
    "each",
    "each while",
    "group-by",
    "insert",
    "par-each",
    "reduce",
    "skip until",
    "skip while",
    "sort-by",
    "take until",
    "take while",
    "update",
    "upsert",
    "where",
];

/// A pipeline element the cursor is nested in.
#[derive(Debug, Clone)]
pub(crate) struct Nest<'a> {
    pub element: &'a Expression,
    /// What flows into it from the elements before it, if any do.
    pub input: Option<Type>,
    /// The span of the elements before it in its pipeline, if there are any: the pipeline that
    /// makes what flows into it, which [`data`](super::data) can run to read.
    pub prior: Option<Span>,
    /// The rows its closures and conditions name, by the variable of their first parameter.
    pub rows: Vec<(VarId, Type)>,
    /// The span of the pipeline whose rows `rows` are: the elements before it, or, for the loop
    /// variable of `for`, what it loops over.
    pub source: Option<Span>,
    /// What `$in` stands for at the start of the block this element is in: the pipeline that
    /// makes what flows into the call that runs the block, if one does.
    pub feed: Option<Span>,
}

/// A pipeline whose data is wanted, and the pipeline that `$in` at its start stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Source {
    pub span: Span,
    pub feed: Option<Span>,
}

/// One row of `ty`: the element of a list, or a table's record.
pub(crate) fn row_of(ty: &Type) -> Type {
    match ty {
        Type::List(row) => (**row).clone(),
        Type::Table(columns) => Type::Record(columns.clone()),
        Type::Range => Type::Int,
        _ => Type::Any,
    }
}

/// `place.input` of a completer: the type that is piped into the command under the cursor, and
/// the columns it has, if it has any. A command with nothing piped into it is given `nothing`.
pub(crate) fn input_record(piped: Option<&Type>, span: Span) -> Value {
    let ty = piped.unwrap_or(&Type::Nothing);
    let mut input = record! { "type" => Value::string(ty.to_string(), span) };
    let mut columns = ty;
    while let Type::List(inner) = columns {
        columns = inner;
    }
    if let Type::Table(columns) | Type::Record(columns) = columns
        && !columns.is_empty()
    {
        let rows = columns.iter().map(|(name, ty)| {
            Value::record(
                record! {
                    "name" => Value::string(name, span),
                    "type" => Value::string(ty.to_string(), span),
                },
                span,
            )
        });
        input.insert("columns", Value::list(rows.collect(), span));
    }
    Value::record(input, span)
}

/// Whether `ty` says nothing worth completing from.
pub(crate) fn is_unknown(ty: &Type) -> bool {
    match ty {
        Type::Any => true,
        Type::Table(columns) | Type::Record(columns) => columns.is_empty(),
        Type::List(inner) => is_unknown(inner),
        _ => false,
    }
}

/// Whether `call` is a builtin named one of `names`: a custom command of the same name is not.
fn is_builtin_of(working_set: &StateWorkingSet, call: &Call, names: &[&str]) -> bool {
    let command = working_set.get_decl(call.decl_id);
    matches!(
        command.command_type(),
        CommandType::Builtin | CommandType::Keyword
    ) && names.contains(&command.name())
}

/// Whether `call` is the builtin `let` or `const`, which only bind a variable to a value.
pub(crate) fn is_binding(working_set: &StateWorkingSet, call: &Call) -> bool {
    is_builtin_of(working_set, call, &["let", "const"])
}

/// What flows into the first element of the block that `element` runs, when `outer` flows into
/// `element`: a row of it where the call runs the block per row.
fn into_block(working_set: &StateWorkingSet, element: &Expression, outer: Type) -> Option<Type> {
    match &element.expr {
        Expr::Call(call) if is_builtin_of(working_set, call, &PER_ROW) => Some(row_of(&outer)),
        Expr::Call(_) | Expr::Collect(..) => Some(outer),
        _ => None,
    }
}

/// What flows into the element a nest stands for: from the elements before it, else, for the
/// first of a block, what flows into the call the block belongs to (a row of it where that
/// call runs the block per row).
fn flowing(working_set: &StateWorkingSet, nests: &[Nest], at: usize) -> Option<Type> {
    let nest = nests.get(at)?;
    if let Some(input) = &nest.input {
        return Some(input.clone());
    }
    let outer = flowing(working_set, nests, at.checked_sub(1)?)?;
    into_block(working_set, nests[at - 1].element, outer)
}

/// What flows into the first element of the block the cursor is in, when no element of it is
/// there yet: `nests` ends with the call that runs the block.
pub(crate) fn block_input(working_set: &StateWorkingSet, nests: &[Nest]) -> Option<Type> {
    let last = nests.len().checked_sub(1)?;
    into_block(
        working_set,
        nests[last].element,
        flowing(working_set, nests, last)?,
    )
}

/// What the variable `var_id` stands for where the cursor is nested in `nests`, if it stands for
/// what is piped: `$in`, or the parameter of a closure or condition that runs per row.
pub(crate) fn rows_named(
    working_set: &StateWorkingSet,
    nests: &[Nest],
    var_id: VarId,
) -> Option<Type> {
    if var_id == IN_VARIABLE_ID {
        return flowing(working_set, nests, nests.len().checked_sub(1)?);
    }
    // `$in` of an element that uses it is a variable of its own, which the element collects.
    if let Some(at) = nests
        .iter()
        .rposition(|nest| matches!(nest.element.expr, Expr::Collect(id, _) if id == var_id))
    {
        return flowing(working_set, nests, at);
    }
    // The cursor may be past the condition that declares it (`where size ⌶`).
    (nests.iter().rev().flat_map(|nest| &nest.rows))
        .find(|(id, _)| *id == var_id)
        .map(|(_, ty)| ty.clone())
}

/// The pipeline whose rows the variable `var_id` stands for where the cursor is nested in
/// `nests`, as [`rows_named`] reads their type: the data to read the values of its columns from.
pub(crate) fn rows_source(nests: &[Nest], var_id: VarId) -> Option<Source> {
    if let Some(nest) =
        (nests.iter().rev()).find(|nest| nest.rows.iter().any(|(id, _)| *id == var_id))
    {
        return Some(Source {
            span: nest.source?,
            feed: nest.feed,
        });
    }
    let at = if var_id == IN_VARIABLE_ID {
        nests.len().checked_sub(1)?
    } else {
        nests
            .iter()
            .rposition(|nest| matches!(nest.element.expr, Expr::Collect(id, _) if id == var_id))?
    };
    flowing_from(nests, at)
}

/// The pipeline that makes what flows into the element a nest stands for, as [`flowing`] finds its
/// type.
fn flowing_from(nests: &[Nest], at: usize) -> Option<Source> {
    let nest = nests.get(at)?;
    if nest.input.is_some() {
        return Some(Source {
            span: nest.prior?,
            feed: nest.feed,
        });
    }
    let outer = at.checked_sub(1)?;
    match &nests[outer].element.expr {
        Expr::Call(_) | Expr::Collect(..) => flowing_from(nests, outer),
        _ => None,
    }
}

/// What flows through a pipeline, read off the parse and the engine's state.
pub(crate) struct Flow<'a> {
    working_set: &'a StateWorkingSet<'a>,
    stack: &'a Stack,
    /// The rows the variables in scope stand for, from the closures and conditions around.
    scope: Vec<(VarId, Type)>,
}

impl<'a> Flow<'a> {
    pub(crate) fn new(working_set: &'a StateWorkingSet<'a>, stack: &'a Stack) -> Self {
        Self {
            working_set,
            stack,
            scope: Vec::new(),
        }
    }

    /// This flow, inside the closures and conditions of `nests`.
    pub(crate) fn within_nests(&self, nests: &[Nest]) -> Self {
        Self {
            scope: nests.iter().flat_map(|nest| nest.rows.clone()).collect(),
            ..Self::new(self.working_set, self.stack)
        }
    }

    /// The nest of `element`, an element of `block`, inside the `outer` nests.
    pub(crate) fn nest<'e>(
        &self,
        element: &'e Expression,
        block: &Block,
        outer: &[Nest],
    ) -> Nest<'e> {
        let flow = self.within_nests(outer);
        let (input, prior) = block
            .pipelines
            .iter()
            .find_map(|pipeline| {
                let at = (pipeline.elements.iter())
                    .position(|candidate| std::ptr::eq(&candidate.expr, element))?;
                let before = pipeline.elements.get(..at)?;
                let prior = Span::merge_many(before.iter().flat_map(element_spans));
                Some((
                    flow.flowing_into(&pipeline.elements, at),
                    (!before.is_empty()).then_some(prior),
                ))
            })
            .unwrap_or_default();
        // What `$in` is at the start of the block the element is in.
        let feed = outer.len().checked_sub(1).and_then(|last| {
            into_block(self.working_set, outer[last].element, Type::Any)?;
            flowing_from(outer, last).map(|source| source.span)
        });
        // A closure or condition of a per-row command is handed a row of what flows in, which
        // for the first element of a block is what flows into the block.
        let flowing = input
            .clone()
            .or_else(|| block_input(self.working_set, outer));
        let row = flowing.as_ref().map(row_of);
        let source = prior.or(feed);
        let (rows, source) = match (&element.expr, row) {
            (Expr::Call(call), Some(row)) if is_builtin_of(self.working_set, call, &PER_ROW) => (
                call.positional_iter()
                    .filter_map(|argument| {
                        let (Expr::RowCondition(id) | Expr::Closure(id) | Expr::Block(id)) =
                            argument.expr
                        else {
                            return None;
                        };
                        let signature = &self.working_set.get_block(id).signature;
                        let first = (signature.required_positional.first())
                            .or(signature.optional_positional.first());
                        Some((first?.var_id?, row.clone()))
                    })
                    .collect(),
                source,
            ),
            // `for x in rows { .. }` binds `x` to each of the rows it loops over.
            (Expr::Call(call), _) if is_builtin_of(self.working_set, call, &["for"]) => {
                let looped = call
                    .positional_iter()
                    .find_map(|argument| match &argument.expr {
                        Expr::Keyword(keyword) => Some(&keyword.expr),
                        _ => None,
                    });
                let variable = call
                    .positional_iter()
                    .find_map(|argument| match argument.expr {
                        Expr::VarDecl(id) => Some(id),
                        _ => None,
                    });
                match (variable, looped) {
                    (Some(id), Some(looped)) => (
                        vec![(id, row_of(&flow.within(looped, None, None, BODIES)))],
                        Some(looped.span),
                    ),
                    _ => (Vec::new(), None),
                }
            }
            _ => (Vec::new(), None),
        };
        Nest {
            element,
            input,
            prior,
            rows,
            source,
            feed,
        }
    }

    /// What the elements of `elements` before the one at `index` leave flowing; nothing before
    /// the first.
    pub(crate) fn flowing_into(&self, elements: &[PipelineElement], index: usize) -> Option<Type> {
        let before = elements.get(..index)?;
        self.through(before, None, BODIES)
    }

    /// What `elements` leave flowing, given what `input` flows into the first.
    fn through(
        &self,
        elements: &[PipelineElement],
        input: Option<Type>,
        depth: usize,
    ) -> Option<Type> {
        elements.iter().fold(input, |flowing, element| {
            Some(self.within(&element.expr, flowing.as_ref(), None, depth))
        })
    }

    /// What `element` outputs when `input` is piped into it, reading `depth` more custom
    /// commands' bodies. `collected` is what `$in` stands for in `element`, by the variable the
    /// parser bound it to.
    fn within(
        &self,
        element: &Expression,
        input: Option<&Type>,
        collected: Option<(VarId, &Type)>,
        depth: usize,
    ) -> Type {
        let declared = element.ty.clone();
        match &element.expr {
            Expr::Call(call) => self.call(call, declared, input, depth),
            Expr::Collect(var_id, inner) => {
                let collected = input.map(|input| (*var_id, input));
                self.within(inner, input, collected, depth)
            }
            Expr::Subexpression(block_id) => self
                .body(self.working_set.get_block(*block_id), input, depth)
                .unwrap_or(declared),
            Expr::FullCellPath(path) if is_unknown(&declared) => self
                .variable(&path.head, &path.tail, collected, input)
                .unwrap_or(declared),
            Expr::Var(_) if is_unknown(&declared) => self
                .variable(element, &[], collected, input)
                .unwrap_or(declared),
            _ => declared,
        }
    }

    /// The type of `head` followed by `tail` where `head` is a variable: what `$in` stands for,
    /// else the value the variable has now.
    fn variable(
        &self,
        head: &Expression,
        tail: &[PathMember],
        collected: Option<(VarId, &Type)>,
        input: Option<&Type>,
    ) -> Option<Type> {
        let Expr::Var(var_id) = head.expr else {
            return None;
        };
        let standing = match collected {
            Some((id, ty)) if id == var_id => Some(ty),
            _ if var_id == IN_VARIABLE_ID => input,
            _ => None,
        };
        let named = self.scope.iter().find(|(id, _)| *id == var_id);
        if let Some(ty) = standing.or(named.map(|(_, ty)| ty)) {
            return ty.follow_cell_path(tail).map(|ty| ty.into_owned());
        }
        let value = eval_cell_path(self.working_set, self.stack, head, tail, head.span).ok()?;
        Some(match &value {
            // The first row stands for the rest, so a long list is not read through.
            Value::List { vals, .. } => match vals.first().map(Value::get_type) {
                Some(Type::Record(columns)) => Type::Table(columns),
                Some(row) => Type::list(row),
                None => Type::list(Type::Any),
            },
            value => value.get_type(),
        })
    }

    /// The output of `call`, whose declared output is `declared`.
    fn call(&self, call: &Call, declared: Type, input: Option<&Type>, depth: usize) -> Type {
        let command = self.working_set.get_decl(call.decl_id);
        let builtin = matches!(
            command.command_type(),
            CommandType::Builtin | CommandType::Keyword
        );

        if let Some(output) = self.branches(command.name(), call, input, depth) {
            return output;
        }
        if builtin && command.name() == "ls" && call.get_named_arg("long").is_some() {
            return long_listing(declared);
        }
        if builtin {
            if let Some(input) = input
                && let Some(output) = self.builtin(command.name(), call, input)
            {
                return output;
            }
            let signature = self.working_set.get_signature_shared(call.decl_id);
            // The output the signature pairs with what flows in, when that is known.
            let pair = signature.input_output_types.iter().find(|(accepts, _)| {
                input.map_or(*accepts == Type::Nothing, |input| {
                    input.is_subtype_of(accepts)
                })
            });
            return pair.map_or(declared, |(_, output)| output.clone());
        }

        // A custom command that declares nothing is what its body ends in.
        if is_unknown(&declared)
            && depth > 0
            && let Some(block_id) = command.block_id()
        {
            let body = self.working_set.get_block(block_id);
            return self.body(body, input, depth - 1).unwrap_or(declared);
        }
        declared
    }

    /// The output of the builtin `name` where it is one of the commands that know more than its
    /// signature says about what comes out of `input`.
    fn builtin(&self, name: &str, call: &Call, input: &Type) -> Option<Type> {
        // The column a literal cell path argument starts with.
        let column = |argument: &Expression| match &argument.expr {
            Expr::CellPath(path) => match path.members.first()? {
                PathMember::String { val, .. } => Some(val.clone()),
                PathMember::Int { .. } => None,
            },
            _ => None,
        };
        let literal_columns =
            || -> Option<Vec<String>> { call.positional_iter().map(column).collect() };
        match name {
            // Without a count, a row.
            "first" | "last" if call.positional_iter().next().is_none() => Some(row_of(input)),
            name if KEEPS_ROWS.contains(&name) => Some(input.clone()),
            "insert" | "upsert" => with_column(input, &column(call.positional_iter().next()?)?),
            "enumerate" => Some(Type::Table(
                [("index", Type::Int), ("item", row_of(input))].into(),
            )),
            "select" => with_columns(input, &literal_columns()?, true),
            "reject" => with_columns(input, &literal_columns()?, false),
            "rename" => renamed(input, call),
            "get" => {
                let members = match &call.positional_iter().next()?.expr {
                    Expr::CellPath(path) => &path.members,
                    _ => return None,
                };
                input.follow_cell_path(members).map(|ty| ty.into_owned())
            }
            _ => None,
        }
    }

    /// The output of a command that runs one of its blocks, when every branch it may take agrees:
    /// `if`'s then and else (and `else if`), `match`'s arms, `try` and its `catch`, `do`'s closure.
    /// A branch that says nothing (an arm of unknown type, a `null`) is not counted.
    fn branches(
        &self,
        name: &str,
        call: &Call,
        input: Option<&Type>,
        depth: usize,
    ) -> Option<Type> {
        // The positionals that are branches: after `if`'s condition and `match`'s value, `try`'s
        // body and `catch` (not `finally`), and `do`'s closure (not the values passed to it).
        let (skip, take) = match name {
            "if" | "match" => (1, usize::MAX),
            "try" => (0, 2),
            "do" => (0, 1),
            _ => return None,
        };
        let mut outputs = Vec::new();
        for argument in call.positional_iter().skip(skip).take(take) {
            self.branch(argument, input, depth, &mut outputs);
        }
        let mut known = outputs
            .into_iter()
            .filter(|ty| !is_unknown(ty) && *ty != Type::Nothing);
        let first = known.next()?;
        known.all(|ty| ty == first).then_some(first)
    }

    /// Push the outputs of the branch `expression`, looking through `else`/`catch` and arms.
    fn branch(
        &self,
        expression: &Expression,
        input: Option<&Type>,
        depth: usize,
        outputs: &mut Vec<Type>,
    ) {
        match &expression.expr {
            Expr::Block(id) | Expr::Closure(id) => {
                if let Some(output) = self.body(self.working_set.get_block(*id), input, depth) {
                    outputs.push(output);
                }
            }
            Expr::Keyword(keyword) => self.branch(&keyword.expr, input, depth, outputs),
            Expr::MatchBlock(arms) => {
                for (_, arm) in arms {
                    self.branch(arm, input, depth, outputs);
                }
            }
            _ => outputs.push(self.within(expression, input, None, depth)),
        }
    }

    /// The output of the last pipeline of `block`, when `input` is piped into it.
    fn body(&self, block: &Block, input: Option<&Type>, depth: usize) -> Option<Type> {
        let last = block.pipelines.last()?;
        let input = input.filter(|_| block.pipelines.len() == 1).cloned();
        self.through(&last.elements, input, depth)
    }
}

/// `ls --long`'s rows: the columns `ls` declares (`listing`), then the ones `--long` adds, of
/// which a platform may lack some.
fn long_listing(listing: Type) -> Type {
    let Type::Table(columns) = listing else {
        return listing;
    };
    let mut all: Vec<(String, Type)> = columns.iter().cloned().collect();
    for (name, ty) in [
        ("target", Type::String),
        ("readonly", Type::Bool),
        ("mode", Type::String),
        ("num_links", Type::Int),
        ("inode", Type::Int),
        ("user", Type::String),
        ("group", Type::String),
        ("created", Type::Date),
        ("accessed", Type::Date),
    ] {
        if columns.get(name).is_none() {
            all.push((name.into(), ty));
        }
    }
    Type::Table(all.into())
}

/// `ty` with one more column `name`, of a type nothing is known of, unless it has it.
fn with_column(ty: &Type, name: &str) -> Option<Type> {
    let (Type::Table(columns) | Type::Record(columns)) = ty else {
        return None;
    };
    let mut all: Vec<(String, Type)> = columns.iter().cloned().collect();
    if columns.get(name).is_none() {
        all.push((name.to_owned(), Type::Any));
    }
    Some(match ty {
        Type::Table(_) => Type::Table(all.into()),
        _ => Type::Record(all.into()),
    })
}

/// The spans of `element`: its expression, and what it is redirected to.
fn element_spans(element: &PipelineElement) -> Vec<Span> {
    let targets = match &element.redirection {
        None => Vec::new(),
        Some(PipelineRedirection::Single { target, .. }) => vec![target],
        Some(PipelineRedirection::Separate { out, err }) => vec![out, err],
    };
    let redirected = targets.into_iter().flat_map(|target| {
        [Some(target.span()), target.expr().map(|expr| expr.span)]
            .into_iter()
            .flatten()
    });
    std::iter::once(element.expr.span)
        .chain(redirected)
        .collect()
}

/// The text of a literal string argument.
fn literal_string(argument: &Expression) -> Option<String> {
    match &argument.expr {
        Expr::String(text) | Expr::RawString(text) => Some(text.clone()),
        _ => None,
    }
}

/// `ty` with the columns `rename` renames: those named in `--column {old: new}`, else the first
/// ones, in order, to the names given. `None` where the new names are not literal, as with
/// `--block`, or `ty` has no columns.
fn renamed(ty: &Type, call: &Call) -> Option<Type> {
    let (Type::Table(columns) | Type::Record(columns)) = ty else {
        return None;
    };
    if call.get_named_arg("block").is_some() {
        return None;
    }
    let all = columns.iter().cloned();
    let renamed: Vec<(String, Type)> = if let Some(pairs) = call.get_flag_expr("column") {
        // A record literal is a cell path with no members.
        let record = match &pairs.expr {
            Expr::FullCellPath(path) if path.tail.is_empty() => &path.head,
            _ => pairs,
        };
        let Expr::Record(items) = &record.expr else {
            return None;
        };
        let pairs: Vec<(String, String)> = (items.iter())
            .map(|item| match item {
                RecordItem::Pair(old, new) => Some((literal_string(old)?, literal_string(new)?)),
                RecordItem::Spread(..) => None,
            })
            .collect::<Option<_>>()?;
        all.map(|(name, ty)| {
            let new = pairs.iter().find(|(old, _)| *old == name);
            (new.map_or(name, |(_, new)| new.clone()), ty)
        })
        .collect()
    } else {
        let new: Vec<String> = call
            .positional_iter()
            .map(literal_string)
            .collect::<Option<_>>()?;
        all.enumerate()
            .map(|(at, (name, ty))| (new.get(at).cloned().unwrap_or(name), ty))
            .collect()
    };
    Some(match ty {
        Type::Table(_) => Type::Table(renamed.into()),
        _ => Type::Record(renamed.into()),
    })
}

/// `ty` with only the columns `names` (`keep`), or without them. `None` where `ty` has no
/// columns to choose from.
fn with_columns(ty: &Type, names: &[String], keep: bool) -> Option<Type> {
    let (Type::Table(columns) | Type::Record(columns)) = ty else {
        return None;
    };
    let chosen: Vec<(String, Type)> = if keep {
        names
            .iter()
            .map(|name| {
                let column = columns.get(name).cloned();
                (name.clone(), column.unwrap_or(Type::Any))
            })
            .collect()
    } else {
        columns
            .iter()
            .filter(|(name, _)| !names.contains(name))
            .cloned()
            .collect()
    };
    Some(match ty {
        Type::Table(_) => Type::Table(chosen.into()),
        _ => Type::Record(chosen.into()),
    })
}
