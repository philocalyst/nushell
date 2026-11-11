use crate::completions::{
    Candidates, Context, SemanticSuggestion, completion_common::complete_item,
    completion_common::surround_remove, to_reedline_span,
};
use nu_path::expand_tilde;
use nu_protocol::{
    SuggestionKind, VirtualPathId,
    engine::{StateWorkingSet, VirtualPath},
};
use reedline::Suggestion;
use std::collections::{HashMap, HashSet};

/// The children of the virtual directory `prefix` names up to its last `/`, such as `std` for
/// `use std/l`.
fn virtual_dir<'a>(working_set: &'a StateWorkingSet, prefix: &str) -> Option<&'a [VirtualPathId]> {
    let (base_dir, _) = prefix.rsplit_once('/')?;
    match working_set.find_virtual_path(&surround_remove(base_dir)) {
        Some(VirtualPath::Dir(sub_paths)) => Some(sub_paths),
        _ => None,
    }
}

/// The modules in scope and, with `std_virtual_path`, the virtual paths of the standard
/// library. The files come next: see [`module_files`].
pub(crate) fn modules(std_virtual_path: bool, ctx: &Context) -> Candidates {
    let working_set = ctx.working_set;
    let prefix = ctx.prefix_str();
    let span = to_reedline_span(ctx.span, ctx.offset);
    let mut candidates = Candidates {
        partial: true,
        ..Candidates::default()
    };

    // Modules that are already loaded go first
    let mut modules_map = HashMap::new();
    // TODO: inline-defined modules, e.g. `module foo {}; use foo<tab>` ?
    for overlay_frame in working_set.permanent_state.active_overlays(&[]) {
        modules_map.extend(&overlay_frame.modules);
    }

    for (module_name_bytes, module_id) in modules_map.into_iter() {
        // TODO: this is a quickfix. just like `help modules`, this constructs the module
        // description from scratch each time. we should construct module descriptions when the
        // module is first parsed and store it for later use (like we do with custom commands)
        // TODO: we need to provide descriptions not just for modules that are already in scope,
        // but for modules that haven't been imported yet.
        // (importing a new file like `use foo.nu`)
        let description = working_set
            .get_module_comments(*module_id)
            .map(|spans| working_set.build_desc(spans).0);

        candidates.push(SemanticSuggestion {
            suggestion: Suggestion {
                value: String::from_utf8_lossy(module_name_bytes).into_owned(),
                description,
                span,
                append_whitespace: true,
                ..Suggestion::default()
            },
            kind: Some(SuggestionKind::Module),
            ..Default::default()
        });
    }

    // Add std virtual paths first
    if std_virtual_path {
        let mut push = |value: String, append_whitespace: bool| {
            candidates.push(SemanticSuggestion {
                suggestion: Suggestion {
                    value,
                    span,
                    append_whitespace,
                    ..Suggestion::default()
                },
                kind: Some(SuggestionKind::Module),
                ..Default::default()
            });
        };
        // Where we have '/' in the prefix, e.g. use std/l
        if prefix.contains('/') {
            for sub_vp_id in virtual_dir(working_set, &prefix).unwrap_or_default() {
                let (path, sub_vp) = working_set.get_virtual_path(*sub_vp_id);
                // A module directory goes on with its submodules, if it has any besides its own
                // `mod.nu`.
                let goes_on = match sub_vp {
                    VirtualPath::Dir(entries) => entries
                        .iter()
                        .any(|entry| !working_set.get_virtual_path(*entry).0.ends_with("mod.nu")),
                    _ => false,
                };
                push(path.into(), !goes_on);
            }
        } else {
            for path in ["std", "std-rfc"] {
                push(path.into(), false);
            }
        }
    }
    candidates
}

/// The `.nu` files and the directories under the current directory and `NU_LIB_DIRS`, ranked
/// by their last component.
pub(crate) fn module_files(std_virtual_path: bool, ctx: &Context) -> Candidates {
    let working_set = ctx.working_set;
    let prefix = ctx.prefix_str();

    // NOTE: 2 ways to setup `NU_LIB_DIRS`
    // 1. `const NU_LIB_DIRS = [paths]`, equal to `nu -I paths`
    // 2. `$env.NU_LIB_DIRS = [paths]`
    let const_lib_dirs = working_set
        .find_variable(b"$NU_LIB_DIRS")
        .and_then(|vid| working_set.get_variable(vid).const_val.as_ref());
    let env_lib_dirs = working_set.get_env_var("NU_LIB_DIRS");
    let mut search_dirs = [const_lib_dirs, env_lib_dirs]
        .into_iter()
        .flatten()
        .flat_map(|lib_dirs| {
            lib_dirs
                .as_list()
                .into_iter()
                .flat_map(|it| it.iter().filter_map(|x| x.to_path().ok()))
                .map(expand_tilde)
        })
        .collect::<HashSet<_>>();

    // A virtual dir (`use std/`) already supplied its children. Walking cwd
    // is the `/` hitch in large trees and cannot complete extra files under
    // virtual `std` (find_in_dirs prefers the VFS). Still search NU_LIB_DIRS
    // so a real `std/` overlay in lib dirs can appear.
    let resolved_virtual_dir = std_virtual_path && virtual_dir(working_set, &prefix).is_some();
    if !resolved_virtual_dir && let Ok(cwd) = working_set.permanent_state.cwd(Some(ctx.stack)) {
        search_dirs.insert(cwd.into_std_path_buf());
    }

    let (directories, files): (Vec<_>, Vec<_>) = complete_item(
        false,
        ctx.span,
        prefix.as_ref(),
        &search_dirs
            .iter()
            .filter_map(|d| d.to_str())
            .collect::<Vec<_>>(),
        ctx,
    )
    .into_iter()
    .partition(|found| found.kind == Some(SuggestionKind::Directory));

    let files = files.into_iter().filter(|found| {
        // The path as it is, not as it is spelled (spaces, quotes).
        found
            .suggestion
            .display_override
            .as_deref()
            .unwrap_or(&found.suggestion.value)
            .ends_with(".nu")
    });
    let mut found = Vec::new();
    for mut module in files.chain(directories) {
        module.suggestion.append_whitespace = module.kind != Some(SuggestionKind::Directory);
        module.kind = Some(SuggestionKind::Module);
        found.push(module);
    }
    Candidates::kept(found)
}
