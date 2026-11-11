use crate::completions::{Candidates, Context, SemanticSuggestion, to_reedline_span};
use nu_protocol::SuggestionKind;
use reedline::Suggestion;

/// The names of the environment variables, each that the shell documents with what its
/// documentation says: the first line as its description, the rest once its preview unfolds.
pub(crate) fn env_vars(ctx: &Context) -> Candidates {
    let mut candidates = Candidates::default();
    let span = to_reedline_span(ctx.span, ctx.offset);

    for name in ctx.stack.get_env_var_names(ctx.working_set.permanent_state) {
        let (description, help) = match super::config_docs::doc(&name) {
            Some((first, rest)) => (
                Some(first),
                Some(rest)
                    .filter(|rest| !rest.is_empty())
                    .map(|rest| nu_protocol::Value::string(rest, nu_protocol::Span::unknown())),
            ),
            None => (None, None),
        };
        candidates.push(SemanticSuggestion {
            suggestion: Suggestion {
                value: name,
                description,
                span,
                append_whitespace: true,
                ..Suggestion::default()
            },
            kind: Some(SuggestionKind::Value(nu_protocol::Type::String)),
            help,
            ..Default::default()
        });
    }
    candidates
}
