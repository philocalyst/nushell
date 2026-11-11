use super::completer::touches;
use crate::completions::{
    Candidates, Context, SemanticSuggestion, Source, to_reedline_span, usage::Used,
};
use nu_parser::parse_module_file_or_dir;
use nu_protocol::{
    DynamicCompletionCallRef, ModuleId, Span, SyntaxShape,
    ast::{Argument, Call, Expr, Expression, ListItem},
    engine::{ArgType, StateWorkingSet},
};

/// The argument of a call that is being completed.
pub(crate) struct ArgValueCompletion<'a> {
    pub call: &'a Call,
    pub arg_type: ArgType<'a>,
    /// Index into `call.arguments`, or `.len()` for a trailing slot with no argument.
    pub arg_idx: usize,
    /// The `SyntaxShape` this argument is declared with in the command's signature, if
    /// known. Used to pick a type-specific fallback (e.g. directories for `cd <tab>`) when
    /// the slot is still empty and so has no parsed expression to read the shape from.
    pub declared_shape: Option<SyntaxShape>,
    /// Cursor, in absolute working-set (span) coordinates.
    pub cursor: usize,
}

impl<'a> ArgValueCompletion<'a> {
    /// What a plugin offers for the argument, or `None` where the command has no completion
    /// of its own (or it failed) and the type's fallback applies.
    pub(crate) fn dynamic(&self, context: &Context) -> Option<Candidates> {
        let working_set = context.working_set;
        let declaration = working_set.get_decl(self.call.decl_id);
        let mut stack = context.stack.to_owned();

        let dynamic_completion_call = DynamicCompletionCallRef {
            call: self.call,
            // No sentinel is appended to the buffer, so nothing to strip.
            strip: false,
            pos: self.cursor,
        };

        let completion_result = declaration.get_dynamic_completion(
            working_set.permanent_state,
            &mut stack,
            dynamic_completion_call,
            &self.arg_type,
            #[expect(deprecated, reason = "internal usage")]
            nu_protocol::engine::ExperimentalMarker,
        );

        match completion_result {
            Ok(Some(items)) => {
                let mut candidates = Candidates {
                    used: Some(Used::Argument),
                    ..Candidates::default()
                };
                for suggestion_item in items {
                    // The span is untrusted user/plugin input; fall back to the argument's
                    // own span unless it is well-formed and within the working set.
                    let result_span = suggestion_item
                        .span
                        .filter(|span| span.start >= context.offset && span.end >= span.start)
                        .unwrap_or(context.span);
                    candidates.push(SemanticSuggestion::from_dynamic_suggestion(
                        suggestion_item,
                        to_reedline_span(result_span, context.offset),
                        None,
                    ));
                }
                Some(candidates)
            }
            Ok(None) => None, // fallback to type based completion, file completion, etc.
            Err(error) => {
                log::error!(
                    "error on fetching dynamic suggestion on {} with {:?}: {error}",
                    declaration.name(),
                    self.arg_type
                );
                None
            }
        }
    }

    /// Whether the argument names columns of the rows piped in: a cell path, or a condition on a
    /// row.
    pub(crate) fn names_columns(&self) -> bool {
        fn names(shape: &SyntaxShape) -> bool {
            match shape {
                SyntaxShape::CellPath | SyntaxShape::RowCondition => true,
                SyntaxShape::OneOf(shapes) => shapes.iter().any(names),
                SyntaxShape::List(shape) | SyntaxShape::Keyword(_, shape) => names(shape),
                _ => false,
            }
        }
        self.declared_shape.as_ref().is_some_and(names)
    }

    /// The parsed expression of the argument being completed, if any.
    fn arg_expr(&self) -> Option<&Expr> {
        self.call
            .arguments
            .get(self.arg_idx)
            .and_then(|argument| argument.expr())
            .map(|expression_wrapper| &expression_wrapper.expr)
    }

    /// The files or directories that the argument's type calls for, which follow what the
    /// command offers. A parsed path or glob decides at once. Otherwise the declared type does,
    /// even once the slot holds text (issue #18862): a path, directory or glob type, or a
    /// `oneof` holding one, wants paths. So does an untyped argument when `untyped_may_be_path`,
    /// since it may be one. Any other type, `string` included, never lists the directory.
    pub(crate) fn path_source(&self, untyped_may_be_path: bool) -> Option<Source<'static>> {
        match self.arg_expr() {
            Some(Expr::Directory(_, _)) => return Some(Source::Paths { directories: true }),
            Some(Expr::Filepath(_, _) | Expr::GlobPattern(_, _)) => {
                return Some(Source::Paths { directories: false });
            }
            _ => {}
        }
        match self.declared_shape.as_ref() {
            None | Some(SyntaxShape::Any) => {
                untyped_may_be_path.then_some(Source::Paths { directories: false })
            }
            Some(shape) => paths_for(shape),
        }
    }

    /// The exports of the module the call imports from, for the item the cursor is in.
    pub(crate) fn module_exports(&self, context: &Context<'a>) -> Source<'a> {
        let working_set = context.working_set;
        let Some((module_name, span)) = self.find_module_name_and_span() else {
            return Source::Nothing;
        };
        let Some((module, module_set)) = Self::resolve_module(working_set, module_name, span)
        else {
            return Source::Nothing;
        };

        let at = match self.arg_expr() {
            // `[a, b<tab>]`: complete against the bracketed import list.
            Some(Expr::FullCellPath(full_cell_path)) => match &full_cell_path.head.expr {
                Expr::List(items) => Some(self.list_item(items, context)),
                _ => return Source::Nothing,
            },
            // No expression at all or any other scalar shape (`Expr::String`, plus `Expr::Nothing`
            // for the `null` keyword): search exports by the raw prefix text.
            _ => None,
        };

        Source::Exports {
            module,
            module_set: module_set.map(Box::new),
            at,
        }
    }

    fn find_module_name_and_span(&self) -> Option<(&[u8], Span)> {
        // Only the first positional is the module path, not later import names.
        let first = self.call.arguments.iter().find_map(|a| {
            if let Argument::Positional(e) = a {
                Some(e)
            } else {
                None
            }
        })?;

        if let Expression {
            expr: Expr::String(name),
            span,
            ..
        } = first
        {
            Some((name.as_bytes(), *span))
        } else {
            None
        }
    }

    /// The module, and the working set that holds it when it is a file the line does not load.
    fn resolve_module<'set>(
        working_set: &'set StateWorkingSet,
        module_name: &[u8],
        span: Span,
    ) -> Option<(ModuleId, Option<StateWorkingSet<'set>>)> {
        if let Some(module_id) = working_set.find_module(module_name) {
            return Some((module_id, None));
        }

        let mut temp_working_set = StateWorkingSet::new(working_set.permanent_state);
        let module_id = parse_module_file_or_dir(&mut temp_working_set, module_name, span, None)?;

        Some((module_id, Some(temp_working_set)))
    }

    /// The span and typed text of the item of `items` the cursor is in.
    fn list_item(&self, items: &[ListItem], context: &Context<'a>) -> (Span, &'a [u8]) {
        let item_span = items
            .iter()
            .map(|item| item.expr().span)
            .find(|item_span| touches(*item_span, self.cursor))
            .unwrap_or(Span::point(self.cursor));

        // The member's typed text is the tail of `context.prefix`, from the member's start
        // onward; the cursor-sliced buffer already ends it.
        let relative_offset = item_span.start.saturating_sub(context.span.start);
        (
            Span::new(item_span.start, context.span.end.min(item_span.end)),
            context.prefix.get(relative_offset..).unwrap_or_default(),
        )
    }
}

/// The paths a value of `shape` may name: directories for a directory, any path for a file
/// path, a glob or an argument of an external program, and the widest of those a `oneof` allows.
fn paths_for(shape: &SyntaxShape) -> Option<Source<'static>> {
    match shape {
        SyntaxShape::Directory => Some(Source::Paths { directories: true }),
        // What a `def --wrapped` passes on to the program it wraps is anything a program takes.
        SyntaxShape::Filepath | SyntaxShape::GlobPattern | SyntaxShape::ExternalArgument => {
            Some(Source::Paths { directories: false })
        }
        SyntaxShape::OneOf(shapes) => shapes
            .iter()
            .filter_map(paths_for)
            .min_by_key(|source| matches!(source, Source::Paths { directories: true })),
        _ => None,
    }
}
