use crate::completions::{Candidates, Context, SemanticSuggestion, to_reedline_span};
use nu_protocol::{DeclId, Record, Span, SuggestionKind, Value};
use reedline::Suggestion;

/// The field of a flag's suggestion that says what it takes, which the panel draws as a type.
pub(crate) const TAKES: &str = "takes";

/// The flags of the command `decl_id`, one suggestion for each: its long form with its short one
/// beside it, or the short form while a short one is being typed (`-h`), and what a flag that
/// takes a value takes.
pub(crate) fn flags(decl_id: DeclId, ctx: &Context) -> Candidates {
    let mut candidates = Candidates::default();
    let span = to_reedline_span(ctx.span, ctx.offset);
    let typed = ctx.prefix_str();
    let short_typed = typed.len() > 1 && typed.starts_with('-') && !typed.starts_with("--");
    let mut push =
        |value: String, other: Option<String>, takes: Option<String>, description: &str| {
            // The other form and the type are drawn beside the value, as a completer's extra fields
            // are.
            let fields = [("form", other), (TAKES, takes)]
                .into_iter()
                .filter_map(|(name, text)| {
                    Some((name.to_owned(), Value::string(text?, Span::unknown())))
                })
                .collect::<Record>();
            candidates.push(SemanticSuggestion {
                suggestion: Suggestion {
                    value,
                    description: Some(description.to_owned()),
                    span,
                    append_whitespace: true,
                    ..Suggestion::default()
                },
                kind: Some(SuggestionKind::Flag),
                fields: (!fields.is_empty()).then_some(fields),
                ..Default::default()
            });
        };

    for named in &ctx.working_set.get_decl(decl_id).signature().named {
        let long = named.long_name().map(|long| format!("--{long}"));
        let short = named.short.map(|short| format!("-{short}"));
        let (value, other) = match (long, short) {
            (Some(long), Some(short)) if short_typed && short.starts_with(typed.as_ref()) => {
                (short, Some(long))
            }
            (Some(long), short) => (long, short),
            (None, Some(short)) => (short, None),
            (None, None) => continue,
        };
        let takes = named.arg.as_ref().and_then(crate::status_line::shape_label);
        push(value, other, takes, &named.desc);
    }
    candidates
}
