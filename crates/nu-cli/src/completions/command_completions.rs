use std::{collections::HashSet, path::PathBuf, sync::Arc};

use crate::completions::{Candidates, Context, delegation::Listed, to_reedline_span};
use nu_protocol::{
    Category, CompareTypes, DeclId, Record, Span, SuggestionKind, Type, Value,
    ast::Expr,
    engine::{Command, CommandType, StateWorkingSet},
};
use reedline::Suggestion;

use super::{
    SemanticSuggestion,
    cache::PathEntries,
    completion_options::NuMatcher,
    quote::{Quote, Spelling, quote},
    usage::Used,
};

/// Which command declarations are offered.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    /// Internal commands (all visible) plus external `PATH` commands — the default head scope.
    All,
    /// External `PATH` commands only (the `^` sigil).
    ExternalsOnly,
    /// Built-in commands only, scanning even shadowed declarations (the `%` sigil).
    BuiltinsOnly,
    /// Internal commands only — never scans `PATH` (subcommands, `attr complete`).
    InternalsOnly,
}

impl CommandScope {
    fn externals(self) -> bool {
        matches!(self, CommandScope::All | CommandScope::ExternalsOnly)
    }

    /// Narrowed by `$env.config.completions.external.enable`: only [`All`](Self::All)
    /// collapses to [`InternalsOnly`](Self::InternalsOnly) when externals are disabled.
    fn enabled_in(self, context: &Context) -> Self {
        let enabled = context
            .working_set
            .permanent_state
            .config
            .completions
            .external
            .enable;
        match (self, enabled) {
            (CommandScope::All, false) => CommandScope::InternalsOnly,
            (scope, _) => scope,
        }
    }
}

/// A command as a suggestion.
fn command_suggestion(
    value: String,
    description: Option<String>,
    kind: SuggestionKind,
    span: reedline::Span,
) -> SemanticSuggestion {
    SemanticSuggestion {
        suggestion: Suggestion {
            value,
            description,
            span,
            append_whitespace: true,
            ..Suggestion::default()
        },
        kind: Some(kind),
        ..Default::default()
    }
}

/// The entries of the directories of `PATH`, as the cache has them: read again only when a
/// directory changed, not for each token typed.
pub(crate) fn executable_files(context: &Context) -> Arc<PathEntries> {
    let directories: Vec<PathBuf> = context
        .working_set
        .permanent_state
        .get_env_var("path")
        .and_then(|path_value| path_value.as_list().ok())
        .into_iter()
        .flatten()
        .map(|path_value| PathBuf::from(path_value.coerce_str().unwrap_or_default().as_ref()))
        .collect();
    context.cache.path_entries(&directories, context.trace)
}

fn is_executable_command(path: impl AsRef<std::path::Path>) -> bool {
    let path = path.as_ref();

    is_executable::is_executable(path)
        || (cfg!(windows)
            && path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ps1")))
}

/// The built-in commands that match, including shadowed ones (reverse traversal).
fn builtins(probe: &mut NuMatcher<()>, context: &Context) -> Vec<SemanticSuggestion> {
    let working_set = context.working_set;
    let span = to_reedline_span(context.span, context.offset);
    let mut seen_names: HashSet<&str> = HashSet::new();

    (0..working_set.num_decls())
        .rev()
        .map(DeclId::new)
        .map(|declaration_id| (declaration_id, working_set.get_decl(declaration_id)))
        .filter(|(_, command)| {
            command.signature().category != Category::Removed
                && command.command_type() == CommandType::Builtin
                && seen_names.insert(command.name())
        })
        // Match before allocating a description: this runs on every keystroke.
        .filter(|(_, command)| probe.check_match(command.name()).is_some())
        .map(|(declaration_id, command)| {
            command_suggestion(
                command.name().to_string(),
                Some(command.description().to_string()),
                SuggestionKind::Command(CommandType::Builtin, Some(declaration_id)),
                span,
            )
        })
        .collect()
}

/// The visible internal commands that match, using the engine's traversal, each with the text it
/// is matched by. With a `parent`, only its subcommands.
///
/// An alias that only renames another command (`cargo b` for `cargo build`) is no row of its own:
/// the command it names is offered once, with the alias in its `alias` field, and matched as the
/// alias where the alias matches.
fn visible_internals(
    probe: &mut NuMatcher<()>,
    context: &Context,
    quoted: bool,
    parent: Option<&str>,
) -> Vec<(String, SemanticSuggestion)> {
    let working_set = context.working_set;
    let span = to_reedline_span(context.span, context.offset);
    let mut found = Vec::new();
    let mut renames = Vec::new();
    // A command name spelled as a string stays in a quote the user opened.
    let spelling = Spelling {
        inside: Quote::opened_by(context.prefix_str().as_ref()),
        ..Spelling::default()
    };

    // A name declared again, as a file being edited is, is the newest declaration's alone.
    let mut seen = std::collections::HashSet::new();
    working_set.traverse_commands(|name_bytes, declaration_id| {
        let command = working_set.get_decl(declaration_id);
        if command.signature().category == Category::Removed || !seen.insert(name_bytes.to_vec()) {
            return;
        }

        let raw_name = String::from_utf8_lossy(name_bytes);
        if let Some(parent) = parent
            && !raw_name
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with(' '))
        {
            return;
        }
        let name = match quoted {
            true => quote(&raw_name, spelling).into_owned(),
            false => raw_name.into_owned(),
        };

        // Match the prefix before `description()` allocates (runs every keystroke).
        let matched = probe.check_match(&name).is_some();
        if let Some(target) = renamed(command) {
            renames.push((name, target, matched));
        } else if matched {
            found.push((
                name.clone(),
                command_suggestion(
                    name,
                    Some(command.description().to_string()),
                    SuggestionKind::Command(command.command_type(), Some(declaration_id)),
                    span,
                ),
            ));
        }
    });

    for (alias, target, matched) in renames {
        let named = |(_, item): &(String, SemanticSuggestion)| matches!(item.kind, Some(SuggestionKind::Command(_, Some(id))) if id == target);
        let at = match found.iter().position(named) {
            Some(at) => at,
            None if matched => {
                let command = working_set.get_decl(target);
                let name = match quoted {
                    true => quote(command.name(), spelling).into_owned(),
                    false => command.name().to_owned(),
                };
                found.push((
                    name.clone(),
                    command_suggestion(
                        name,
                        Some(command.description().to_string()),
                        SuggestionKind::Command(command.command_type(), Some(target)),
                        span,
                    ),
                ));
                found.len() - 1
            }
            None => continue,
        };
        let (haystack, item) = &mut found[at];
        add_alias(item, &alias);
        if matched {
            *haystack = alias;
        }
    }
    found
}

/// Names `alias` in the `alias` field of the command `item` it renames, by its own words, without
/// those it shares with the command: `b`, not `cargo b`.
fn add_alias(item: &mut SemanticSuggestion, alias: &str) {
    let shared = (alias.split(' ').zip(item.suggestion.value.split(' ')))
        .take_while(|(word, of_command)| word == of_command)
        .count();
    let short = alias.split(' ').skip(shared).collect::<Vec<_>>().join(" ");
    let fields = item.fields.get_or_insert_with(Record::new);
    let aliases = match fields.get("alias").and_then(|known| known.as_str().ok()) {
        Some(known) => format!("{known}, {short}"),
        None => short,
    };
    fields.insert("alias", Value::string(aliases, Span::unknown()));
}

/// Folds each suggestion of `found` that only renames another of them into it: an alias of a
/// command offered too (`b` for `build` after `cargo`), or what a completer describes as one
/// (`alias: build`). The command is offered once, with the alias in its `alias` field, where the
/// first of the two was.
pub(crate) fn fold_renames(found: &mut Vec<SemanticSuggestion>, working_set: &StateWorkingSet) {
    let target_of = |item: &SemanticSuggestion| -> Option<usize> {
        // A command the line itself declares is not in `working_set`.
        let by_decl = match item.kind {
            Some(SuggestionKind::Command(_, Some(id))) if id.get() < working_set.num_decls() => {
                renamed(working_set.get_decl(id))
            }
            _ => None,
        };
        let by_description = (item.suggestion.description.as_deref())
            .and_then(|said| {
                said.strip_prefix("alias: ")
                    .or(said.strip_prefix("alias for "))
            })
            .map(str::trim);
        found.iter().position(|other| {
            let decl = match other.kind {
                Some(SuggestionKind::Command(_, Some(id))) => Some(id),
                _ => None,
            };
            (by_decl.is_some() && decl == by_decl)
                || by_description.is_some_and(|name| other.suggestion.value == name)
        })
    };
    let targets: Vec<Option<usize>> = found.iter().map(target_of).collect();
    if targets.iter().all(Option::is_none) {
        return;
    }
    let mut taken: Vec<Option<SemanticSuggestion>> =
        std::mem::take(found).into_iter().map(Some).collect();
    // The aliases of each command, by the command's place.
    let mut aliases: Vec<Vec<String>> = vec![Vec::new(); taken.len()];
    for (at, target) in targets.iter().enumerate() {
        if let (Some(target), Some(item)) = (target, &taken[at])
            && *target != at
        {
            aliases[*target].push(item.suggestion.value.clone());
        }
    }
    for at in 0..taken.len() {
        let place = match targets[at] {
            Some(target) if target != at => target,
            _ => at,
        };
        if place != at {
            taken[at] = None;
        }
        if let Some(mut command) = taken[place].take() {
            for alias in &aliases[place] {
                add_alias(&mut command, alias);
            }
            found.push(command);
        }
    }
}

/// The command `command` stands for when it is an alias that only renames it, with no
/// arguments of its own.
fn renamed(command: &dyn Command) -> Option<DeclId> {
    let alias = command.as_alias()?;
    match &alias.wrapped_call.expr {
        Expr::Call(call) if call.arguments.is_empty() => Some(call.decl_id),
        _ => None,
    }
}

/// The commands another machine has, as its `commands` closure listed them, in place of the
/// ones on `PATH`.
fn remote_commands(
    probe: &mut NuMatcher<()>,
    context: &Context,
    listed: &Listed,
) -> Vec<SemanticSuggestion> {
    let names: &[String] = &listed.names;
    let maximum_results = context
        .working_set
        .permanent_state
        .config
        .completions
        .external
        .max_results as usize;
    let span = to_reedline_span(context.span, context.offset);
    let mut seen = HashSet::new();
    let mut found = Vec::new();

    for name in names {
        if seen.len() >= maximum_results {
            break;
        }
        if probe.check_match(name).is_some() && seen.insert(name.as_str()) {
            let mut suggestion = command_suggestion(
                name.clone(),
                None,
                SuggestionKind::Command(CommandType::External, None),
                span,
            );
            suggestion.origin = listed.origin;
            found.push(suggestion);
        }
    }
    found
}

/// Walks `PATH` once, offering the commands that match. A program that an internal command of
/// the same name hides is left out: typing `^` asks for it, and `%` for a builtin a definition
/// hides, so neither needs a second copy of the name in the list.
fn path_commands(
    probe: &mut NuMatcher<()>,
    context: &Context,
    internals: &[(String, SemanticSuggestion)],
) -> Vec<SemanticSuggestion> {
    let maximum_results = context
        .working_set
        .permanent_state
        .config
        .completions
        .external
        .max_results as usize;
    let span = to_reedline_span(context.span, context.offset);
    let internal_names: HashSet<&str> = internals
        .iter()
        .map(|(_, suggestion)| suggestion.suggestion.value.as_str())
        .collect();

    let mut externals: Vec<SemanticSuggestion> = Vec::new();
    let mut external_values: HashSet<&str> = HashSet::new();

    let files = executable_files(context);
    for (file_name, file_path) in files.iter() {
        if external_values.len() >= maximum_results
            || internal_names.contains(file_name)
            || external_values.contains(file_name)
            || probe.check_match(file_name).is_none()
        {
            continue;
        }
        // `is_executable_command` stats the file, which dominates the scan on slow
        // filesystems (e.g. WSL's 9P mounts to Windows `PATH` directories), so only
        // pay for entries that can still contribute a suggestion.
        if !is_executable_command(&file_path) {
            continue;
        }
        external_values.insert(file_name);
        externals.push(command_suggestion(
            file_name.to_owned(),
            None,
            SuggestionKind::Command(CommandType::External, None),
            span,
        ));
    }
    externals
}

/// The texts `internals` are matched by of the commands that take `piped`: one of their input
/// types accepts it. A command that takes anything does, so this only tells commands that take
/// something else apart, and only when `piped` says something.
fn taking(
    piped: &Type,
    internals: &[(String, SemanticSuggestion)],
    context: &Context,
) -> HashSet<String> {
    if matches!(piped, Type::Any | Type::Nothing) {
        return HashSet::new();
    }
    internals
        .iter()
        .filter_map(|(haystack, suggestion)| match suggestion.kind {
            Some(SuggestionKind::Command(_, Some(decl_id))) => {
                let signature = context.working_set.get_signature_shared(decl_id);
                // Taking `any` says nothing of what is piped: a `def` with no input type does.
                (signature.input_output_types.iter())
                    .any(|(input, _)| *input != Type::Any && piped.is_subtype_of(input))
                    .then(|| haystack.clone())
            }
            _ => None,
        })
        .collect()
}

/// The commands of `scope` that match the token: internal ones, then the ones on `PATH`. Where
/// a command is begun (not `quoted`, as an argument is), those that take what is piped into it
/// rank ahead of the rest.
/// `quoted` quotes the names of internal commands that hold a space; `parent`, the command the
/// line already names, limits them to its subcommands, so `co f` after a defined `co` never
/// reaches `config flatten`.
pub(crate) fn commands(
    scope: CommandScope,
    quoted: bool,
    parent: Option<&str>,
    context: &Context,
) -> Candidates {
    let scope = scope.enabled_in(context);
    // Only to keep what cannot match from being allocated or, for `PATH`, stat-ed: the
    // candidates are ranked together once they are all in.
    let mut probe = NuMatcher::<()>::new(context.prefix_str(), context.options, false);

    let internals = match scope {
        CommandScope::ExternalsOnly => Vec::new(),
        CommandScope::BuiltinsOnly => (builtins(&mut probe, context).into_iter())
            .map(|builtin| (builtin.suggestion.value.clone(), builtin))
            .collect(),
        CommandScope::All | CommandScope::InternalsOnly => {
            visible_internals(&mut probe, context, quoted, parent)
        }
    };

    // Externals are the `PATH` scan, which the cache holds between requests.
    let externals = if scope.externals() {
        match context
            .delegation
            .and_then(|delegation| delegation.command_names(context))
        {
            Some(listed) => remote_commands(&mut probe, context, &listed),
            None => path_commands(&mut probe, context, &internals),
        }
    } else {
        Vec::new()
    };

    // A binary on `PATH` is not listed after every custom command that matches.
    let mut candidates = Candidates {
        used: Some(Used::Command),
        fits: context
            .piped
            .filter(|_| !quoted)
            .map_or_else(HashSet::new, |piped| taking(piped, &internals, context)),
        ..Candidates::default()
    };
    for (haystack, suggestion) in internals {
        candidates.push_as(haystack, suggestion);
    }
    for suggestion in externals {
        candidates.push(suggestion);
    }
    if candidates.items.is_empty()
        && !quoted
        && let Some(split) = run_on(context, scope.externals())
    {
        // Matched as the word it reads the typed one as, so that it is kept.
        candidates.push_as(context.prefix_str().into_owned(), split);
    }
    candidates
}

/// The word typed read as a command and its first argument run together, when nothing else
/// matches it: `cdcrates/` for `cd crates/`. The longest command the word starts with, an
/// internal one or, if `externals`, one on `PATH`, is the one meant.
fn run_on(context: &Context, externals: bool) -> Option<SemanticSuggestion> {
    let typed = context.prefix_str();
    let files = externals.then(|| executable_files(context));
    let on_path = |name: &str| {
        files
            .as_ref()
            .is_some_and(|files| files.iter().any(|(file, _)| file == name))
    };
    let ends: Vec<usize> = typed.char_indices().skip(2).map(|(at, _)| at).collect();
    let (at, decl_id) = (ends.into_iter().rev()).find_map(|at| {
        let name = &typed[..at];
        match context.working_set.find_decl(name.as_bytes()) {
            Some(decl_id) => Some((at, Some(decl_id))),
            None => on_path(name).then_some((at, None)),
        }
    })?;
    let (name, rest) = typed.split_at(at);
    if !rest.starts_with(|c: char| c.is_alphanumeric() || matches!(c, '.' | '/' | '~' | '-')) {
        return None;
    }
    let kind = match decl_id {
        Some(decl_id) => SuggestionKind::Command(
            context.working_set.get_decl(decl_id).command_type(),
            Some(decl_id),
        ),
        None => SuggestionKind::Command(CommandType::External, None),
    };
    let both = format!("{name} {rest}");
    Some(SemanticSuggestion {
        suggestion: Suggestion {
            value: both.clone(),
            display_override: Some(both),
            description: Some(format!("`{name}`, then `{rest}`: a space between")),
            span: to_reedline_span(context.span, context.offset),
            append_whitespace: !rest.ends_with('/'),
            ..Suggestion::default()
        },
        kind: Some(kind),
        ..SemanticSuggestion::default()
    })
}
