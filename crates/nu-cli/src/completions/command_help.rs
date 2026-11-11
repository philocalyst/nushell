//! The preview of a command: only what the hint line above the panel does not already say, which
//! is its usage and the first line of what it is for. That leaves, in order, its examples (with
//! the result as a table where one is known), the types it takes and returns, the rest of its
//! description, and its parameters. The first examples and the types show folded; the rest shows
//! once the preview is unfolded.
//!
//! The help for a place in a command, [`at`], is what a flag or an argument folds under its own
//! preview: the examples of the command that use it.

use super::{
    HelpAt, HelpFor, Slot, placeholder,
    preview::{draw, quiet},
};
use crate::syntax_highlight::highlight_syntax;
use nu_ansi_term::Style;
use nu_protocol::{
    DeclId, Flag, ShellError, Signature, Type, Value,
    engine::{EngineState, Stack, StateWorkingSet},
};
use nu_table::string_wrap;
use reedline::{Preview, StyledText};
use std::ops::Range;

/// Rows the folded part of a preview takes.
const FOLDED_ROWS: usize = 12;
/// Rows the whole of a preview takes at most.
const UNFOLDED_ROWS: usize = 80;
/// Rows the result of an example takes at most.
const RESULT_ROWS: usize = 6;
/// Rows of the rest of the description at most.
const PROSE_ROWS: usize = 12;
/// Examples the help for a place in a command shows at most.
const COMPOSED_EXAMPLES: usize = 6;
/// Examples that show folded at most.
const FOLDED_EXAMPLES: usize = 2;

/// One example: what it does, the code, and what it gives if that is known.
struct Sample {
    description: String,
    code: String,
    result: Option<Value>,
    /// The flags and parameters it says it is about, if it says.
    about: Vec<String>,
}

/// What there is to say of a command, before it is laid out.
#[derive(Default)]
struct Help {
    /// What the command is, for a command whose page is all there is.
    blurb: String,
    examples: Vec<Sample>,
    /// The description past its first line, and the extra description.
    prose: String,
}

/// How to draw: where, how wide, and the quiet style of the hint line.
struct Canvas<'a> {
    engine_state: &'a EngineState,
    stack: &'a Stack,
    width: usize,
    dim: Style,
}

/// What the line says of a command whose examples are shown, which ranks them.
#[derive(Debug, Default)]
pub(super) struct Typed<'a> {
    /// The text of the pipeline piped into it.
    pub(super) input: Option<&'a str>,
    /// The words written after it.
    pub(super) args: Vec<&'a str>,
}

/// The preview of the command `decl_id`, its rows at most `width` columns wide, its examples
/// the most like what is `typed` of it first.
pub(super) fn command(
    engine_state: &EngineState,
    stack: &Stack,
    decl_id: DeclId,
    width: usize,
    typed: &Typed,
) -> Result<(Preview, RowMap), ShellError> {
    // What it takes is the status line's, which shows its usage and the parameter being typed;
    // a preview shows it at work.
    let decl = engine_state.get_decl(decl_id);
    let mut examples = samples(engine_state, decl_id);
    rank(engine_state, &mut examples, decl.name(), typed);
    let help = Help {
        examples,
        prose: prose(decl.description(), decl.extra_description()),
        ..Help::default()
    };
    help.layout(&canvas(engine_state, stack, width))
}

/// Orders `samples` of the command `name` by how like what is `typed` they are, keeping the
/// order they were given in between equals: an example that writes the flags and the words
/// written (`name` and `=~` for `where name =~ x`) comes first, then one that takes what is piped
/// in, as rows, a list, a record or text, or takes nothing when nothing is.
fn rank<S: std::borrow::Borrow<Sample>>(
    engine_state: &EngineState,
    samples: &mut [S],
    name: &str,
    typed: &Typed,
) {
    let name: Vec<&str> = name.split(' ').collect();
    let (flags, words): (Vec<&str>, Vec<&str>) = (typed.args.iter().copied())
        .filter(|arg| *arg != "-")
        .partition(|arg| arg.starts_with('-'));
    let piped = typed.input.map(|input| shape_of(engine_state, input));
    let score = |sample: &S| {
        let sample = sample.borrow();
        let args = arguments(&sample.code, &name);
        let written = (flags.iter())
            .filter(|flag| args.iter().any(|arg| writes_flag(arg, flag)))
            .count()
            + (words.iter()).filter(|word| args.contains(word)).count();
        let input = super::examples::split_input(&sample.code, name[0]).map(|(input, _)| input);
        let fits = match (&piped, input) {
            (Some(piped), Some(input)) => {
                1 + usize::from(piped.is_some() && *piped == shape_of(engine_state, input))
            }
            (None, None) => 1,
            _ => 0,
        };
        2 * written + fits
    };
    samples.sort_by_cached_key(|sample| std::cmp::Reverse(score(sample)));
}

/// The kind of value the pipeline `text` gives, as far as its parse says: rows, a list, a record,
/// text or a number. `None` for anything.
fn shape_of(engine_state: &EngineState, text: &str) -> Option<&'static str> {
    let mut working_set = StateWorkingSet::new(engine_state);
    let block = nu_parser::parse(&mut working_set, None, text.as_bytes(), false);
    Some(match block.output_type() {
        Type::Table(_) => "rows",
        Type::List(inner) if matches!(*inner, Type::Record(_) | Type::Table(_)) => "rows",
        Type::List(_) => "list",
        Type::Record(_) => "record",
        Type::String | Type::Glob => "text",
        Type::Int | Type::Float | Type::Number | Type::Filesize | Type::Duration => "number",
        _ => return None,
    })
}

/// The examples of the command `decl_id`.
fn samples(engine_state: &EngineState, decl_id: DeclId) -> Vec<Sample> {
    let decl = engine_state.get_decl(decl_id);
    let mut about = decl.examples_about().into_iter();
    decl.examples()
        .into_iter()
        .map(|example| Sample {
            about: about.next().unwrap_or_default(),
            // The hint line has the first line of what the command is for.
            description: if same_words(example.description, decl.description()) {
                String::new()
            } else {
                example.description.to_owned()
            },
            code: example.example.to_owned(),
            result: example.result,
        })
        .collect()
}

/// The help for a place in a command, to fold under the preview of what is offered there.
pub(super) struct Composed {
    pub(super) rows: Vec<String>,
    /// What the rows are, in a few words: `3 examples using -b`.
    pub(super) summary: String,
    /// Which of the rows hold an example, counted from the first of `rows`.
    pub(super) map: RowMap,
}

/// What a place in a command is, so far as the examples can tell.
enum Subject {
    /// A flag, by the forms it is written in, the one to call it by first.
    Flag(Vec<String>),
    /// The positional with this index and name.
    Positional(usize, String),
    /// Nothing that tells the examples apart.
    Command,
}

/// The help for the place `value` is offered at in the command of `at`: what the flag or the
/// parameter is, and the examples of the command that use it. For a parameter that no example
/// uses, all of the command's examples. `None` when there is nothing to say.
pub(super) fn at(
    engine_state: &EngineState,
    stack: &Stack,
    at: &HelpAt,
    value: &str,
    width: usize,
    alone: bool,
    typed: &Typed,
) -> Result<Option<Composed>, ShellError> {
    let (samples, signature, command) = match &at.command {
        HelpFor::Decl(decl_id) => {
            let decl = engine_state.get_decl(*decl_id);
            let signature = decl.signature();
            (
                samples(engine_state, *decl_id),
                Some(signature),
                decl.name().to_owned(),
            )
        }
        HelpFor::External(name) => (Vec::new(), None, name.clone()),
    };
    let flag = |name: &str| signature.as_ref().and_then(|s| flag_named(s, name));

    // What the place is. What a flag is called and takes, the list says beside it, and what a
    // place is for, the hint and the status line: the help here is the examples.
    let subject = match &at.slot {
        Slot::Flag | Slot::Argument if value.starts_with('-') => match flag(value) {
            Some(found) => Subject::Flag(flag_forms(found, value)),
            None => Subject::Flag(vec![value.to_owned()]),
        },
        Slot::FlagValue(name) => match flag(name) {
            Some(found) => Subject::Flag(flag_forms(found, name)),
            None => Subject::Command,
        },
        Slot::Positional(index) => {
            match signature.as_ref().and_then(|s| s.get_positional(*index)) {
                Some(arg) => Subject::Positional(*index, arg.name.clone()),
                None => Subject::Command,
            }
        }
        Slot::Flag | Slot::Argument => Subject::Command,
    };

    let name: Vec<&str> = command.split(' ').collect();
    let takes_value = |arg: &str| flag(arg).is_some_and(|flag| flag.arg.is_some());
    let uses = |sample: &Sample| {
        // An example that says what it is about is about only that.
        if !sample.about.is_empty() {
            return match &subject {
                Subject::Flag(forms) => (sample.about.iter())
                    .any(|about| forms.iter().any(|form| flag_named_by(about, form))),
                Subject::Positional(_, name) => sample.about.iter().any(|about| about == name),
                Subject::Command => true,
            };
        }
        let args = arguments(&sample.code, &name);
        match &subject {
            Subject::Flag(forms) => args
                .iter()
                .any(|arg| forms.iter().any(|form| writes_flag(arg, form))),
            Subject::Positional(index, _) => positionals(&args, takes_value) > *index,
            Subject::Command => true,
        }
    };
    let using: Vec<&Sample> = samples.iter().filter(|sample| uses(sample)).collect();
    // Examples that every example would be, as those of a value every one of them takes (an ssh
    // host), say nothing of the value selected beside a preview of its own. Alone, as a column
    // in `where`'s condition is, the command's examples are the help there is.
    let (mut shown, label) = match &subject {
        Subject::Flag(forms) => (using, Some(forms[0].as_str())),
        Subject::Positional(_, name) if using.len() < samples.len() => (using, Some(name.as_str())),
        _ if alone => (samples.iter().collect(), None),
        _ => (Vec::new(), None),
    };
    rank(engine_state, &mut shown, &command, typed);

    let canvas = canvas(engine_state, stack, width);
    let mut rows = Vec::new();
    let mut map = RowMap::new();
    for sample in shown.iter().take(COMPOSED_EXAMPLES) {
        let block = canvas.example(sample)?;
        let len = block.len();
        let start = join(&mut rows, block);
        map.push((start..start + len, sample.code.clone()));
    }
    if shown.len() > COMPOSED_EXAMPLES {
        let more = format!("+{} more", shown.len() - COMPOSED_EXAMPLES);
        rows.push(canvas.dim.paint(more).to_string());
    }
    if rows.is_empty() {
        return Ok(None);
    }
    let count = shown.len();
    let noun = if count == 1 { "example" } else { "examples" };
    let summary = match label {
        _ if count == 0 => String::new(),
        Some(label) => format!("{count} {noun} using {label}"),
        None => format!("{count} {noun}"),
    };
    Ok(Some(Composed { rows, summary, map }))
}

/// Whether `about`, as an example names what it is about, is the flag written `form`: `--branch`
/// and `-b` name the flag whose forms they are.
fn flag_named_by(about: &str, form: &str) -> bool {
    about == form || about.trim_start_matches('-') == form.trim_start_matches('-')
}

/// The flag of `signature` that `name` spells: `--branch`, `-b` or `branch`.
fn flag_named<'a>(signature: &'a Signature, name: &str) -> Option<&'a Flag> {
    let name = name.trim_start_matches('-');
    signature.named.iter().find(|flag| {
        flag.long_name() == Some(name)
            || (name.chars().count() == 1
                && flag.short.is_some_and(|short| name.starts_with(short)))
    })
}

/// The ways `flag` is written, `first` being the one the caller knows it by.
fn flag_forms(flag: &Flag, first: &str) -> Vec<String> {
    let first = if first.starts_with('-') {
        first.to_owned()
    } else if first.chars().count() == 1 {
        format!("-{first}")
    } else {
        format!("--{first}")
    };
    let mut forms = vec![first];
    let long = flag.long_name().map(|long| format!("--{long}"));
    let short = flag.short.map(|short| format!("-{short}"));
    for form in long.into_iter().chain(short) {
        if !forms.contains(&form) {
            forms.push(form);
        }
    }
    forms
}

/// Whether the word `arg` of an example writes the flag in the form `form`: `--branch`,
/// `--branch=x`, `-b`, or `-b` among the letters of `-xbf`.
fn writes_flag(arg: &str, form: &str) -> bool {
    match form.strip_prefix("--") {
        Some(long) => arg
            .strip_prefix("--")
            .is_some_and(|rest| rest.split('=').next() == Some(long)),
        None => {
            let short = form.trim_start_matches('-');
            arg.strip_prefix('-').is_some_and(|rest| {
                !rest.starts_with('-')
                    && rest
                        .split('=')
                        .next()
                        .is_some_and(|letters| letters.contains(short))
            })
        }
    }
}

/// The words of the lines of `code` that follow the command `name`, each line's up to the end
/// of its pipeline element.
fn arguments<'a>(code: &'a str, name: &[&str]) -> Vec<&'a str> {
    let mut found = Vec::new();
    for line in code.lines() {
        let words = words(line);
        let at = words.windows(name.len()).position(|window| {
            window
                .iter()
                .zip(name)
                .all(|(word, name)| word.trim_start_matches('^') == *name)
        });
        if let Some(at) = at {
            found.extend(
                words[at + name.len()..]
                    .iter()
                    .copied()
                    .take_while(|word| !matches!(*word, "|" | ";")),
            );
        }
    }
    found
}

/// The words after the command `name` in `element`, a pipeline element of the line.
pub(super) fn args_of<'a>(element: &'a str, name: &str) -> Vec<&'a str> {
    arguments(element, &name.split(' ').collect::<Vec<_>>())
}

/// The words of `line`, a quoted one whole.
fn words(line: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let (mut start, mut quote) = (None, None);
    for (at, c) in line.char_indices() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => {
                quote = Some(c);
                start.get_or_insert(at);
            }
            (None, c) if c.is_whitespace() => {
                if let Some(from) = start.take() {
                    found.push(&line[from..at]);
                }
            }
            (None, _) => {
                start.get_or_insert(at);
            }
        }
    }
    if let Some(from) = start {
        found.push(&line[from..]);
    }
    found
}

/// How many of `args` are positionals, the value of a flag that `takes_value` not counting.
fn positionals(args: &[&str], takes_value: impl Fn(&str) -> bool) -> usize {
    let (mut count, mut value_next) = (0, false);
    for arg in args {
        if std::mem::take(&mut value_next) {
            continue;
        }
        if arg.starts_with('-') {
            value_next = !arg.contains('=') && takes_value(arg);
        } else {
            count += 1;
        }
    }
    count
}

fn canvas<'a>(engine_state: &'a EngineState, stack: &'a Stack, width: usize) -> Canvas<'a> {
    Canvas {
        engine_state,
        stack,
        width,
        dim: quiet(engine_state, stack),
    }
}

/// Whether `example` says what the first line of `description` does, give or take a final
/// period and capitals.
fn same_words(example: &str, description: &str) -> bool {
    let words = |text: &str| text.trim().trim_end_matches('.').to_lowercase();
    let first = description.lines().next().unwrap_or_default();
    !example.trim().is_empty() && words(example) == words(first)
}

/// The description past its first line, which the hint line has, then the extra description.
fn prose(description: &str, extra: &str) -> String {
    let rest = description.lines().skip(1).collect::<Vec<_>>().join("\n");
    [rest.trim(), extra.trim()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl Help {
    /// The preview: the first examples folded, the rest and the prose behind them, and the rows
    /// each example takes.
    fn layout(self, canvas: &Canvas) -> Result<(Preview, RowMap), ShellError> {
        let mut rows: Vec<String> = Vec::new();
        let mut more: Vec<String> = Vec::new();
        // Where each example block starts, how long it is, and its code, in `rows` and `more`.
        let mut in_rows: Vec<(usize, usize, String)> = Vec::new();
        let mut in_more: Vec<(usize, usize, String)> = Vec::new();
        if !self.blurb.is_empty() {
            rows.extend(canvas.wrapped(&self.blurb, Some(canvas.dim)));
        }

        let mut hidden = 0;
        let mut folded_away: Vec<(Vec<String>, String)> = Vec::new();
        for (index, sample) in self.examples.iter().enumerate() {
            let block = canvas.example(sample)?;
            let room = rows.len() + usize::from(!rows.is_empty()) + block.len() <= FOLDED_ROWS;
            if index < FOLDED_EXAMPLES && folded_away.is_empty() && (index == 0 || room) {
                let len = block.len();
                in_rows.push((join(&mut rows, block), len, sample.code.clone()));
            } else {
                hidden += 1;
                folded_away.push((block, sample.code.clone()));
            }
        }

        for (block, code) in folded_away {
            let len = block.len();
            in_more.push((join(&mut more, block), len, code));
        }
        // With no examples, what it does is all there is to show.
        if rows.is_empty() {
            join(&mut rows, canvas.prose(&self.prose));
        } else {
            join(&mut more, canvas.prose(&self.prose));
        }

        rows.truncate(FOLDED_ROWS + 2);
        // The fold falls between sections.
        let blank = !rows.is_empty() && !more.is_empty();
        if blank {
            more.insert(0, String::new());
        }
        more.truncate(UNFOLDED_ROWS.saturating_sub(rows.len()));
        let summary = summary(hidden);
        let map = in_rows
            .into_iter()
            .filter_map(|block| placed(block, rows.len(), 0))
            .chain(in_more.into_iter().filter_map(|(start, len, code)| {
                placed(
                    (start + usize::from(blank), len, code),
                    more.len(),
                    rows.len(),
                )
            }))
            .collect();
        Ok((
            Preview {
                rows,
                more,
                summary,
                ..Preview::default()
            },
            map,
        ))
    }
}

/// `block` after `rows`, a blank row between them. Returns the index of the block's first row.
fn join(rows: &mut Vec<String>, block: Vec<String>) -> usize {
    if !block.is_empty() && !rows.is_empty() {
        rows.push(String::new());
    }
    let start = rows.len();
    rows.extend(block);
    start
}

/// The rows of a preview that hold an example, and the code of the example: a click on one
/// takes it. Rows are counted as reedline counts them for a click, those of `Preview::rows`
/// and then those of `Preview::more`.
pub(super) type RowMap = Vec<(Range<usize>, String)>;

/// An example block of `len` rows that starts at `start` in a part of a preview `limit` rows
/// long, which starts at `offset` among all rows. `None` when truncation cut all of it away.
fn placed(
    (start, len, code): (usize, usize, String),
    limit: usize,
    offset: usize,
) -> Option<(Range<usize>, String)> {
    (start < limit).then(|| (offset + start..offset + (start + len).min(limit), code))
}

/// What is behind the fold, in a few words: `4 more examples`.
fn summary(hidden: usize) -> String {
    match hidden {
        0 => String::new(),
        1 => "1 more example".to_owned(),
        hidden => format!("{hidden} more examples"),
    }
}

impl Canvas<'_> {
    /// `text` wrapped to the width, one row for each line, in `style`.
    fn wrapped(&self, text: &str, style: Option<Style>) -> Vec<String> {
        text.lines()
            .flat_map(|line| {
                string_wrap(line.trim_end(), self.width, true)
                    .lines()
                    .map(|row| match style {
                        Some(style) if !row.is_empty() => style.paint(row).to_string(),
                        _ => row.to_owned(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// An example: what it does, dim, then its code in the colors of the prompt, then what it
    /// gives as a table.
    fn example(&self, sample: &Sample) -> Result<Vec<String>, ShellError> {
        let mut rows = self.wrapped(&sample.description, Some(self.dim));
        for line in sample.code.lines() {
            rows.extend(self.code(line));
        }
        if let Some(result) = &sample.result {
            let indent = "  ";
            let drawn = draw(
                self.engine_state,
                self.stack,
                result.clone(),
                self.width.saturating_sub(indent.len()),
                RESULT_ROWS,
            )?;
            rows.extend(drawn.into_iter().map(|row| format!("{indent}{row}")));
        }
        Ok(rows)
    }

    /// A line of code highlighted as the prompt would, wrapped under itself. A `<placeholder>`
    /// is a word to be replaced, so it is underlined, in the style of the argument it is in.
    fn code(&self, line: &str) -> Vec<String> {
        let mut text = highlight_syntax(self.engine_state, self.stack, line, 0).text;
        for found in placeholder::find(line) {
            let style = style_at(&text, found.start).underline();
            text.style_range(found.start, found.end, style);
        }
        string_wrap(&text.render_simple(), self.width, true)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The rest of the description, wrapped, cut at a dozen rows.
    fn prose(&self, prose: &str) -> Vec<String> {
        let mut rows = self.wrapped(prose, None);
        if rows.len() > PROSE_ROWS {
            rows.truncate(PROSE_ROWS - 1);
            rows.push(self.dim.paint("…").to_string());
        }
        rows
    }
}

/// The style of the text at byte `at`.
fn style_at(text: &StyledText, at: usize) -> Style {
    let mut end = 0;
    for (style, piece) in &text.buffer {
        end += piece.len();
        if at < end {
            return *style;
        }
    }
    Style::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn examples_like_what_is_typed_come_first() {
        let engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        let sample = |code: &str| Sample {
            description: String::new(),
            code: code.to_owned(),
            result: None,
            about: Vec::new(),
        };
        let codes = |samples: &[Sample]| -> Vec<String> {
            samples.iter().map(|sample| sample.code.clone()).collect()
        };
        let all = || {
            vec![
                sample("'a b' | split row ' '"),
                sample("[[a]; [1]] | sort-by a"),
                sample("[[a]; [1]] | sort-by a --reverse"),
            ]
        };
        // Rows piped in rank the examples that take rows first; a flag written, those that write it.
        let mut samples = all();
        let typed = Typed {
            input: Some("ls"),
            args: Vec::new(),
        };
        rank(&engine_state, &mut samples, "sort-by", &typed);
        assert_eq!(codes(&samples)[2], "'a b' | split row ' '");
        let mut samples = all();
        let typed = Typed {
            input: Some("ls"),
            args: vec!["--reverse"],
        };
        rank(&engine_state, &mut samples, "sort-by", &typed);
        assert_eq!(codes(&samples)[0], "[[a]; [1]] | sort-by a --reverse");

        // The words written rank them too: the column and the operator of a condition.
        let mut samples = vec![
            sample("[{a: 1}] | where a > 1"),
            sample("ls | where type == file"),
            sample("ls | where name =~ 'Car'"),
        ];
        let typed = Typed {
            input: Some("ls"),
            args: vec!["name", "=~", "'.*_test'"],
        };
        rank(&engine_state, &mut samples, "where", &typed);
        assert_eq!(codes(&samples)[0], "ls | where name =~ 'Car'");
    }

    #[test]
    fn an_example_that_repeats_the_hint_line_is_not_said_twice() {
        assert!(same_words(
            "Filter rows of a table.",
            "Filter rows of a table.\nMore."
        ));
        assert!(same_words(
            "filter rows of a table",
            "Filter rows of a table."
        ));
        assert!(!same_words("Filter rows", "Filter rows of a table."));
        assert!(!same_words("", ""));
    }

    #[test]
    fn the_prose_is_what_the_hint_line_leaves() {
        assert_eq!(prose("First.", ""), "");
        assert_eq!(prose("First.\nSecond.", "More."), "Second.\n\nMore.");
    }

    #[test]
    fn what_is_behind_the_fold_is_counted() {
        assert_eq!(summary(4), "4 more examples");
        assert_eq!(summary(1), "1 more example");
        assert_eq!(summary(0), "");
    }
}
