use crate::completions::{
    Candidates, Context, SemanticSuggestion, taking, to_reedline_span, usage::Used,
};
use nu_protocol::SuggestionKind;
use nu_utils::NuCow;
use reedline::Suggestion;

/// The values a signature lists for an argument.
pub(crate) fn list(
    values: &NuCow<&'static [&'static str], Vec<String>>,
    ctx: &Context,
) -> Candidates {
    let mut candidates = Candidates {
        used: Some(Used::Argument),
        ..Candidates::default()
    };
    let span = to_reedline_span(ctx.span, ctx.offset);
    let mut push = |value: &str| {
        candidates.push(SemanticSuggestion {
            suggestion: Suggestion {
                value: value.to_owned(),
                span,
                append_whitespace: taking::ends_an_argument(value, None),
                ..Suggestion::default()
            },
            kind: Some(SuggestionKind::Value(nu_protocol::Type::String)),
            ..Default::default()
        });
    };

    match values {
        NuCow::Borrowed(values) => values.iter().for_each(|value| push(value)),
        NuCow::Owned(values) => values.iter().for_each(|value| push(value)),
    }
    candidates
}
