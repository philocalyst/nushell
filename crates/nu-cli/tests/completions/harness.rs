//! The adapter the completion harnesses reach the engine through: [`complete`] (one stateless
//! request) and [`Session`] (a long-lived completer, which exercises the narrowing cache) map the
//! engine's types onto the plain [`Candidate`], so when the engine is swapped only they and
//! [`Candidate::from_suggestion`] change.
//!
//! * `cursor` is a byte offset into `line`, clamped to the line and floored to a char boundary
//!   before the engine sees it, so callers may pass any `usize`.
//! * `span` is a byte range of `line`; accepting a candidate replaces that range with `value`.
//! * Order is the engine's ranking order, best first.

use super::support::completions_helpers::new_engine_helper;
use nu_cli::{CompletionEngine, NuCompleter, NuHighlighter};
use nu_engine::{eval_block, eval_expression};
use nu_parser::parse;
use nu_protocol::{
    PipelineData, ShellError, Span, Value,
    ast::{Block, Expr},
    debugger::WithoutDebug,
    engine::{EngineState, Stack, StateWorkingSet},
};
use reedline::{Highlighter, History, Suggestion};
use std::{
    fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::TempDir;

/// One completion candidate, in terms that do not depend on the engine's own types.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// The text that replaces `span` when the candidate is accepted.
    pub value: String,
    /// What the user is shown (`value` unless the engine overrides it).
    pub display: String,
    /// The candidate's description, if any.
    pub description: Option<String>,
    /// The byte range of the line that `value` replaces.
    pub span: Range<usize>,
    /// Whether accepting the candidate appends a space.
    pub append_whitespace: bool,
    /// Where the cursor goes in the accepted line, when it is not just after `value`.
    pub cursor: Option<usize>,
    /// The graphemes of the displayed text that matched what was typed, if the engine says. Empty
    /// for a candidate offered as a replacement of what was typed (a typo correction).
    pub match_indices: Option<Vec<usize>>,
}

impl Candidate {
    /// The adapter's mapping from the current engine's suggestion type.
    pub fn from_suggestion(suggestion: &Suggestion) -> Self {
        Self {
            display: suggestion.display_value().to_owned(),
            value: suggestion.value.clone(),
            description: suggestion.description.clone(),
            span: suggestion.span.start..suggestion.span.end,
            append_whitespace: suggestion.append_whitespace,
            cursor: suggestion.cursor,
            match_indices: suggestion.match_indices.clone(),
        }
    }
}

/// The values of `candidates`, in order.
pub fn values(candidates: &[Candidate]) -> Vec<&str> {
    candidates.iter().map(|c| c.value.as_str()).collect()
}

/// `cursor` as the adapter hands it to the engine: clamped to the line, then floored to a char
/// boundary.
pub fn floor_cursor(line: &str, cursor: usize) -> usize {
    line.floor_char_boundary(cursor.min(line.len()))
}

/// THE adapter: complete `line` at byte offset `cursor`, statelessly. The engine runs inline, so
/// a panic inside it is a panic of the caller.
pub fn complete(
    engine: &mut EngineState,
    stack: &mut Stack,
    line: &str,
    cursor: usize,
) -> Vec<Candidate> {
    let cursor = floor_cursor(line, cursor);
    CompletionEngine::new(engine, stack)
        .fetch_completions_at(line, cursor)
        .into_iter()
        .map(|semantic| Candidate::from_suggestion(&semantic.suggestion))
        .collect()
}

/// A long-lived completer over a snapshot of an engine and stack: the adapter for tests about
/// state that survives between requests, such as the narrowing cache.
pub struct Session {
    completer: NuCompleter,
    /// The highlighter whose parse of the line the completer shares, if it does.
    highlighter: Option<NuHighlighter>,
}

impl Session {
    /// A completer over a snapshot of `engine` and `stack`.
    pub fn new(engine: &EngineState, stack: &Stack) -> Self {
        Self {
            completer: NuCompleter::new(Arc::new(engine.clone()), Arc::new(stack.clone())),
            highlighter: None,
        }
    }

    /// Like [`Session::new`], with a completer that completes from the parse a highlighter makes
    /// of the line, as the editor's does.
    pub fn sharing_the_highlighters_parse(engine: &EngineState, stack: &Stack) -> Self {
        let (engine, stack) = (Arc::new(engine.clone()), Arc::new(stack.clone()));
        let highlighter = NuHighlighter::new(Arc::clone(&engine), Arc::clone(&stack));
        Self {
            completer: NuCompleter::new(engine, stack).share_parse_of(&highlighter),
            highlighter: Some(highlighter),
        }
    }

    /// Paint `line` as the editor does after an edit, which parses it for the highlighter. A
    /// session that does not share has no highlighter to paint with.
    pub fn paint(&self, line: &str, cursor: usize) {
        if let Some(highlighter) = &self.highlighter {
            highlighter.highlight(line, floor_cursor(line, cursor));
        }
    }

    /// Rank by what `history` says the user runs, as the editor does on its first request.
    pub fn learn_usage(&mut self, history: &dyn History) {
        self.completer.learn_usage(history);
    }

    /// Complete `line` at `cursor`, waiting for the answer.
    pub fn complete(&mut self, line: &str, cursor: usize) -> Vec<Candidate> {
        let cursor = floor_cursor(line, cursor);
        let answer = self.completer.complete_blocking(line, cursor);
        answer.iter().map(Candidate::from_suggestion).collect()
    }
}

/// `line` after accepting `candidate`: `value` spliced over `span`, then a space when the
/// candidate asks for one.
pub fn accept(line: &str, candidate: &Candidate) -> String {
    let space = candidate.append_whitespace && candidate.cursor.is_none();
    let (before, after) = (&line[..candidate.span.start], &line[candidate.span.end..]);
    format!(
        "{before}{}{}{after}",
        candidate.value,
        if space { " " } else { "" }
    )
}

/// Why `candidates` are not valid answers for `line` completed at `cursor` (the raw cursor, which
/// is floored here as the adapter does): every span must lie inside the line, on char boundaries,
/// and contain the cursor.
pub fn check_spans(line: &str, cursor: usize, candidates: &[Candidate]) -> Result<(), String> {
    let cursor = floor_cursor(line, cursor);
    for candidate in candidates {
        let Range { start, end } = candidate.span;
        let problem = if start > end {
            "start is after end"
        } else if end > line.len() {
            "end is past the end of the line"
        } else if !line.is_char_boundary(start) || !line.is_char_boundary(end) {
            "span is not on char boundaries"
        } else if !(start <= cursor && cursor <= end) && candidate.cursor.is_none() {
            // One that says where the cursor goes may put right a word behind it.
            "span does not contain the cursor"
        } else {
            continue;
        };
        return Err(format!(
            "{problem}: {candidate:?} for {line:?} at {cursor} (raw cursor in the property)"
        ));
    }
    Ok(())
}

/// Parse `source` and merge what it defines into `engine`. Parse errors are errors.
fn parse_into(engine: &mut EngineState, source: &str) -> Result<Arc<Block>, String> {
    let mut working_set = StateWorkingSet::new(engine);
    let block = parse(&mut working_set, None, source.as_bytes(), false);
    if let Some(error) = working_set.parse_errors.first() {
        return Err(format!("parse error: {error:?}"));
    }
    let delta = working_set.render();
    engine
        .merge_delta(delta)
        .map_err(|error| format!("merge error: {error:?}"))?;
    Ok(block)
}

/// Evaluate nushell `source` in `engine` and `stack`, merging what it defines and sets, and
/// return the value of its last expression. Parse errors are errors.
pub fn eval_source(
    engine: &mut EngineState,
    stack: &mut Stack,
    source: &str,
) -> Result<Value, String> {
    let block = parse_into(engine, source)?;
    let value = eval_block::<WithoutDebug>(engine, stack, &block, PipelineData::empty())
        .and_then(|data| data.body.into_value(Span::unknown()))
        .map_err(|error: ShellError| format!("{error:?}"))?;
    engine.merge_deletions(stack);
    engine
        .merge_env(stack)
        .map_err(|error| format!("env merge error: {error:?}"))?;
    Ok(value)
}

/// Parse `line` (which must be a single call) and evaluate its first positional argument as a
/// nushell expression, exactly as the command would receive it. A string or glob yields its text.
/// A parse error, a line that is not a call, a missing argument or a non-string value is an error.
pub fn first_argument(engine: &EngineState, stack: &Stack, line: &str) -> Result<String, String> {
    let mut engine = engine.clone();
    let block = parse_into(&mut engine, line)?;
    let expression = (block.pipelines.first())
        .and_then(|pipeline| pipeline.elements.first())
        .map(|element| &element.expr)
        .ok_or("no expression")?;
    let Expr::Call(call) = &expression.expr else {
        return Err(format!("not a call: {:?}", expression.expr));
    };
    let argument = (call.positional_iter().next()).ok_or("no positional argument")?;
    match eval_expression::<WithoutDebug>(&engine, &mut stack.clone(), argument) {
        Ok(Value::String { val, .. }) | Ok(Value::Glob { val, .. }) => Ok(val),
        Ok(other) => Err(format!("argument is not a string: {other:?}")),
        Err(error) => Err(format!("argument does not evaluate: {error:?}")),
    }
}

/// File names that need spelling: spaces, every quote, glob characters, brackets, `~`, a leading
/// dash, unicode that folds and normalizes oddly, an emoji. A trailing `/` is a directory, and a
/// `/` inside a name is a nested path.
#[rustfmt::skip]
pub const WEIRD: &[&str] = &[
    "plain.txt", "with space.txt", "it's.txt", "q\"uote.txt", "back`tick.txt", "$dollar.txt",
    "(paren).txt", "[bracket].txt", "{brace}.txt", "semi;colon.txt", "pi|pe.txt", "hash#tag.txt",
    "-dash.txt", "~tilde.txt", "back\\slash.txt", "star*.txt", "q?mark.txt", "a,b.txt", "x=y.txt",
    "amp&ersand.txt", "100%.txt", "ünï.txt", "emoji😀.txt", "İstanbul.txt", "straße.txt", "ﬁne.txt",
    "e\u{301}.txt", "true", "null", "123", "1.5", "0x1f", "a.b.c", ".hidden",
    "caps/Mixed Case.txt", "nested dir/inner file.txt", "nested dir/deeper/x.txt", "src/main.rs",
    "src/lib.rs", "src/completions/mod.rs", "crates/nu-cli/src/lib.rs", "crates/nu-cli/Cargo.toml",
    "crates/nu-command/src/lib.rs", "Cargo.toml", "README.md", "target/", "docs/", "script.nu",
    "mod/mod.nu", "tool*",
];

/// A directory of files to complete in, with an engine whose `PWD` is that directory.
pub struct World {
    pub engine: EngineState,
    pub stack: Stack,
    /// The temporary directory the world was made in, removed with it.
    _dir: Option<TempDir>,
    cwd: PathBuf,
}

impl World {
    /// A world whose working directory holds `entries`: a trailing `/` makes a directory, anything
    /// else a file (whose content is `content:<entry>`), and parents are created as needed.
    /// Returns the filesystem's refusal (a case-insensitive collision, an invalid name) as an
    /// error, so a property can skip names the platform cannot hold.
    pub fn new(entries: &[&str]) -> Result<Self, String> {
        let dir = tempfile::Builder::new()
            .prefix("nu-completion-")
            .tempdir()
            .map_err(|error| format!("cannot make a temp dir: {error}"))?;
        // Temp dirs live behind a symlink on macOS; the engine's cwd must be the real one.
        let cwd = (dir.path().canonicalize())
            .map_err(|error| format!("cannot resolve the temp dir: {error}"))?
            .join("work");
        fs::create_dir_all(&cwd).map_err(|error| format!("cannot make the cwd: {error}"))?;
        for entry in entries {
            create_entry(&cwd, entry)?;
        }
        Ok(Self {
            _dir: Some(dir),
            ..Self::at(cwd)?
        })
    }

    /// A world in the directory `cwd` as it is, which is not removed with the world.
    pub fn at(cwd: PathBuf) -> Result<Self, String> {
        let abs =
            (cwd.clone().try_into()).map_err(|error| format!("{cwd:?} is relative: {error:?}"))?;
        let (_, _, engine, stack) = new_engine_helper(abs);
        Ok(Self {
            engine,
            stack,
            _dir: None,
            cwd,
        })
    }

    /// Add one more entry (see [`World::new`]) to a world that exists.
    pub fn add(&self, entry: &str) -> Result<(), String> {
        create_entry(&self.cwd, entry)
    }

    /// Put empty executables of these names in a directory and make it the whole of `PATH`.
    pub fn set_path_executables(&mut self, names: &[&str]) -> Result<(), String> {
        let bin = self.cwd.with_file_name("bin");
        fs::create_dir_all(&bin).map_err(|error| format!("cannot make {bin:?}: {error}"))?;
        for name in names {
            self.add_path_executable(name)?;
        }
        let path_variable = if cfg!(windows) { "Path" } else { "PATH" };
        let dir = (bin.to_string_lossy().replace('\\', "\\\\")).replace('"', "\\\"");
        self.run(&format!("$env.{path_variable} = [\"{dir}\"]"))
            .map(|_| ())
    }

    /// Add one more empty executable to the directory that [`World::set_path_executables`] made
    /// the `PATH`, without changing the `PATH` itself.
    pub fn add_path_executable(&self, name: &str) -> Result<(), String> {
        let path = self.cwd.with_file_name("bin").join(name);
        fs::write(&path, "").map_err(|error| format!("cannot write {path:?}: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("cannot chmod {path:?}: {error}"))?;
        }
        Ok(())
    }

    /// Backdate the modification time of every directory in the world, as if each had been made
    /// an hour ago: a directory the completion cache may trust not to change unnoticed. Later
    /// additions to a directory give it a new modification time again.
    pub fn age_directories(&self) -> Result<(), String> {
        fn age(dir: &Path, then: std::time::SystemTime) -> std::io::Result<()> {
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    age(&path, then)?;
                }
            }
            fs::File::open(dir)?.set_modified(then)
        }
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let bin = self.cwd.with_file_name("bin");
        for dir in [self.cwd.as_path(), bin.as_path()] {
            if dir.is_dir() {
                age(dir, then).map_err(|error| format!("cannot age {dir:?}: {error}"))?;
            }
        }
        Ok(())
    }

    /// The working directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Run `source` in the world, merging what it defines and sets.
    pub fn run(&mut self, source: &str) -> Result<Value, String> {
        eval_source(&mut self.engine, &mut self.stack, source)
    }

    /// Evaluate `source` in a copy of the world, leaving the world as it was.
    pub fn eval(&self, source: &str) -> Result<Value, String> {
        eval_source(&mut self.engine.clone(), &mut self.stack.clone(), source)
    }

    /// [`complete`] in this world.
    pub fn complete(&mut self, line: &str, cursor: usize) -> Vec<Candidate> {
        complete(&mut self.engine, &mut self.stack, line, cursor)
    }

    /// A [`Session`] over a snapshot of the world.
    pub fn session(&self) -> Session {
        Session::new(&self.engine, &self.stack)
    }
}

/// Create one `entry` of a world under `root`.
fn create_entry(root: &Path, entry: &str) -> Result<(), String> {
    let path = root.join(entry);
    let refused = |error: std::io::Error| format!("cannot create {entry:?}: {error}");
    if entry.ends_with('/') {
        return fs::create_dir_all(&path).map_err(refused);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(refused)?;
    }
    // `create_new` refuses a name that already exists, which includes a case-insensitive or
    // normalization-insensitive collision with an earlier entry.
    let mut file = (fs::OpenOptions::new().write(true).create_new(true))
        .open(&path)
        .map_err(refused)?;
    std::io::Write::write_all(&mut file, format!("content:{entry}").as_bytes()).map_err(refused)
}
