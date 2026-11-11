use std::collections::HashMap;

use crate::completions::{Candidates, Context, SemanticSuggestion, taking, to_reedline_span};
use nu_protocol::{
    ENV_VARIABLE_ID, IN_VARIABLE_ID, LAST_RESULT_VAR_NAME, LAST_VARIABLE_ID, NU_VARIABLE_ID,
    SuggestionKind,
};
use reedline::Suggestion;

/// The variables in scope, and the ones every scope has.
pub(crate) fn variables(ctx: &Context) -> Candidates {
    let working_set = ctx.working_set;
    let stack = ctx.stack;
    let mut candidates = Candidates::default();
    let current_span = to_reedline_span(ctx.span, ctx.offset);

    // Variable completion (e.g: $en<tab> to complete $env)
    let mut variables = HashMap::new();
    variables.insert("$nu".into(), &NU_VARIABLE_ID);
    variables.insert("$in".into(), &IN_VARIABLE_ID);
    variables.insert("$env".into(), &ENV_VARIABLE_ID);
    variables.insert(format!("${LAST_RESULT_VAR_NAME}"), &LAST_VARIABLE_ID);

    let kept =
        |var_id: &_| !stack.parent_deletions.contains(var_id) && !stack.deletions.contains(var_id);
    let mut add = |name: &[u8], var_id| {
        if kept(var_id) {
            variables.insert(String::from_utf8_lossy(name).into_owned(), var_id);
        }
    };
    // The scope the parser saw at the cursor, whose blocks a parse of a whole file has closed.
    if !ctx.scope.is_empty() {
        for (name, var_id) in ctx.scope.iter().rev() {
            add(name, var_id);
        }
    } else {
        let mut removed_overlays = vec![];
        for scope_frame in working_set.delta.scope.iter().rev() {
            for overlay_frame in scope_frame.active_overlays(&mut removed_overlays).rev() {
                for (name, var_id) in &overlay_frame.vars {
                    add(name, var_id);
                }
            }
        }
        let permanent = working_set
            .permanent_state
            .active_overlays(&removed_overlays);
        for overlay_frame in permanent.rev() {
            for (name, var_id) in &overlay_frame.vars {
                add(name, var_id);
            }
        }
    }

    for (name, var_id) in variables {
        let ty = &working_set.get_variable(*var_id).ty;
        candidates.push(SemanticSuggestion {
            suggestion: Suggestion {
                value: name,
                span: current_span,
                description: Some(ty.to_string()),
                append_whitespace: !taking::has_members(ty),
                ..Suggestion::default()
            },
            kind: Some(SuggestionKind::Variable),
            ..Default::default()
        });
    }

    candidates
}
