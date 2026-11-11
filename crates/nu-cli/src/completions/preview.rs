//! Previews of suggestions: what a command takes, what a directory holds, how a file begins,
//! what a variable is. Each is a value drawn as a Nushell table the width it is given, the
//! panel framing the rows.
//!
//! A custom completer may say what to preview with a `preview` key in the record of a
//! suggestion: a value, or a closure that returns one and is called only once the suggestion
//! is selected. Its `help` key, of the same kind, is what shows once the preview is unfolded,
//! as does the description past its first line, which the hint line does not show.
//!
//! What is offered as a flag or an argument of a command also folds the command's help for that
//! place under its preview: what the flag or the parameter is, and the examples that use it.

use super::{
    Detail, Details, ENV_VAR_SOURCE, HelpFor, catch_completion_panic, command_help,
    command_help::RowMap, data::evaluate_text, kind_name, report,
};
use crate::status_line::StatusStyles;
use nu_ansi_term::Style;
use nu_engine::{ClosureEvalOnce, eval_call, eval_variable};
use nu_protocol::{
    Config, DeclId, ExamplesMode, PipelineData, Record, ShellError, Span, Spanned, SuggestionKind,
    TrimStrategy, Type, Value,
    ast::{Argument, Call, Expr, Expression},
    debugger::WithoutDebug,
    engine::{EngineState, Stack, StateWorkingSet},
};
use nu_table::string_wrap;
use reedline::{Completions, Pick, Preview};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Columns the panel's frame takes around a preview.
const FRAME_COLUMNS: u16 = 4;
/// Rows of a preview at most, the panel's frame not counted. The panel shows what fits and
/// scrolls through the rest.
pub(super) const ROWS: usize = 200;
/// The most rows of what a line gives that a hover shows; a longer result is left out.
const HOVER_RESULT_ROWS: usize = 12;
/// Entries of a directory a preview lists at most.
const ENTRIES: usize = ROWS;
/// Lines of a file a preview shows at most.
const LINES: usize = ROWS;
/// Bytes of a file read for its preview at most, so a huge file costs little.
const HEAD: u64 = 64 * 1024;
/// Bytes of a file read to parse it, when it is a structured one.
const PARSED: u64 = 256 * 1024;
/// The theme of the tables: a rule under the header, and no edges for the frame to double.
const THEME: &str = "light";

/// The preview `$env.config.completions.previewers` gives `item`: the closure for its kind, as
/// `debug complete` names it (`file`, `directory`, `command`), or for a command the kind of
/// command first (`external`). It is given `{value, display, kind, description, path, width}`,
/// `path` absolute for a file or a directory, and returns text, a list of lines, or a value drawn
/// as a table. `None` when there is no closure for the kind, or it returns nothing, which leaves
/// the shell's own preview.
fn previewed_by_config(
    engine_state: &EngineState,
    stack: &Stack,
    item: &reedline::Suggestion,
    detail: &Detail,
    width: usize,
) -> Result<Option<Vec<String>>, ShellError> {
    let config = stack.get_config(engine_state);
    let Some(kind) = &detail.kind else {
        return Ok(None);
    };
    let (name, of) = kind_name(kind);
    let previewer = (of
        .as_deref()
        .and_then(|of| config.completions.previewers.get(of)))
    .or_else(|| config.completions.previewers.get(name));
    let Some(previewer) = previewer else {
        return Ok(None);
    };
    let span = Span::unknown();
    let display = item.display_value();
    let path = matches!(kind, SuggestionKind::File | SuggestionKind::Directory)
        .then(|| engine_state.cwd(Some(stack)).ok())
        .flatten()
        .map(|cwd| nu_path::expand_path_with(display, cwd, true));
    let text =
        |text: Option<&str>| text.map_or(Value::nothing(span), |text| Value::string(text, span));
    let record = Value::record(
        nu_protocol::record! {
            "value" => Value::string(&item.value, span),
            "display" => Value::string(display, span),
            "kind" => Value::string(of.unwrap_or_else(|| name.to_owned()), span),
            "description" => text(item.description.as_deref()),
            "path" => text(path.as_ref().and_then(|path| path.to_str())),
            "width" => Value::int(i64::try_from(width).unwrap_or(i64::MAX), span),
        },
        span,
    );
    let value = ClosureEvalOnce::try_new(engine_state, stack, previewer.clone(), span)?
        .add_arg(record)?
        .run_with_input(PipelineData::empty())?
        .into_value(span)?;
    Ok(match value {
        Value::Nothing { .. } => None,
        Value::String { val, .. } => Some(val.lines().map(str::to_owned).collect()),
        Value::List { vals, .. }
            if vals.iter().all(|line| matches!(line, Value::String { .. })) =>
        {
            Some(
                vals.iter()
                    .filter_map(|line| line.as_str().ok())
                    .map(str::to_owned)
                    .collect(),
            )
        }
        value => Some(draw(engine_state, stack, value, width, ROWS)?),
    })
}

/// What the command `decl_id` gives when it is run on what is piped into it, `input`, if it needs
/// nothing else and may run: `ls | first` shows the first row. Only a pipeline of commands that
/// only read is run, within a budget, as completion reads data; see [`data`](super::data).
fn at_work(
    engine_state: &EngineState,
    stack: &Stack,
    decl_id: DeclId,
    input: Option<&str>,
    width: usize,
) -> Result<Option<Vec<String>>, ShellError> {
    let Some(input) = input.filter(|input| !input.is_empty()) else {
        return Ok(None);
    };
    if decl_id.get() >= engine_state.num_decls() {
        return Ok(None);
    }
    // One that is told what to work on, such as `select` with its columns, shows nothing of use
    // without being told: its examples are what it does.
    let decl = engine_state.get_decl(decl_id);
    let signature = decl.signature();
    if !signature.required_positional.is_empty() || signature.rest_positional.is_some() {
        return Ok(None);
    }
    let Some(value) = evaluate_text(engine_state, stack, &format!("{input} | {}", decl.name()))
    else {
        return Ok(None);
    };
    let rows = draw(engine_state, stack, (*value).clone(), width, ROWS)?;
    Ok((!rows.is_empty()).then_some(rows))
}

/// `rows` shown first, and `help` folded behind them: what was shown, and then what was folded.
///
/// What is folded is the help, a part of its own: it starts with a rule that says so, `rule` made
/// of its title.
fn behind(
    rows: Vec<String>,
    (help, examples): (Preview, RowMap),
    rule: impl Fn(&str) -> String,
) -> (Preview, RowMap) {
    let summary = match examples.len() {
        0 if help.is_empty() => String::new(),
        0 => "help".to_owned(),
        1 => "1 example".to_owned(),
        count => format!("{count} examples"),
    };
    let mut more = Vec::new();
    if !help.is_empty() {
        more.push(rule(&summary));
    }
    more.extend(help.rows);
    more.extend(help.more);
    // The examples move down behind the rows put before them, and the rule.
    let shown = rows.len() + usize::from(!summary.is_empty());
    let examples = examples
        .into_iter()
        .map(|(rows, code)| (shown + rows.start..shown + rows.end, code))
        .collect();
    (
        Preview {
            rows,
            more,
            summary,
            ..Preview::default()
        },
        examples,
    )
}

/// The preview of `completions.items[index]`, its rows at most `width` columns wide, the panel's
/// frame included, for the line and cursor `place` it is shown at. Each example in it is a pick,
/// which takes it over the command at the cursor. Empty when there is nothing to show for it, or
/// showing it failed.
pub(crate) fn preview(
    engine_state: &EngineState,
    stack: &Stack,
    completions: &Completions,
    index: usize,
    width: u16,
    place: Option<(&str, usize)>,
) -> Preview {
    let made = preview_with_examples(engine_state, stack, completions, index, width, place);
    picked(engine_state, Some((completions, index)), made, place)
}

/// The preview of what the cursor is on in a normal mode of the line, where nothing is offered to
/// insert: what the line gives through the pipeline element the cursor is in, when it may be run
/// as completion runs data, and folded behind it the preview of the token itself, `hovered`,
/// which is its examples for a command or a column of a condition.
pub(crate) fn hover(
    engine_state: &EngineState,
    stack: &Stack,
    hovered: Option<&Completions>,
    width: u16,
    line: &str,
    pos: usize,
) -> Preview {
    let place = Some((line, pos));
    let made = hovered.map_or_else(Default::default, |completions| {
        preview_with_examples(engine_state, stack, completions, 0, width, place)
    });
    let cols = usize::from(width.saturating_sub(FRAME_COLUMNS)).max(8);
    // What the line gives is shown only for a command, and only when it is short: values are for
    // the completions to show, and a table of a thousand rows under a word is no hover.
    let on_a_command = hovered
        .and_then(|completions| completions.data::<Details>()?.0.first())
        .is_some_and(|detail| matches!(detail.kind, Some(SuggestionKind::Command(..))));
    let gives = pipeline_through(engine_state, line, pos)
        .filter(|_| on_a_command)
        .and_then(|text| evaluate_text(engine_state, stack, &text))
        .and_then(|value| {
            draw(
                engine_state,
                stack,
                (*value).clone(),
                cols,
                HOVER_RESULT_ROWS,
            )
            .ok()
        })
        .filter(|rows| !rows.is_empty() && rows.len() <= HOVER_RESULT_ROWS);
    let made = match gives {
        Some(rows) => behind(rows, made, |title| rule(engine_state, stack, title, cols)),
        None => made,
    };
    picked(
        engine_state,
        hovered.map(|completions| (completions, 0)),
        made,
        place,
    )
}

/// `preview` with each of its `examples` a pick, which takes it over the command at the cursor
/// of `place`, as the suggestion `of` names it.
fn picked(
    engine_state: &EngineState,
    of: Option<(&Completions, usize)>,
    (mut preview, examples): (Preview, RowMap),
    place: Option<(&str, usize)>,
) -> Preview {
    if let Some((line, cursor)) = place {
        let head = of
            .and_then(|(completions, index)| completions.data::<Details>()?.0.get(index))
            .and_then(|detail| command_of(engine_state, detail));
        preview.picks = (examples.into_iter())
            .map(|(rows, code)| Pick {
                rows,
                suggestion: super::examples::take_into(&code, head.as_deref(), line, cursor),
            })
            .collect();
    }
    preview
}

/// The quiet style of what a preview says besides what it previews: that of the status line
/// and the hint line over the panel, which the history hints do not share.
pub(super) fn quiet(engine_state: &EngineState, stack: &Stack) -> Style {
    let config = stack.get_config(engine_state);
    let styles = nu_color_config::StyleComputer::from_config(engine_state, stack);
    StatusStyles::from_config(&config, &styles).text
}

/// A rule across `cols` columns with `title` in it, as the edges of the panel's frame have:
/// `─ title ────`. It starts a part of a preview, such as its help.
fn rule(engine_state: &EngineState, stack: &Stack, title: &str, cols: usize) -> String {
    let title = cut(title, cols.saturating_sub(5));
    let fill = cols.saturating_sub(title.width() + 3);
    quiet(engine_state, stack)
        .paint(format!("─ {title} {}", "─".repeat(fill)))
        .to_string()
}

/// The text of the pipeline of `line` through the element at byte `pos`, when that is one of the
/// line's own pipelines rather than a block's.
fn pipeline_through(engine_state: &EngineState, line: &str, pos: usize) -> Option<String> {
    let mut working_set = StateWorkingSet::new(engine_state);
    let offset = working_set.next_span_start();
    let block = nu_parser::parse(&mut working_set, None, line.as_bytes(), false);
    let cursor = offset + pos;
    block.pipelines.iter().find_map(|pipeline| {
        let element = (pipeline.elements.iter())
            .find(|element| element.expr.span.start <= cursor && cursor <= element.expr.span.end)?;
        let start = pipeline
            .elements
            .first()?
            .expr
            .span
            .start
            .checked_sub(offset)?;
        let end = element.expr.span.end.checked_sub(offset)?;
        Some(line.get(start..end)?.trim().to_owned()).filter(|text| !text.is_empty())
    })
}

/// The name of the command whose help a suggestion's preview shows: the command it is, or the one
/// it is a flag or an argument of.
fn command_of(engine_state: &EngineState, detail: &Detail) -> Option<String> {
    match (&detail.kind, detail.help_at.as_deref()) {
        (Some(SuggestionKind::Command(_, Some(decl_id))), _) => {
            Some(engine_state.get_decl(*decl_id).name().to_owned())
        }
        (_, Some(at)) => Some(match &at.command {
            HelpFor::Decl(decl_id) => engine_state.get_decl(*decl_id).name().to_owned(),
            HelpFor::External(name) => name.clone(),
        }),
        _ => None,
    }
}

/// [`preview`] without its picks, and which of its rows hold an example, for a click on one to
/// take it.
pub(super) fn preview_with_examples(
    engine_state: &EngineState,
    stack: &Stack,
    completions: &Completions,
    index: usize,
    width: u16,
    place: Option<(&str, usize)>,
) -> (Preview, RowMap) {
    let Some(item) = completions.items.get(index) else {
        return Default::default();
    };
    let detail = completions.data::<Details>().and_then(|d| d.0.get(index));
    let width = usize::from(width.saturating_sub(FRAME_COLUMNS)).max(8);
    let mode = stack.get_config(engine_state).completions.examples;
    let made = catch_completion_panic(|| -> Result<(Preview, RowMap), ShellError> {
        let name = item.display_value();
        let Some(detail) = detail else {
            return Ok(Default::default());
        };
        // What the line already writes of the command whose help this is ranks its examples.
        let command = command_of(engine_state, detail);
        let element = place.map(|(line, cursor)| &line[super::examples::command_at(line, cursor)]);
        let typed = command_help::Typed {
            input: detail.input.as_deref(),
            args: (element.zip(command.as_deref()))
                .map(|(element, name)| command_help::args_of(element, name))
                .unwrap_or_default(),
        };
        let (mut preview, mut examples) = if let Some(custom) = &detail.preview {
            (
                Preview::from(custom_rows(engine_state, stack, custom, width)?),
                RowMap::new(),
            )
        } else if let Some(rows) = previewed_by_config(engine_state, stack, item, detail, width)? {
            (Preview::from(rows), RowMap::new())
        } else {
            match &detail.kind {
                Some(SuggestionKind::Command(_, Some(decl_id))) => {
                    let help = if mode == ExamplesMode::Off {
                        Default::default()
                    } else {
                        command_help::command(engine_state, stack, *decl_id, width, &typed)?
                    };
                    match at_work(
                        engine_state,
                        stack,
                        *decl_id,
                        detail.input.as_deref(),
                        width,
                    )? {
                        Some(rows) => {
                            behind(rows, help, |title| rule(engine_state, stack, title, width))
                        }
                        None => help,
                    }
                }
                Some(SuggestionKind::Directory | SuggestionKind::File) => (
                    Preview::from(path_rows(engine_state, stack, name, width)?),
                    RowMap::new(),
                ),
                Some(SuggestionKind::Variable) => (
                    Preview::from(variable_rows(engine_state, stack, name, width)?),
                    RowMap::new(),
                ),
                Some(SuggestionKind::Value(_))
                    if detail.source.as_deref() == Some(ENV_VAR_SOURCE) =>
                {
                    match stack.get_env_var(engine_state, name) {
                        Some(value) => (
                            Preview::from(draw(engine_state, stack, value.clone(), width, ROWS)?),
                            RowMap::new(),
                        ),
                        None => Default::default(),
                    }
                }
                _ => Default::default(),
            }
        };
        // What the completer says to unfold, and the description past the line the hint has.
        // A command's own description is part of its help already.
        let own_help = matches!(&detail.kind, Some(SuggestionKind::Command(_, Some(_))));
        let mut extra = Vec::new();
        if let Some(help) = &detail.help {
            extra.extend(custom_rows(engine_state, stack, help, width)?);
        }
        if let Some(description) = item.description.as_deref().filter(|_| !own_help) {
            let rest = description.lines().skip(1).collect::<Vec<_>>().join("\n");
            if !rest.trim().is_empty() {
                if !extra.is_empty() {
                    extra.push(String::new());
                }
                extra.extend(draw(
                    engine_state,
                    stack,
                    Value::string(rest.trim(), Span::unknown()),
                    width,
                    ROWS,
                )?);
            }
        }
        if !extra.is_empty() {
            if !preview.more.is_empty() {
                preview.more.push(String::new());
            }
            preview.more.extend(extra);
        }
        // Help for where the suggestion sits in its command comes last, under all of that.
        let composed = match &detail.help_at {
            Some(_) if mode == ExamplesMode::Off => None,
            Some(at) => {
                let alone = preview.is_empty();
                command_help::at(
                    engine_state,
                    stack,
                    at,
                    item.value.trim(),
                    width,
                    alone,
                    &typed,
                )?
            }
            None => None,
        };
        if let Some(composed) = composed {
            // Under a preview of its own, the help is a part apart, which a rule names.
            if !preview.is_empty() {
                preview
                    .more
                    .push(rule(engine_state, stack, &composed.summary, width));
            }
            // The rows of the preview as reedline counts them: `rows`, then `more`.
            let at = preview.rows.len() + preview.more.len();
            examples.extend(
                composed
                    .map
                    .into_iter()
                    .map(|(rows, code)| (at + rows.start..at + rows.end, code)),
            );
            preview.more.extend(composed.rows);
            preview.summary = [preview.summary, composed.summary]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
        }
        // Shown from the start, what is folded away is under the rest, and rows count the same.
        if mode == ExamplesMode::Shown && !preview.more.is_empty() {
            let more = std::mem::take(&mut preview.more);
            preview.rows.extend(more);
            preview.summary.clear();
        }
        Ok((preview, examples))
    });
    match made {
        Ok(Ok(made)) => made,
        Ok(Err(err)) => {
            report(format!("a preview failed: {err}"));
            Default::default()
        }
        Err(_) => Default::default(),
    }
}

/// `text` cut to `cols` columns, an ellipsis standing for what was cut.
pub(crate) fn cut(text: &str, cols: usize) -> String {
    if text.width() <= cols {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let width = grapheme.width();
        if used + width + 1 > cols {
            break;
        }
        out.push_str(grapheme);
        used += width;
    }
    if cols > 0 {
        out.push('…');
    }
    out
}

/// What a completer said to preview: the value, or what the closure returns.
fn custom_rows(
    engine_state: &EngineState,
    stack: &Stack,
    custom: &Value,
    width: usize,
) -> Result<Vec<String>, ShellError> {
    let span = Span::unknown();
    let value = match custom {
        Value::Closure { val, .. } => {
            ClosureEvalOnce::try_new(engine_state, stack, (**val).clone(), span)?
                .run_with_input(PipelineData::empty())?
                .into_value(span)?
        }
        value => value.clone(),
    };
    draw(engine_state, stack, value, width, ROWS)
}

/// The value of a variable, as the line has it in scope.
fn variable_rows(
    engine_state: &EngineState,
    stack: &Stack,
    name: &str,
    width: usize,
) -> Result<Vec<String>, ShellError> {
    let working_set = StateWorkingSet::new(engine_state);
    let Some(var_id) = working_set.find_variable(name.as_bytes()) else {
        return Ok(Vec::new());
    };
    let value = eval_variable(engine_state, stack, var_id, Span::unknown())?;
    draw(engine_state, stack, value, width, ROWS)
}

/// What the directory or file named `name` is: its entries, or its metadata and its start.
fn path_rows(
    engine_state: &EngineState,
    stack: &Stack,
    name: &str,
    width: usize,
) -> Result<Vec<String>, ShellError> {
    let Ok(cwd) = engine_state.cwd(Some(stack)) else {
        return Ok(Vec::new());
    };
    let path = nu_path::expand_path_with(name.trim_end_matches(['/', '\\']), cwd, true);
    if path.is_dir() {
        let span = Span::unknown();
        let entries = entries(&path).into_iter().map(|(is_dir, name, size)| {
            let mut record = Record::new();
            record.push("name", Value::string(name, span));
            record.push(
                "type",
                Value::string(if is_dir { "dir" } else { "file" }, span),
            );
            record.push(
                "size",
                if is_dir {
                    Value::nothing(span)
                } else {
                    Value::filesize(i64::try_from(size).unwrap_or(i64::MAX), span)
                },
            );
            Value::record(record, span)
        });
        let list = Value::list(entries.collect(), span);
        return draw(engine_state, stack, list, width, ROWS);
    }
    let Ok(metadata) = fs::metadata(&path) else {
        return Ok(Vec::new());
    };
    if !metadata.is_file() {
        return Ok(Vec::new());
    }
    let mut rows = draw(engine_state, stack, file_record(&metadata), width, ROWS)?;
    // The rows left, after a blank one to set the file apart from its metadata. A few rows of
    // it are not worth the room.
    let room = ROWS.saturating_sub(rows.len() + 1);
    if room >= 3 {
        rows.push(String::new());
        let content = match parsed(engine_state, stack, &path, &metadata) {
            Some(value) => draw(engine_state, stack, value, width, room)?,
            None => file(&path, width),
        };
        rows.extend(content.into_iter().take(room));
    }
    Ok(rows)
}

/// What is known of a file without reading it: `{type, size, modified, permissions}`.
fn file_record(metadata: &fs::Metadata) -> Value {
    let span = Span::unknown();
    let mut record = Record::new();
    record.push("type", Value::string("file", span));
    record.push(
        "size",
        Value::filesize(i64::try_from(metadata.len()).unwrap_or(i64::MAX), span),
    );
    if let Ok(modified) = metadata.modified() {
        let modified = chrono::DateTime::<chrono::Local>::from(modified).fixed_offset();
        record.push("modified", Value::date(modified, span));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        record.push(
            "permissions",
            Value::string(mode_string(metadata.permissions().mode()), span),
        );
    }
    Value::record(record, span)
}

/// `mode` the way `ls -l` writes the permission bits: `rw-r--r--`.
#[cfg(unix)]
fn mode_string(mode: u32) -> String {
    "rwxrwxrwx"
        .chars()
        .enumerate()
        .map(|(bit, flag)| {
            if mode & (0o400 >> bit) != 0 {
                flag
            } else {
                '-'
            }
        })
        .collect()
}

/// What the file holds as a value, when it is a structured text file that is not large: the
/// command named `from <extension>` parses it.
fn parsed(
    engine_state: &EngineState,
    stack: &Stack,
    path: &Path,
    metadata: &fs::Metadata,
) -> Option<Value> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let format = match extension.as_str() {
        "json" | "toml" | "csv" | "tsv" | "nuon" => extension.as_str(),
        "yaml" | "yml" => "yaml",
        _ => return None,
    };
    if metadata.len() > PARSED {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let decl_id = engine_state.find_decl(format!("from {format}").as_bytes(), &[])?;
    let span = Span::unknown();
    let call = Call {
        decl_id,
        ..Call::new(span)
    };
    let mut stack = stack.clone().collect_value();
    eval_call::<WithoutDebug>(
        engine_state,
        &mut stack,
        &call,
        PipelineData::value(Value::string(text, span), None),
    )
    .ok()?
    .into_value(span)
    .ok()
}

/// `value` drawn as a table at most `width` columns wide and `rows` rows tall, with a count of
/// what did not fit. Text is wrapped instead. A long cell is cut.
pub(super) fn draw(
    engine_state: &EngineState,
    stack: &Stack,
    value: Value,
    width: usize,
    rows: usize,
) -> Result<Vec<String>, ShellError> {
    let cut = TrimStrategy::Truncate {
        suffix: Some("…".into()),
    };
    draw_trimmed(engine_state, stack, value, width, rows, cut)
}

/// [`draw`], with a long cell trimmed as `trim` says.
pub(super) fn draw_trimmed(
    engine_state: &EngineState,
    stack: &Stack,
    value: Value,
    width: usize,
    rows: usize,
    trim: TrimStrategy,
) -> Result<Vec<String>, ShellError> {
    if let Value::String { val, .. } = &value {
        return Ok(val
            .lines()
            .flat_map(|line| {
                string_wrap(line, width, true)
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .take(rows)
            .collect());
    }
    let total = match &value {
        Value::List { vals, .. } => vals.len(),
        Value::Record { val, .. } => val.len(),
        _ => 1,
    };
    // Rows tall are what the table takes, which the number of entries only starts to tell:
    // try fewer until it fits.
    let mut keep = total.min(rows).max(1);
    loop {
        let hidden = total.saturating_sub(keep);
        let mut lines = table_lines(engine_state, stack, first(&value, keep), width, &trim)?;
        let extra = usize::from(hidden > 0);
        if lines.len() + extra <= rows || keep == 1 {
            // A table with no room to draw its cells is rows of nothing: not a preview.
            if lines.iter().all(|line| line.trim().is_empty()) {
                return Ok(Vec::new());
            }
            if hidden > 0 {
                lines.push(format!("… {hidden} more"));
            }
            lines.truncate(rows);
            return Ok(lines);
        }
        keep = keep.saturating_sub(lines.len() + extra - rows).max(1);
    }
}

/// The first `count` entries of a list or fields of a record, or any other value as it is.
fn first(value: &Value, count: usize) -> Value {
    let span = Span::unknown();
    match value {
        Value::List { vals, .. } => Value::list(vals.iter().take(count).cloned().collect(), span),
        Value::Record { val, .. } => Value::record(
            val.iter()
                .take(count)
                .map(|(column, field)| (column.clone(), field.clone()))
                .collect(),
            span,
        ),
        value => value.clone(),
    }
}

/// The least a column of text is cut to.
const NARROWEST_COLUMN: usize = 6;

/// What a column costs beside what it holds: the padding around it and the rule after it.
const COLUMN_FRAME: usize = 3;

/// `value`, a table too wide for `width`, with its longest text cut so that the columns fit side
/// by side and the table fills the width, rather than leaving out every column after the first
/// wide one. Each column of text is cut only as far as the others need: the narrow ones keep all
/// they hold. Anything else, and a table that fits, is left as it is.
fn fit_columns(value: Value, width: usize, config: &Config) -> Value {
    let Value::List { vals, .. } = &value else {
        return value;
    };
    let Some(columns) = vals
        .first()
        .and_then(|row| row.as_record().ok())
        .map(|row| row.columns().cloned().collect::<Vec<String>>())
    else {
        return value;
    };
    let cell = |row: &Value, column: &str| {
        row.as_record()
            .ok()
            .and_then(|record| record.get(column).cloned())
    };
    // A table holding tables takes the room they need, which is not told by their text.
    if (vals.iter()).any(|row| {
        (columns.iter()).any(|column| {
            matches!(
                cell(row, column),
                Some(Value::List { .. } | Value::Record { .. })
            )
        })
    }) {
        return value;
    }
    let widest: Vec<usize> = (columns.iter())
        .map(|column| {
            (vals.iter())
                .filter_map(|row| cell(row, column))
                .map(|cell| cell.to_abbreviated_string(config).width())
                .max()
                .unwrap_or(0)
                .max(column.width())
        })
        .collect();
    let room = width.saturating_sub(columns.len() * COLUMN_FRAME);
    if widest.iter().sum::<usize>() <= room {
        return value;
    }
    let is_text = |column: &str| {
        (vals.iter())
            .filter_map(|row| cell(row, column))
            .all(|cell| matches!(cell, Value::String { .. } | Value::Nothing { .. }))
    };
    // The least a column can be cut to, and the level the columns of text are cut down to, which
    // is found by trying each from the widest.
    let least: Vec<usize> = (columns.iter().zip(&widest))
        .map(|(column, widest)| {
            if is_text(column) {
                (*widest).min(column.width().max(NARROWEST_COLUMN))
            } else {
                *widest
            }
        })
        .collect();
    let used = |level: usize| -> usize {
        (widest.iter().zip(&least))
            .map(|(widest, least)| level.min(*widest).max(*least))
            .sum()
    };
    let level = (0..=widest.iter().copied().max().unwrap_or(0))
        .rev()
        .find(|level| used(*level) <= room);
    // Not even the least of each fits: the table leaves columns out, as it would have.
    let Some(level) = level else {
        return value;
    };
    let caps: Vec<usize> = (widest.iter().zip(&least))
        .map(|(widest, least)| level.min(*widest).max(*least))
        .collect();
    let span = value.span();
    let rows = vals
        .iter()
        .map(|row| {
            let Ok(record) = row.as_record() else {
                return row.clone();
            };
            let cut: Record = record
                .iter()
                .map(|(column, cell)| {
                    let cap = columns
                        .iter()
                        .position(|name| name == column)
                        .map_or(usize::MAX, |at| caps[at]);
                    let cell = match cell {
                        Value::String { val, .. } if val.width() > cap => {
                            Value::string(cut(val, cap), cell.span())
                        }
                        cell => cell.clone(),
                    };
                    (column.clone(), cell)
                })
                .collect();
            Value::record(cut, row.span())
        })
        .collect();
    Value::list(rows, span)
}

/// The lines of `value` drawn by the `table` command, `width` columns wide, with no index and
/// nested data expanded, so a record holding a table shows the table.
fn table_lines(
    engine_state: &EngineState,
    stack: &Stack,
    value: Value,
    width: usize,
    trim: &TrimStrategy,
) -> Result<Vec<String>, ShellError> {
    let span = Span::unknown();
    let value = fit_columns(value, width, engine_state.get_config());
    let table = engine_state
        .find_decl(b"table", &[])
        .ok_or(ShellError::NotFound { span })?;
    let flag = |name: &str, value: Expression| {
        let name = Spanned {
            item: name.into(),
            span,
        };
        Argument::Named((name, None, Some(value)))
    };
    let argument = |expr, ty| Expression::new_unknown(expr, span, ty);
    let call = Call {
        decl_id: table,
        arguments: vec![
            flag(
                "width",
                argument(
                    Expr::Int(i64::try_from(width).unwrap_or(i64::MAX)),
                    Type::Int,
                ),
            ),
            flag("index", argument(Expr::Bool(false), Type::Bool)),
            flag("expand", argument(Expr::Bool(true), Type::Bool)),
            flag("theme", argument(Expr::String(THEME.into()), Type::String)),
        ],
        ..Call::new(span)
    };
    let mut stack = stack.clone().collect_value();
    let mut config = (*stack.get_config(engine_state)).clone();
    config.table.trim = trim.clone();
    stack.config = Some(std::sync::Arc::new(config));
    let drawn = eval_call::<WithoutDebug>(
        engine_state,
        &mut stack,
        &call,
        PipelineData::value(value, None),
    )?
    .collect_string("", &stack.get_config(engine_state))?;
    Ok(drawn.lines().map(str::to_owned).collect())
}

/// A plain-text preview of the directory or file at `path`, at most `width` columns wide: what
/// the completion panel previews, without styling, for a client that is not a terminal.
pub fn preview_path(engine_state: &EngineState, path: &Path, width: u16) -> Vec<String> {
    let name = path.to_string_lossy();
    path_rows(
        engine_state,
        &Stack::new(),
        &name,
        usize::from(width).max(16),
    )
    .unwrap_or_default()
    .iter()
    .map(|row| nu_utils::strip_ansi_unlikely(row).into_owned())
    .collect()
}

/// The first entries of `dir`: directories first, then files, by name, as
/// `(is a directory, name, size)`.
fn entries(dir: &Path) -> Vec<(bool, String, u64)> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<(bool, String, u64)> = read
        .filter_map(Result::ok)
        .take(ENTRIES * 4)
        .map(|entry| {
            let metadata = entry.metadata().ok();
            let is_dir = metadata.as_ref().is_some_and(fs::Metadata::is_dir);
            let size = metadata.as_ref().map_or(0, fs::Metadata::len);
            (
                is_dir,
                entry.file_name().to_string_lossy().into_owned(),
                size,
            )
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    entries.truncate(ENTRIES);
    entries
}

/// The first lines of `file`, or nothing for one that is not text.
fn file(file: &Path, width: usize) -> Vec<String> {
    let Ok(handle) = fs::File::open(file) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for line in BufReader::new(std::io::Read::take(handle, HEAD))
        .lines()
        .take(LINES)
    {
        let Ok(line) = line else {
            // Not UTF-8: a binary file previews as nothing.
            return Vec::new();
        };
        let line = line.replace('\t', "    ");
        lines.push(
            nu_utils::strip_ansi_unlikely(&line)
                .chars()
                .take(width)
                .collect(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completions::{Detail, HelpAt, HelpFor, Slot, SpanClamp, map_value_completions};
    use nu_engine::eval_block;
    use nu_parser::parse;
    use reedline::Suggestion;
    use std::sync::Arc;

    #[test]
    fn a_table_too_wide_keeps_its_columns_by_cutting_its_longest_text() {
        let config = Config::default();
        let span = Span::unknown();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let row = Value::test_record(nu_protocol::record! {
            "commit" => Value::string(hash, span),
            "subject" => Value::string("a fairly long subject that goes on", span),
            "n" => Value::int(1, span),
        });
        let table = Value::list(vec![row], span);
        let fitted = fit_columns(table.clone(), 50, &config);
        let row = fitted.as_list().expect("a table")[0].clone();
        let row = row.as_record().expect("a row").clone();
        let width = |name: &str| {
            row.get(name)
                .map_or(0, |cell| cell.to_abbreviated_string(&config).width())
        };
        assert!(row.get("commit").is_some() && row.get("subject").is_some());
        assert!(width("commit") + width("subject") + width("n") + 9 <= 50);
        // Cut evenly: neither column takes the room of the other.
        assert!(width("commit").abs_diff(width("subject")) <= 1, "{row:?}");

        // A table that fits is left as it is.
        assert_eq!(fit_columns(table.clone(), 200, &config), table);
    }

    /// The preview of `ls`, as a completion shows it with `mode` for examples set.
    fn examples_of_ls(mode: &str) -> Preview {
        let (mut engine, _) = engine_and_value("1");
        let mut stack = stack();
        let source = format!("$env.config.completions.examples = '{mode}'");
        let mut working_set = StateWorkingSet::new(&engine);
        let block = parse(&mut working_set, None, source.as_bytes(), false);
        engine
            .merge_delta(working_set.render())
            .expect("merge_delta");
        eval_block::<WithoutDebug>(&engine, &mut stack, &block, PipelineData::empty())
            .expect("the setting");
        let decl = engine.find_decl(b"ls", &[]).expect("ls");
        let item = Suggestion {
            value: "ls".into(),
            ..Suggestion::default()
        };
        let details = Details(vec![Detail {
            kind: Some(SuggestionKind::Command(
                nu_protocol::engine::CommandType::Builtin,
                Some(decl),
            )),
            ..Detail::default()
        }]);
        let mut completions = Completions::new(vec![item]);
        completions.data = Some(Arc::new(details));
        preview(&engine, &stack, &completions, 0, 80, None)
    }

    #[test]
    fn examples_fold_show_or_are_left_out_as_configured() {
        let folded = examples_of_ls("folded");
        assert!(!folded.more.is_empty() && folded.summary.ends_with("examples"));
        let shown = examples_of_ls("shown");
        assert!(shown.more.is_empty() && shown.rows.len() >= folded.rows.len() + folded.more.len());
        let off = examples_of_ls("off");
        assert!(off.more.is_empty() && off.summary.is_empty() && off.picks.is_empty());
    }

    #[test]
    fn nothing_to_draw_is_drawn_as_nothing() {
        let (engine, empty) = engine_and_value("[]");
        draw(&engine, &stack(), empty, 40, ROWS).expect("no rows are drawn");
    }

    /// An engine with the shell commands, and the value of `source` in it.
    fn engine_and_value(source: &str) -> (EngineState, Value) {
        let mut engine =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        let mut working_set = StateWorkingSet::new(&engine);
        let block = parse(&mut working_set, None, source.as_bytes(), false);
        assert!(
            working_set.parse_errors.is_empty(),
            "{:?}",
            working_set.parse_errors
        );
        engine
            .merge_delta(working_set.render())
            .expect("merge_delta");
        let value =
            eval_block::<WithoutDebug>(&engine, &mut stack(), &block, PipelineData::empty())
                .expect("eval source")
                .body
                .into_value(Span::unknown())
                .expect("a value");
        (engine, value)
    }

    /// A stack in the current directory.
    fn stack() -> Stack {
        let mut stack = Stack::new();
        let cwd = std::env::current_dir().expect("a current directory");
        stack.add_env_var(
            "PWD".into(),
            Value::string(cwd.to_string_lossy(), Span::unknown()),
        );
        stack
    }

    /// The answer a custom completer's `suggestions` make, with their details.
    fn answer(suggestions: Value) -> Completions {
        let Value::List { vals, .. } = suggestions else {
            panic!("a list of suggestions");
        };
        let found = map_value_completions(
            vals.into_iter(),
            reedline::Span::new(0, 0),
            SpanClamp::within(""),
        );
        let items: Vec<_> = found.iter().map(|found| found.suggestion.clone()).collect();
        Completions::new(items).with_data(Details::of(&found).expect("details"))
    }

    #[test]
    fn a_record_preview_is_drawn_as_a_table() {
        let (engine, suggestions) = engine_and_value(
            "[{value: main, preview: {branch: main, commits: 12}}, {value: other}]",
        );
        let completions = answer(suggestions);

        let rows = preview(&engine, &stack(), &completions, 0, 60, None)
            .rows
            .join("\n");

        for cell in ["branch", "main", "commits", "12"] {
            assert!(rows.contains(cell), "{cell} is missing from\n{rows}");
        }
        assert!(rows.lines().count() >= 2, "a table, not a line:\n{rows}");
        assert!(preview(&engine, &stack(), &completions, 1, 60, None).is_empty());
    }

    /// The answer of one command suggestion, `name`, with `input` piped into it.
    fn command_answer(engine: &EngineState, name: &str, input: &str) -> Completions {
        let decl_id = engine.find_decl(name.as_bytes(), &[]).expect("a command");
        let found = [crate::SemanticSuggestion {
            suggestion: reedline::Suggestion {
                value: name.to_owned(),
                ..Default::default()
            },
            kind: Some(SuggestionKind::Command(
                nu_protocol::engine::CommandType::Builtin,
                Some(decl_id),
            )),
            input: Some(input.into()),
            ..Default::default()
        }];
        let items: Vec<_> = found.iter().map(|found| found.suggestion.clone()).collect();
        Completions::new(items).with_data(Details::of(&found).expect("details"))
    }

    #[test]
    fn a_command_that_takes_nothing_else_previews_what_it_gives_with_its_examples_behind() {
        let (engine, _) = engine_and_value("1");
        let preview = preview(
            &engine,
            &stack(),
            &command_answer(&engine, "length", "[1 2 3]"),
            0,
            60,
            None,
        );
        assert_eq!(preview.rows, ["3"]);
        assert!(preview.summary.ends_with("examples"), "{}", preview.summary);
        assert!(!preview.more.is_empty());

        // One that needs an argument shows its examples, and one that may not run is not run.
        let preview = preview_rows(&engine, "where", "[1 2 3]");
        assert!(!preview.contains(&"3".to_owned()) || preview.len() > 1);
        let preview = preview_rows(&engine, "length", "[1 2 3] | each { $in }");
        assert_ne!(preview, ["3"], "a closure is not run");
    }

    #[test]
    fn an_example_that_says_what_it_is_about_is_the_help_for_that_alone() {
        let (engine, _) = engine_and_value(
            r#"@example "Make a branch" "tool -b <name>" --for [-b]
               @example "Make one, listing the rest" "tool -b x --list" --for [--list]
               extern tool [-b: string, --list]"#,
        );
        let completions = offered_in(&engine, "tool", "-b", Slot::Flag);
        let help = preview(&engine, &stack(), &completions, 0, 70, None);
        let text = nu_utils::strip_ansi_string_unlikely([help.rows, help.more].concat().join("\n"));
        assert!(text.contains("Make a branch"), "{text}");
        assert!(
            !text.contains("listing the rest"),
            "it says it is about --list:\n{text}"
        );
    }

    #[test]
    fn a_command_told_what_to_work_on_is_not_run_without_it() {
        let (engine, _) = engine_and_value("1");
        let rows = preview_rows(&engine, "select", "[[a b]; [1 2]]");
        let rows = nu_utils::strip_ansi_string_unlikely(rows.join("\n"));
        assert!(rows.contains("select"), "its examples:\n{rows}");
    }

    #[test]
    fn a_previewer_in_the_config_previews_its_kind_in_place_of_the_shells() {
        let (mut engine, previewer) =
            engine_and_value(r#"{|item| $"seen ($item.display), a ($item.kind)" }"#);
        let Value::Closure { val, .. } = previewer else {
            panic!("a closure");
        };
        let mut config = engine.get_config().as_ref().clone();
        config.completions.previewers.insert("file".into(), *val);
        engine.set_config(config);
        let (_, suggestions) = engine_and_value("[{value: Cargo.toml, kind: file}]");
        let preview = preview(&engine, &stack(), &answer(suggestions), 0, 60, None);
        assert_eq!(preview.rows, ["seen Cargo.toml, a file"]);
    }

    fn preview_rows(engine: &EngineState, name: &str, input: &str) -> Vec<String> {
        preview(
            engine,
            &stack(),
            &command_answer(engine, name, input),
            0,
            60,
            None,
        )
        .rows
    }

    #[test]
    fn a_command_previews_its_examples_and_not_its_parameters() {
        let (engine, _) = engine_and_value("1");
        let preview = preview(
            &engine,
            &stack(),
            &command_answer(&engine, "str substring", ""),
            0,
            80,
            None,
        );
        let all =
            nu_utils::strip_ansi_string_unlikely([preview.rows, preview.more].concat().join("\n"));
        assert!(all.contains("str substring"), "{all}");
        for table in ["parameter", "input", "output"] {
            assert!(
                !all.lines()
                    .any(|line| line.split_whitespace().next() == Some(table)),
                "{table} in {all}"
            );
        }
    }

    #[test]
    fn a_closure_preview_runs_only_for_the_selected_suggestion() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = dir.path().join("log");
        let log = log.display();
        let (engine, suggestions) = engine_and_value(&format!(
            "[{{value: a, preview: {{|| 'a' | save --append '{log}'; {{ran: a}} }}}},
              {{value: b, preview: {{|| 'b' | save --append '{log}'; {{ran: b}} }}}}]"
        ));
        let completions = answer(suggestions);
        assert!(
            !std::path::Path::new(&log.to_string()).exists(),
            "run before it was selected"
        );

        let rows = preview(&engine, &stack(), &completions, 1, 60, None)
            .rows
            .join("\n");

        assert!(rows.contains("ran"), "{rows}");
        assert_eq!(fs::read_to_string(log.to_string()).expect("log"), "b");
    }

    #[test]
    fn a_file_previews_its_metadata_and_a_structured_one_as_a_table() {
        let (engine, _) = engine_and_value("1");
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(
            dir.path().join("data.json"),
            r#"{"name": "nu", "stars": 3}"#,
        )
        .expect("json");
        fs::write(dir.path().join("notes.txt"), "first line\nsecond line\n").expect("text");
        let mut stack = stack();
        stack.add_env_var(
            "PWD".into(),
            Value::string(dir.path().to_string_lossy(), Span::unknown()),
        );
        let file = |name: &str| {
            Completions::new(vec![Suggestion {
                value: name.into(),
                ..Suggestion::default()
            }])
            .with_data(Details(vec![Detail {
                kind: Some(SuggestionKind::File),
                ..Detail::default()
            }]))
        };

        let json = preview(&engine, &stack, &file("data.json"), 0, 60, None)
            .rows
            .join("\n");
        for cell in ["size", "modified", "name", "nu", "stars", "3"] {
            assert!(json.contains(cell), "{cell} is missing from\n{json}");
        }
        let text = preview(&engine, &stack, &file("notes.txt"), 0, 60, None)
            .rows
            .join("\n");
        assert!(
            text.contains("size") && text.contains("second line"),
            "{text}"
        );
    }

    /// The completions of the command `name` of `engine`, as the panel gets them.
    fn command_completions(engine: &EngineState, name: &str) -> Completions {
        let decl_id = engine.find_decl(name.as_bytes(), &[]).expect("a command");
        Completions::new(vec![Suggestion {
            value: name.into(),
            description: Some(engine.get_decl(decl_id).description().to_owned()),
            ..Suggestion::default()
        }])
        .with_data(Details(vec![Detail {
            kind: Some(SuggestionKind::Command(
                engine.get_decl(decl_id).command_type(),
                Some(decl_id),
            )),
            ..Detail::default()
        }]))
    }

    #[test]
    fn a_command_previews_what_the_hint_line_does_not_say() {
        let (engine, _) = engine_and_value("1");
        let decl = engine.get_decl(engine.find_decl(b"where", &[]).expect("where"));
        let completions = command_completions(&engine, "where");

        let preview = preview(&engine, &stack(), &completions, 0, 60, None);
        let folded = preview.rows.join("\n");
        let unfolded = preview.more.join("\n");
        let everything = format!("{folded}\n{unfolded}");

        // The examples come first, code and all, and what the first gives is a table.
        let first = decl.examples().remove(0);
        assert!(folded.contains(first.description), "{folded}");
        assert!(preview.is_foldable(), "{preview:#?}");
        // The parameters are behind the fold.
        assert!(
            unfolded.contains("--help") || unfolded.contains("condition"),
            "{unfolded}"
        );
        // What the hint line has is not repeated: the usage, and the line of description.
        let hint = decl.description().lines().next().expect("a description");
        assert!(
            !everything.contains(hint),
            "repeats the hint:\n{everything}"
        );
        assert!(
            !everything.contains("where {flags}"),
            "repeats the usage:\n{everything}"
        );
        assert!(
            preview.summary.contains("more examples"),
            "{}",
            preview.summary
        );
    }

    #[test]
    fn the_examples_of_a_command_are_previewed_with_placeholders_marked() {
        let (engine, _) = engine_and_value(
            "# Switch branches.\n@example \"Create a branch\" \"git checkout -b <name>\"\nextern \"git checkout\" [\n  --branch (-b): string # The new branch.\n]",
        );
        let completions = command_completions(&engine, "git checkout");
        let mut stack = stack();
        // Without a tldr page to fall back on.
        stack.add_env_var(
            "HOME".into(),
            Value::string("/nonexistent", Span::unknown()),
        );

        let preview = preview(&engine, &stack, &completions, 0, 60, None);
        let rows = preview.rows.join("\n");
        let plain = nu_utils::strip_ansi_string_unlikely(rows.clone());

        assert!(plain.contains("Create a branch"), "{plain}");
        assert!(plain.contains("git checkout -b <name>"), "{plain}");
        assert!(
            rows.contains("[4;") || rows.contains("[4m"),
            "the placeholder is underlined:\n{rows:?}"
        );
        assert!(
            !plain.contains("Switch branches"),
            "the hint has it:\n{plain}"
        );
        // What it takes is the status line's, not the preview's.
        assert!(
            !preview.more.join("\n").contains("--branch"),
            "{:?}",
            preview.more
        );
    }

    /// The completions of one `value` offered at `slot` of the command `name`.
    fn offered_in(engine: &EngineState, name: &str, value: &str, slot: Slot) -> Completions {
        let decl_id = engine.find_decl(name.as_bytes(), &[]).expect("a command");
        let kind = if value.starts_with('-') {
            SuggestionKind::Flag
        } else {
            SuggestionKind::Value(Type::String)
        };
        Completions::new(vec![Suggestion {
            value: value.into(),
            ..Suggestion::default()
        }])
        .with_data(Details(vec![Detail {
            kind: Some(kind),
            help_at: Some(Arc::new(HelpAt {
                command: HelpFor::Decl(decl_id),
                slot,
            })),
            ..Detail::default()
        }]))
    }

    const CHECKOUT: &str = "
        @example \"Create a branch\" \"git checkout -b <branch>\"
        @example \"Create a branch from a ref\" \"git checkout --branch <branch> <ref>\"
        @example \"Go back\" \"git checkout -\"
        @example \"Restore a file\" \"git checkout -- <file>\"
        extern \"git checkout\" [
            --branch(-b): string # Create the branch.
            target?: string # What to check out.
            file?: string # A file to restore.
        ]";

    #[test]
    fn a_flag_folds_the_examples_that_use_it_under_its_own_preview() {
        let (engine, _) = engine_and_value(CHECKOUT);
        for flag in ["-b", "--branch"] {
            let completions = offered_in(&engine, "git checkout", flag, Slot::Flag);
            let help = preview(&engine, &stack(), &completions, 0, 70, None);
            let text = nu_utils::strip_ansi_string_unlikely(help.more.join("\n"));
            assert!(help.rows.is_empty(), "{help:#?}");
            // The examples that write it in either form; what it is called and takes is the list's.
            assert!(!text.contains("--branch (-b): string"), "{text}");
            assert!(text.contains("git checkout -b <branch>"), "{text}");
            assert!(
                text.contains("git checkout --branch <branch> <ref>"),
                "{text}"
            );
            assert!(!text.contains("Go back"), "{text}");
            assert_eq!(help.summary, format!("2 examples using {flag}"));
            // The hint line has the flag's description.
            assert!(!text.contains("Create the branch."), "{text}");
        }
    }

    #[test]
    fn a_value_folds_the_examples_that_give_it_one_and_leaves_what_it_is_to_the_status_line() {
        let (engine, _) = engine_and_value(CHECKOUT);
        // The first positional is given by the examples with a word that is not a flag or the
        // value of one: the ref, and the file.
        let completions = offered_in(&engine, "git checkout", "main", Slot::Positional(0));
        let help = preview(&engine, &stack(), &completions, 0, 70, None);
        let text = nu_utils::strip_ansi_string_unlikely(help.more.join("\n"));
        assert!(
            !text.contains("target: string"),
            "the status line's:\n{text}"
        );
        assert!(
            !text.contains("What to check out."),
            "the status line's:\n{text}"
        );
        assert!(
            text.contains("git checkout --branch <branch> <ref>"),
            "{text}"
        );
        assert!(text.contains("git checkout -- <file>"), "{text}");
        assert!(!text.contains("Go back"), "{text}");
        assert_eq!(help.summary, "2 examples using target");

        // None that give a second one: the examples say nothing of it.
        let completions = offered_in(&engine, "git checkout", "main", Slot::Positional(1));
        let help = preview(&engine, &stack(), &completions, 0, 70, None);
        assert!(help.more.is_empty(), "{:?}", help.more);

        // The value of a flag folds the flag's examples.
        let completions = offered_in(
            &engine,
            "git checkout",
            "x",
            Slot::FlagValue("branch".into()),
        );
        let help = preview(&engine, &stack(), &completions, 0, 70, None);
        let text = nu_utils::strip_ansi_string_unlikely(help.more.join("\n"));
        assert!(
            !text.contains("Create the branch."),
            "the status line's:\n{text}"
        );
        assert_eq!(help.summary, "2 examples using --branch");
    }

    #[test]
    fn a_completer_unfolds_help_and_the_rest_of_a_description() {
        let (engine, suggestions) = engine_and_value(
            "[{value: main, description: \"the default\\nnames the trunk\", preview: {commits: 12}, help: \"merged from topics\"}]",
        );
        let completions = answer(suggestions);

        let preview = preview(&engine, &stack(), &completions, 0, 60, None);

        assert!(preview.rows.join("\n").contains("commits"), "{preview:#?}");
        let more = preview.more.join("\n");
        assert!(more.contains("merged from topics"), "{more}");
        assert!(more.contains("names the trunk"), "{more}");
        assert!(
            !more.contains("the default"),
            "the hint line has the first line:\n{more}"
        );
    }

    #[test]
    fn a_binary_file_previews_as_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("blob");
        fs::write(&path, [0xff, 0xfe, 0x00, 0x80]).expect("file");
        assert!(file(&path, 60).is_empty());
    }
}
