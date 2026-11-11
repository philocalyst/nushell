use crate::completions::{Candidates, Context, SemanticSuggestion, to_reedline_span};
use nu_protocol::SuggestionKind;
use reedline::Suggestion;

/// The `attr` commands, named without the `attr ` that `@` stands for.
pub(crate) fn attributes(ctx: &Context) -> Candidates {
    let mut candidates = Candidates::default();
    let span = to_reedline_span(ctx.span, ctx.offset);

    for (decl_id, name, description, ty) in ctx
        .working_set
        .find_commands_by_predicate(|s| s.starts_with(b"attr "), true)
    {
        let name = name.strip_prefix(b"attr ").unwrap_or(&name);
        candidates.push(SemanticSuggestion {
            suggestion: Suggestion {
                value: String::from_utf8_lossy(name).into_owned(),
                description,
                span,
                append_whitespace: false,
                ..Default::default()
            },
            kind: Some(SuggestionKind::Command(ty, Some(decl_id))),
            ..Default::default()
        });
    }
    candidates
}

/// The definitions an attribute may precede.
pub(crate) fn attributables(ctx: &Context) -> Candidates {
    let working_set = ctx.working_set;
    let mut candidates = Candidates::default();
    let span = to_reedline_span(ctx.span, ctx.offset);

    for name in ["def", "extern", "export def", "export extern"] {
        // The builtins exist in every engine; one that does not has nothing to offer.
        let Some(decl_id) = working_set.find_decl(name.as_bytes()) else {
            continue;
        };
        let command = working_set.get_decl(decl_id);
        candidates.push(SemanticSuggestion {
            suggestion: Suggestion {
                value: command.name().into(),
                description: Some(command.description().into()),
                span,
                append_whitespace: true,
                ..Default::default()
            },
            // The declaration is for snippet completion in the LSP.
            kind: Some(SuggestionKind::Command(
                command.command_type(),
                Some(decl_id),
            )),
            ..Default::default()
        });
    }
    candidates
}
