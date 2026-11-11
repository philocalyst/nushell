//! Completion delegation: completing part of a line as a command line of its own.
//!
//! Two things delegate: the programs of [`WRAPPERS`], which run the command they are given, and
//! a completer that returns `{delegate: {from: <int>}}`. Both end in [`complete`], which
//! completes `buffer[from..]` the way it would be completed if it were typed on a line by itself,
//! and moves the spans of the answer back onto the original line.

use crate::completions::{
    Context, Origin, SemanticSuggestion,
    cache::{Names, SourceKey},
    completer::CompletionEngine,
    completion_common,
    custom_completions::{
        catch_completion_panic, engine_state_for_completion, panic_to_shell_error, report,
    },
    quote::Quote,
};
use nu_engine::eval_block_with_early_return;
use nu_protocol::{
    PipelineData, ShellError, Span, Value,
    debugger::WithoutDebug,
    engine::{Closure, StateWorkingSet},
};
use std::{borrow::Cow, sync::Arc};

/// How many delegations may nest. `ssh host sudo git ch` is two deep; the limit only stops a
/// completer that delegates to the line it was given from running forever.
pub(crate) const MAX_DEPTH: usize = 8;

/// The programs that run the command line they are given (`sudo git ch`), as on `main`: their
/// arguments from the first that is no option complete as a line of their own. Options and their
/// values are read by an `extern` that declares them, which can delegate in turn.
pub(crate) const WRAPPERS: [&str; 2] = ["sudo", "doas"];

/// What stands in for the local machine while a delegated line is completed: the
/// closures a completer handed over for the directories and the commands of another one.
#[derive(Clone, Default)]
pub(crate) struct Sources {
    /// `{|dir| ...}`: the names in a directory, directories ending in `/`.
    pub(crate) paths: Option<Arc<Closure>>,
    /// `{|| ...}`: the names of the commands.
    pub(crate) commands: Option<Arc<Closure>>,
    /// The quote of the string the delegated line is inside of (`ssh host 'cat f`), when it is
    /// inside one: what the line inserts has to be valid in it, as well as for the shell that
    /// reads the line.
    pub(crate) inside: Option<Quote>,
}

impl Sources {
    /// These sources, with whatever they leave unsaid taken from `inherited`.
    fn or(self, inherited: &Sources) -> Self {
        Self {
            paths: self.paths.or_else(|| inherited.paths.clone()),
            commands: self.commands.or_else(|| inherited.commands.clone()),
            inside: self.inside.or(inherited.inside),
        }
    }
}

/// The state a delegated line is completed under, carried by its [`Context`].
#[derive(Clone, Default)]
pub(crate) struct Delegation {
    /// How many delegations deep this is.
    pub(crate) depth: usize,
    pub(crate) sources: Sources,
    /// Whether the path being completed is read as it stands (`scp host:my file`), and so is not
    /// quoted for a shell on the other machine.
    pub(crate) literal: bool,
}

impl Delegation {
    /// Whether the line is for another machine: nothing local, such as the configured external
    /// completer or a declaration's own completers, knows what that machine has.
    pub(crate) fn is_remote(&self) -> bool {
        self.sources.paths.is_some() || self.sources.commands.is_some()
    }

    /// The entries of `dir` on the machine the line is for, or `None` when the local
    /// filesystem stands. A failing source offers nothing, rather than the wrong machine's files.
    pub(crate) fn directory_entries(&self, ctx: &Context, dir: &str) -> Option<Listed> {
        let closure = self.sources.paths.as_deref()?;
        Some(names_from(ctx, closure, &[dir]))
    }

    /// The names of the commands on the machine the line is for, or `None` when `PATH` stands.
    pub(crate) fn command_names(&self, ctx: &Context) -> Option<Listed> {
        let closure = self.sources.commands.as_deref()?;
        Some(names_from(ctx, closure, &[]))
    }
}

/// The names a source closure listed, and whether they are as it just listed them or as the
/// cache remembered them from an earlier request: another machine may have changed since.
#[derive(Default)]
pub(crate) struct Listed {
    pub(crate) names: Arc<[String]>,
    pub(crate) origin: Origin,
}

/// The names a source closure lists, as it was last asked within the cache's lifetime: another
/// machine is slow to ask, and typing a character more asks again. A closure is called with
/// `args` as strings.
///
/// A closure that fails offers nothing, and the failure is told to the user as the reason there
/// is nothing: it is the closure's own wording (`error make {msg: ...}`).
fn names_from(ctx: &Context, closure: &Closure, args: &[&str]) -> Listed {
    let key = SourceKey::new(closure, args);
    let (names, origin) = match ctx.cache.listed(&key, ctx.trace) {
        Some(kept) => (kept, Origin::Cached),
        None => {
            let args = args
                .iter()
                .map(|arg| Value::string(*arg, ctx.span))
                .collect();
            let names = list_names(ctx, closure, args);
            ctx.cache.remember_listed(key, names.clone());
            (names, Origin::Live)
        }
    };
    if let Some(failure) = &names.failure {
        ctx.notice.fail(failure.as_ref());
    }
    Listed {
        names: names.list,
        origin,
    }
}

/// Run a source closure for the names it lists; none, logged, when it fails.
fn list_names(ctx: &Context, closure: &Closure, args: Vec<Value>) -> Names {
    let listed = |list: Vec<String>| Names {
        list: list.into(),
        failure: None,
    };
    match run_closure(ctx, closure, args) {
        Ok(Value::List { vals, .. }) => listed(
            vals.into_iter()
                .filter_map(|name| name.coerce_into_string().ok())
                .filter(|name| !name.is_empty())
                .collect(),
        ),
        Ok(Value::String { val, .. }) => listed(val.lines().map(str::to_owned).collect()),
        Ok(other) => {
            report(format!(
                "a delegated source must return a list of names; got {}",
                other.get_type()
            ));
            Names::default()
        }
        Err(err) => {
            // An `error make` is the closure saying why it has nothing, and it is told to the
            // user as that; any other error is a fault of the closure.
            if !matches!(err, ShellError::Generic(_)) {
                report(format!("a delegated source failed: {err}"));
            }
            Names {
                list: Arc::default(),
                failure: Some(err.to_string().into()),
            }
        }
    }
}

/// Call `closure` with `args`, binding them to its parameters in order.
fn run_closure(ctx: &Context, closure: &Closure, args: Vec<Value>) -> Result<Value, ShellError> {
    let working_set = ctx.working_set;
    let block = working_set.get_block(closure.block_id).clone();
    let mut stack = ctx
        .stack
        .captures_to_stack_preserve_out_dest(closure.captures.clone());

    let parameters = block
        .signature
        .required_positional
        .iter()
        .chain(&block.signature.optional_positional);
    for (parameter, arg) in parameters.zip(args) {
        if let Some(var_id) = parameter.var_id {
            stack.add_var(var_id, arg);
        }
    }

    let engine_state = engine_state_for_completion(
        working_set,
        closure.block_id.get() < working_set.permanent_state.num_blocks(),
    );
    let span = ctx.span;
    match catch_completion_panic(|| {
        eval_block_with_early_return::<WithoutDebug>(
            engine_state.as_ref(),
            &mut stack,
            &block,
            PipelineData::empty(),
        )
        .and_then(|data| data.body.into_value(span))
    }) {
        Ok(result) => result,
        Err(payload) => Err(panic_to_shell_error(payload.as_ref(), span)),
    }
}

/// Complete `ctx.buffer[from..]` as a command line of its own, under `sources` (what the line
/// does not name is inherited from the delegation already in force). The spans of what comes
/// back are on the line `ctx` was given.
///
/// Each suggestion is marked as answered for a line delegated from another, keeping the
/// source that found it: `delegated: file`.
///
/// `None` declines: `from` is not a char boundary of the line, or the delegations nest too
/// deep.
pub(crate) fn complete(
    ctx: &Context,
    from: usize,
    sources: Sources,
) -> Option<Vec<SemanticSuggestion>> {
    let line = ctx.buffer.get(from..)?;
    let enclosing = ctx.delegation.cloned().unwrap_or_default();
    if enclosing.depth >= MAX_DEPTH {
        report(format!(
            "completion delegates more than {MAX_DEPTH} levels deep; giving up"
        ));
        return None;
    }

    let delegation = Delegation {
        depth: enclosing.depth + 1,
        sources: sources.or(&enclosing.sources),
        literal: false,
    };
    // A parsed file has the rest of the word under the cursor past it; the span of whatever
    // replaces the word has to cover it.
    let typed_end = ctx.offset + ctx.buffer.len();
    let untyped = (ctx.span.end > typed_end)
        .then(|| {
            ctx.working_set
                .get_span_contents(Span::new(typed_end, ctx.span.end))
        })
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or_default();

    let mut found =
        CompletionEngine::delegated(ctx, delegation).complete_program_line(line, untyped, from);
    mark_delegated(&mut found);
    Some(found)
}

/// Mark each suggestion as answered for a line delegated from another, keeping the source that
/// found it.
fn mark_delegated(found: &mut [SemanticSuggestion]) {
    for item in found {
        item.source = Some(Cow::Owned(match item.source.take() {
            Some(inner) => format!("delegated: {inner}"),
            None => "delegated".to_owned(),
        }));
    }
}

/// Complete `ctx.buffer[from..]` as a path on another machine, which `sources.paths` lists:
/// `scp host:dir/f` has `from` at the `d`. The spans of what comes back are on the line `ctx` was
/// given.
///
/// `None` declines: `from` is not a char boundary of the line, or the delegations nest too deep.
pub(crate) fn complete_path(
    ctx: &Context,
    from: usize,
    sources: Sources,
) -> Option<Vec<SemanticSuggestion>> {
    ctx.buffer.get(from..)?;
    let enclosing = ctx.delegation.cloned().unwrap_or_default();
    if enclosing.depth >= MAX_DEPTH {
        return None;
    }
    let delegation = Delegation {
        depth: enclosing.depth + 1,
        sources: sources.or(&enclosing.sources),
        literal: true,
    };

    let word = Span::new(ctx.offset + from, ctx.span.end.max(ctx.offset + from));
    let word_ctx = Context {
        delegation: Some(&delegation),
        ..ctx.token(word)
    };
    let mut found = completion_common::paths(&word_ctx, false).suggestions(&word_ctx);
    mark_delegated(&mut found);
    Some(found)
}

/// Whether `word` names a command Nushell defines itself rather than a program, so a wrapped
/// line starting with it must be read as a call to the program: `sudo ls` runs `/bin/ls`.
/// Custom commands and `extern` declarations keep their signatures.
pub(crate) fn names_builtin(working_set: &StateWorkingSet, word: &str) -> bool {
    use nu_protocol::engine::CommandType;

    working_set
        .find_decl(word.as_bytes())
        .is_some_and(|decl_id| {
            !matches!(
                working_set.get_decl(decl_id).command_type(),
                CommandType::Custom | CommandType::External
            )
        })
}
