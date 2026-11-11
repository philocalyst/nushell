use crate::completions::{
    Candidates, Context, SemanticSuggestion,
    completion_common::surround_remove,
    quote::{Quote, Spelling, quote},
    to_reedline_span,
};
use nu_protocol::{ModuleId, SuggestionKind, engine::StateWorkingSet};
use reedline::Suggestion;

/// The commands, submodules and constants `module_id` exports, found in `module_set` when the
/// module was parsed apart from the line.
pub(crate) fn exports(
    module_id: ModuleId,
    module_set: Option<&StateWorkingSet>,
    ctx: &Context,
) -> Candidates {
    let prefix = ctx.prefix_str();
    let mut candidates = Candidates {
        needle: Some(surround_remove(prefix.as_ref())),
        ..Candidates::default()
    };
    let span = to_reedline_span(ctx.span, ctx.offset);
    // An exported name is spelled as a word of the item list, staying in a quote the user opened.
    let inside = Quote::opened_by(prefix.as_ref());
    let mut push = |name: &str,
                    description: Option<String>,
                    extra: Option<Vec<String>>,
                    kind: SuggestionKind| {
        let value = quote(
            name,
            Spelling {
                inside,
                ..Spelling::default()
            },
        )
        .into_owned();
        // Matched by the name, not by how it is spelled.
        candidates.push_as(
            name,
            SemanticSuggestion {
                suggestion: Suggestion {
                    value,
                    span,
                    description,
                    extra,
                    append_whitespace: true,
                    ..Suggestion::default()
                },
                kind: Some(kind),
                ..Default::default()
            },
        );
    };

    let working_set = module_set.unwrap_or(ctx.working_set);
    let module = working_set.get_module(module_id);

    for (name, decl_id) in &module.decls {
        let command = working_set.get_decl(*decl_id);
        push(
            &String::from_utf8_lossy(name),
            Some(command.description().to_string()),
            None,
            // `None` here avoids arguments being expanded by snippet edit style for lsp
            SuggestionKind::Command(command.command_type(), None),
        );
    }
    for (name, module_id) in &module.submodules {
        let (description, extra) = working_set
            .get_module_comments(*module_id)
            .map(|spans| working_set.build_desc(spans))
            .unzip();
        push(
            &String::from_utf8_lossy(name),
            description.or_else(|| Some("Submodule".into())),
            extra.map(|extra| vec![extra]),
            SuggestionKind::Module,
        );
    }
    for (name, var_id) in &module.constants {
        let variable = working_set.get_variable(*var_id);
        push(
            &String::from_utf8_lossy(name),
            variable
                .const_val
                .as_ref()
                .and_then(|value| value.clone().coerce_into_string().ok()),
            None,
            SuggestionKind::Variable,
        );
    }
    candidates
}
