use super::commandline::extract_input_buffer;
use crate::{
    NuCompleter, NuHighlighter, NuHinter,
    completions::{
        CompletionCache, CompletionEngine, LatestAnswer, Trace, UsageIndex,
        flush_completion_warnings, history_reader,
    },
    status_line::{NuStatusLine, StatusStyles},
};
use nu_color_config::StyleComputer;
use nu_engine::command_prelude::*;
use nu_utils::time::Instant;
use reedline::{FileBackedHistory, Hinter, StatusLine};
use std::{
    ops::Range,
    sync::{Arc, OnceLock},
    time::Duration,
};

/// What this command remembers between runs, as the line editor remembers between prompts: so
/// that `cached` can say a source was answered from it.
static CACHE: OnceLock<CompletionCache> = OnceLock::new();

/// What the history says about the commands, arguments and directories the user uses, as the
/// line editor ranks by it: read here, from the history the configuration names.
fn learned_usage(engine_state: &EngineState) -> Option<Arc<UsageIndex>> {
    // Read again only when the history has grown, as the line editor's completer reads it: a
    // scan of a large history would be most of what a trace times.
    static LEARNED: std::sync::Mutex<Option<Arc<UsageIndex>>> = std::sync::Mutex::new(None);
    let history = history_reader(engine_state)?()?;
    let len = history.count_all().unwrap_or_default();
    let mut learned = LEARNED.lock().ok()?;
    if let Some(usage) = learned.as_ref().filter(|usage| usage.len() == len) {
        return Some(Arc::clone(usage));
    }
    let usage = Arc::new(UsageIndex::scan(history.as_ref(), |name| {
        engine_state.find_decl(name.as_bytes(), &[]).is_some()
    }));
    *learned = Some(Arc::clone(&usage));
    Some(usage)
}

#[derive(Clone)]
pub struct DebugComplete;

impl Command for DebugComplete {
    fn name(&self) -> &str {
        "debug complete"
    }

    fn description(&self) -> &str {
        "Trace how a line is completed: how it was read, which sources were asked and what each \
         did, what was dropped, and what is shown."
    }

    fn extra_description(&self) -> &str {
        "The line is completed as `commandline complete` completes it, with the cursor at its \
end. Without input, the current commandline is completed at its cursor. `--cursor` puts the cursor \
at a byte of the line instead, and wins over a marker. A `|` that ends a line given as input marks \
the cursor and is not part of the line, so `'git checkout |'` completes the arguments of \
`git checkout`; write `||` to complete after a pipe.

The record holds:

- `line` and `cursor`: what was completed, without the marker.
- `target`: the `{start, end}` range of the line that a suggestion replaces.
- `site`: what is completed. Its `kind` is what a completer reads as `place.kind`; its `command`
  is the command whose argument it is, with its `kind` (built-in, custom, external, ...) and the
  `completer` it declares for the whole call; its `parameter` is the argument it is, with the
  `shape` and `completer` declared for it; its `input` is the type and columns piped in.
- `inputs`: the `token`, `place` and `buffer` a custom completer is handed here, whichever it
  declares.
- `sources`: every source, in the order it was considered. A line may be read more than one way
  (`git ch` is also the argument `ch` of `git`); `reading` says which, and `other_sites` has the
  `site` and `inputs` of the readings after the first.
- `dropped`: some of the candidates that matching removed (`dropped_total` counts them all),
  with the `source` that offered each and the `reason`.
- `shown`: the suggestions, in order, as `commandline complete --explain` reports them. This command
  reads the history, so the `used` term of `why` is the frecency of the candidate there.
- `message`: why there is nothing to suggest, when a completer or a delegated source said so. It is
  what the line editor shows on its hint line.
- `elapsed`: the time all this took.

Each source has a `name`, which is the `source` of the suggestions it offers; a `role`, `primary`
or `fallback` (asked only because the sources before it left the slot open, such as files after
nothing answered); and an `outcome`:

- `answered`: it offered `candidates`, of which matching `kept` some. A source that matches while
  it reads, such as the files, counts the candidates it kept.
- `declined`: it had nothing to say, so the next was asked.
- `skipped`: an earlier source answered, so it was not asked.
- `failed`: a custom completer's block failed; `error` is the error.
- `needs_terminal`: an `@interactive` completer, which this command does not run.
- `ran`, `cached`, `refused` and `timed_out`: the `evaluation` source, which runs the pipeline before the
  cursor to complete from its data (as in `ls | where type == `). It `ran` it, took what it ran a moment ago
  from the cache (`cached`), `refused` to run it because it may write (the `error` says why), or
  stopped it past its time budget (`timed_out`). `candidates` is the rows it read.

`cached` says whether the cache answered (`hit`) or the source had to look (`miss`), or `none`
where it keeps nothing. The cache is this command's own, kept between its runs as the line
editor keeps its own between prompts; it is empty the first time. A custom completer's source also has `handed`, the inputs it declares;
`raw`, what it returned before it was read; and `faults`, the parts of that which were not used
as given, with the `reason` and the position `at` in the returned list."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![
                (Type::String, Type::record()),
                (Type::Nothing, Type::record()),
            ])
            .optional(
                "line",
                SyntaxShape::String,
                "The line to complete, in place of the input or the current commandline. A `|` at its end marks the cursor.",
            )
            .named(
                "cursor",
                SyntaxShape::Int,
                "Complete with the cursor at this byte of the line, instead of at its end.",
                Some('c'),
            )
            .switch(
                "editor",
                "Also report what the line editor shows and does there: the status line, the signs \
                 at the line's end and the history hint.",
                Some('e'),
            )
            .category(Category::Debug)
    }

    fn search_terms(&self) -> Vec<&str> {
        vec!["completion", "completer", "explain", "trace", "why"]
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let head = call.head;
        // A line given as an argument stands for the same line given as input.
        let input = match call.opt::<Spanned<String>>(engine_state, stack, 0)? {
            Some(line) => PipelineData::value(Value::string(line.item, line.span), None),
            None => input,
        };
        let (line, end) =
            extract_input_buffer(&input, engine_state, head, input.span().unwrap_or(head))?;
        let (line, cursor) = match call.get_flag::<i64>(engine_state, stack, "cursor")? {
            Some(at) => (line.into_owned(), usize::try_from(at).unwrap_or(0)),
            // Only a line given as input has a marker; the current commandline is as it is.
            None => match (&input, line.strip_suffix('|')) {
                (PipelineData::Value(..), Some(marked)) => (marked.to_owned(), marked.len()),
                _ => (line.into_owned(), end),
            },
        };
        let cursor = line.floor_char_boundary(cursor.min(line.len()));

        let trace = Trace::default();
        let started = Instant::now();
        let engine = CompletionEngine::new(engine_state, stack)
            .caching_in(CACHE.get_or_init(CompletionCache::default))
            .traced(trace.clone())
            .with_usage(learned_usage(engine_state));
        let shown = engine.fetch_completions_for_editor(&line, cursor);
        let message = engine.message();
        let elapsed = started.elapsed();
        flush_completion_warnings(engine_state, stack);

        let editor = call
            .has_flag(engine_state, stack, "editor")?
            .then(|| editor_view(engine_state, stack, &line, cursor, head));
        let mut report = record! {
            "line" => Value::string(line, head),
            "cursor" => Value::int(cursor as i64, head),
        };
        report.extend(trace.finish(head));
        report.insert(
            "shown",
            Value::list(
                shown
                    .into_iter()
                    .map(|suggestion| suggestion.explain(head))
                    .collect(),
                head,
            ),
        );
        if let Some(message) = message {
            report.insert("message", Value::string(message, head));
        }
        if let Some(editor) = editor {
            report.insert("editor", editor);
        }
        report.insert(
            "elapsed",
            Value::duration(i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX), head),
        );
        Ok(Value::record(report, head).into_pipeline_data())
    }

    fn examples(&self) -> Vec<Example<'_>> {
        vec![
            Example {
                description: "See what the status line, the signs and the history hint say.",
                example: "'ls | where name = |' | debug complete --editor | get editor",
                result: None,
            },
            Example {
                description: "See which sources were asked for an argument, and what each did.",
                example: "'ls ' | debug complete | get sources | select name role outcome kept",
                result: None,
            },
            Example {
                description: "See what the argument a completer is attached to is, and what the \
                              completer is handed there. A `|` ends the line and marks the cursor.",
                example: "def pick [place: record] { [a b] }; def cmd [x: string@pick] {}; \
                          'cmd |' | debug complete | get inputs.place",
                result: None,
            },
            Example {
                description: "Give the line as an argument; a `|` at its end marks the cursor.",
                example: "debug complete 'git checkout ma|' | get shown.value",
                result: None,
            },
            Example {
                description: "See why a candidate is not offered.",
                example: "'ls Car' | debug complete | get dropped",
                result: None,
            },
            Example {
                description: "Put the cursor in the middle of the line.",
                example: "'ls Cargo.toml' | debug complete --cursor 4 | get target",
                result: None,
            },
        ]
    }
}

/// What the line editor shows for `line` with the cursor at `cursor`, as its REPL wires the
/// completer, status line and history hint together: the status line (plain, as wide as 80
/// columns) and the column it is anchored at, the sign at the line's end with the spans it
/// marks, and the hint the history gives once the completer has answered.
fn editor_view(
    engine_state: &EngineState,
    stack: &Stack,
    line: &str,
    cursor: usize,
    head: Span,
) -> Value {
    let engine = Arc::new(engine_state.clone());
    let shared_stack = Arc::new(stack.clone());
    let parse_cache =
        NuHighlighter::new(Arc::clone(&engine), Arc::clone(&shared_stack)).parse_cache();
    let latest = LatestAnswer::default();
    NuCompleter::new(Arc::clone(&engine), shared_stack)
        .with_parse_cache(parse_cache.clone())
        .with_latest(latest.clone())
        .complete_blocking(line, cursor);

    let config = stack.get_config(engine_state);
    let styles =
        StatusStyles::from_config(&config, &StyleComputer::from_config(engine_state, stack));
    let mut status_line =
        NuStatusLine::new(Arc::clone(&engine), parse_cache, config.status_line, styles);
    // What the status line runs, it runs off the paint and shows on a later one: paint until a
    // paint leaves nothing running (for at most two seconds).
    let mut status = status_line.status(line, cursor, 80, false);
    for _ in 0..100 {
        if status_line.is_idle() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
        status = status_line.status(line, cursor, 80, false);
    }
    let span = |range: &Range<usize>| {
        Value::record(
            record! {
                "start" => Value::int(range.start as i64, head),
                "end" => Value::int(range.end as i64, head),
            },
            head,
        )
    };
    let sign = status_line.sign(line, cursor).map_or_else(
        || Value::nothing(head),
        |sign| {
            Value::record(
                record! {
                    "note" => sign.note.map_or_else(|| Value::nothing(head), |note| Value::string(note, head)),
                    "span" => sign.span.as_ref().map_or_else(|| Value::nothing(head), span),
                    "marks" => Value::list(sign.marks.iter().map(span).collect(), head),
                },
                head,
            )
        },
    );

    let history = history_reader(engine_state)
        .and_then(|open| open())
        .unwrap_or_else(|| Box::<FileBackedHistory>::default());
    let cwd = engine_state
        .cwd(Some(stack))
        .map(|cwd| cwd.into_std_path_buf().to_string_lossy().into_owned())
        .unwrap_or_default();
    let hint = NuHinter::new(engine, Default::default())
        .with_answers(latest)
        .handle(line, cursor, history.as_ref(), false, &cwd);

    Value::record(
        record! {
            "status" => Value::string(status, head),
            "anchor" => status_line
                .anchor(line, cursor)
                .map_or_else(|| Value::nothing(head), |at| Value::int(at as i64, head)),
            "sign" => sign,
            "hint" => Value::string(hint, head),
        },
        head,
    )
}
