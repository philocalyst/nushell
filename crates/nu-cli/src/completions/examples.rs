//! A search through the examples of every command there is, for the help menu: the examples
//! the commands of the shell declare (built in, custom and `extern`, the last by `@example`).
//! Taking one puts its code on the line, ready to have its
//! first `<placeholder>` completed.

use super::{
    placeholder,
    preview::{ROWS, draw},
};
use nu_protocol::{
    Value,
    engine::{EngineState, Stack},
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use reedline::{Completions, Preview, Suggestion};
use std::{
    collections::HashSet,
    ops::Range,
    sync::{Arc, Mutex},
};

/// Examples a search offers at most.
const SHOWN: usize = 150;

/// One example, and the command it is of.
#[derive(Debug, PartialEq)]
pub(crate) struct Entry {
    /// The command, as it is typed: `git checkout`.
    command: String,
    description: String,
    code: String,
    result: Option<Value>,
}

impl Entry {
    /// What the search matches: the description and the code.
    fn haystack(&self) -> String {
        format!("{} {}", self.description, self.code)
    }
}

/// The examples there are, built once and searched on every keystroke.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Index {
    entries: Vec<Entry>,
}

impl Index {
    /// The examples of the commands of `engine_state`, an `extern`'s among them (a tldr page
    /// becomes those with `std/completions tldr examples`). Two examples with the same code are one.
    pub(super) fn build(engine_state: &EngineState) -> Self {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |entry: Entry| {
            if seen.insert(entry.code.clone()) {
                entries.push(entry);
            }
        };
        for (name, decl_id) in engine_state.get_decls_sorted(false) {
            let command = String::from_utf8_lossy(&name).into_owned();
            for example in engine_state.get_decl(decl_id).examples() {
                push(Entry {
                    command: command.clone(),
                    description: example.description.to_owned(),
                    code: example.example.to_owned(),
                    result: example.result,
                });
            }
        }
        Self { entries }
    }

    /// The index of `engine_state`, which is built once for each state of the commands and reused
    /// by every prompt after it.
    pub(crate) fn of(engine_state: &EngineState) -> Arc<Self> {
        static BUILT: Mutex<Option<(usize, Arc<Index>)>> = Mutex::new(None);

        let decls = engine_state.num_decls();
        let mut built = BUILT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((for_decls, index)) = &*built
            && *for_decls == decls
        {
            return Arc::clone(index);
        }
        let index = Arc::new(Self::build(engine_state));
        *built = Some((decls, Arc::clone(&index)));
        index
    }

    /// The examples that answer `line` with the cursor at `cursor`, as suggestions that put
    /// their code where the command under the cursor is, best first.
    ///
    /// The command is the pipeline element the cursor is in. Examples of it come first,
    /// matched against the words that follow its name, and then the others, matched against
    /// the whole element. Both match the description and the code, fuzzily, every word.
    pub(crate) fn search(&self, line: &str, cursor: usize) -> Completions {
        let element = command_at(line, cursor);
        let text = line[element.clone()].trim();
        let words: Vec<&str> = text.split_whitespace().collect();
        let mut matcher = Matcher::new(Config::DEFAULT);
        let whole = Pattern::parse(text, CaseMatching::Smart, Normalization::Smart);

        let mut found: Vec<(usize, u32, usize)> = Vec::new();
        for (at, entry) in self.entries.iter().enumerate() {
            let named = named_words(&entry.command, &words);
            let rest = words[named..].join(" ");
            let pattern = if named > 0 {
                Pattern::parse(&rest, CaseMatching::Smart, Normalization::Smart)
            } else {
                whole.clone()
            };
            let mut buffer = Vec::new();
            let haystack = entry.haystack();
            let score = if pattern.atoms.is_empty() {
                Some(0)
            } else {
                pattern.score(Utf32Str::new(&haystack, &mut buffer), &mut matcher)
            };
            if let Some(score) = score {
                found.push((named, score, at));
            }
        }
        // The examples of more of the words come first, then the better matches, then the
        // order of the index.
        found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        found.truncate(SHOWN);

        let at = taken_span(line, cursor);
        let entries: Vec<&Entry> = found.iter().map(|&(.., at)| &self.entries[at]).collect();
        let items: Vec<Suggestion> = entries.iter().map(|entry| suggestion(entry, &at)).collect();
        let results = entries.iter().map(|entry| entry.result.clone()).collect();
        Completions::new(items).with_data(Results(results))
    }
}

/// What each suggestion of a search gives when the example is run, if that is known.
#[derive(Debug, PartialEq)]
pub(crate) struct Results(Vec<Option<Value>>);

/// How many of the leading words of `command` the `words` typed begin with, a `^` in front of
/// the first not counting: `git checkout` is `git` and `checkout`, so `git ch` has one.
fn named_words(command: &str, words: &[&str]) -> usize {
    let typed = words
        .iter()
        .map(|word| word.trim_start_matches('^'))
        .take_while(|word| !word.starts_with('-'));
    command
        .split(' ')
        .zip(typed)
        .take_while(|(name, typed)| name == typed)
        .count()
}

/// The part of `line` an example taken at `cursor` replaces: the command the cursor is in
/// without the blanks around it, or on a blank one where the cursor is.
fn taken_span(line: &str, cursor: usize) -> Range<usize> {
    let element = command_at(line, cursor);
    let text = line[element.clone()].trim();
    if text.is_empty() {
        let cursor = line.floor_char_boundary(cursor.min(line.len()));
        return cursor..cursor;
    }
    let blank = line[element.clone()].len() - line[element.clone()].trim_start().len();
    let start = element.start + blank;
    start..start + text.len()
}

/// The suggestion that puts `entry` over `at`, with the cursor on its first placeholder.
fn suggestion(entry: &Entry, at: &Range<usize>) -> Suggestion {
    Suggestion {
        description: Some(entry.description.clone()).filter(|text| !text.is_empty()),
        ..taking(&entry.code, at)
    }
}

/// The suggestion that puts `code` over `at`, with the cursor on its first placeholder.
fn taking(code: &str, at: &Range<usize>) -> Suggestion {
    let display = match code.split_once('\n') {
        Some((first, _)) => format!("{first} …"),
        None => code.to_owned(),
    };
    Suggestion {
        value: code.to_owned(),
        display_override: Some(display),
        span: reedline::Span::new(at.start, at.end),
        cursor: placeholder::find(code)
            .first()
            .map(|first| at.start + first.end),
        ..Suggestion::default()
    }
}

/// The suggestion that takes the example `code` as the help menu does, over the command at
/// `cursor` in `line`.
pub(crate) fn take(code: &str, line: &str, cursor: usize) -> Suggestion {
    taking(code, &taken_span(line, cursor))
}

/// [`take`] for an example of the command `head`: when something is piped into the command at
/// `cursor` on the line, the example from `head` on, so that what the line pipes in stays and
/// the example's own input is not put in its place.
pub(crate) fn take_into(code: &str, head: Option<&str>, line: &str, cursor: usize) -> Suggestion {
    let at = taken_span(line, cursor);
    let piped = line[..at.start].trim_end().ends_with('|');
    let code = match head.filter(|_| piped) {
        Some(head) => split_input(code, head).map_or(code, |(_, rest)| rest),
        None => code,
    };
    taking(code, &at)
}

/// The example `code` of the command `head` split where `head` is piped into on its first line:
/// what it pipes in, and the rest from `head` on.
pub(crate) fn split_input<'a>(code: &'a str, head: &str) -> Option<(&'a str, &'a str)> {
    let first = code.lines().next()?;
    let (at, _) = first.match_indices('|').find(|(at, _)| {
        let after = first[at + 1..].trim_start();
        after
            .strip_prefix(head)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })?;
    let input = first[..at].trim();
    (!input.is_empty()).then(|| (input, code[at + 1..].trim_start()))
}

/// The rows of the result of the example `index` of `completions`, a search's answer.
pub(crate) fn preview(
    engine_state: &EngineState,
    stack: &Stack,
    completions: &Completions,
    index: usize,
    width: u16,
) -> Preview {
    let Some(Some(result)) = completions.data::<Results>().and_then(|r| r.0.get(index)) else {
        return Preview::default();
    };
    let width = usize::from(width.saturating_sub(4)).max(8);
    draw(engine_state, stack, result.clone(), width, ROWS)
        .map(Preview::from)
        .unwrap_or_default()
}

/// The pipeline element the cursor is in, as a byte range of `line`: between the `|`, `;` and
/// newlines outside quotes that are nearest it, within the brackets that hold it.
pub(crate) fn command_at(line: &str, cursor: usize) -> Range<usize> {
    let cursor = line.floor_char_boundary(cursor.min(line.len()));
    // Where the element starts at each level of brackets the cursor is in.
    let mut starts = vec![0];
    let mut quote = None;
    for (at, c) in line[..cursor].char_indices() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => quote = Some(c),
            (None, '(' | '{' | '[') => starts.push(at + 1),
            (None, ')' | '}' | ']') if starts.len() > 1 => {
                starts.pop();
            }
            (None, '|' | ';' | '\n') => {
                if let Some(start) = starts.last_mut() {
                    *start = at + 1;
                }
            }
            (None, _) => {}
        }
    }
    let mut depth = 0usize;
    let mut end = line.len();
    for (at, c) in line[cursor..].char_indices() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => quote = Some(c),
            (None, '(' | '{' | '[') => depth += 1,
            (None, ')' | '}' | ']') if depth > 0 => depth -= 1,
            (None, ')' | '}' | ']' | '|' | ';' | '\n') if depth == 0 => {
                end = cursor + at;
                break;
            }
            (None, _) => {}
        }
    }
    starts.last().copied().unwrap_or(0)..end
}

#[cfg(test)]
mod tests {
    use super::*;
    use nu_protocol::engine::StateWorkingSet;

    #[test]
    fn an_example_taken_where_something_is_piped_in_keeps_what_is() {
        let code = "[{a: 1} {a: 2}] | where a > 1";
        let line = "ls | whe";
        let taken = take_into(code, Some("where"), line, line.len());
        assert_eq!(taken.value, "where a > 1");
        assert_eq!((taken.span.start, taken.span.end), (5, 8));
        // With nothing piped in, the example's own input comes along.
        let taken = take_into(code, Some("where"), "whe", 3);
        assert_eq!(taken.value, code);
        assert_eq!(split_input("ls | where-not x", "where"), None);
    }

    /// An engine with the commands of the shell and `source` defined in it.
    fn engine_with(source: &str) -> EngineState {
        let mut engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        let mut working_set = StateWorkingSet::new(&engine_state);
        nu_parser::parse(&mut working_set, None, source.as_bytes(), false);
        assert!(
            working_set.parse_errors.is_empty(),
            "{:?}",
            working_set.parse_errors
        );
        let delta = working_set.render();
        engine_state
            .merge_delta(delta)
            .expect("the definitions merge");
        engine_state
    }

    const SOURCE: &str = r#"
        @example "Switch to a new branch" "git checkout -b <branch>"
        @example "Go back to the last branch" "git checkout -"
        export extern "git checkout" [--branch(-b): string]
        @example "Delete a local branch" "git branch -d <name>"
        export extern "git branch" [--delete(-d): string]
        @example "Extract an archive" "tar -x -f <archive>"
        export extern "tar" []
    "#;

    /// The values and descriptions a search offers, in order.
    fn found(index: &Index, line: &str, cursor: usize) -> Vec<(String, String)> {
        let completions = index.search(line, cursor);
        completions
            .items
            .iter()
            .map(|item| {
                (
                    item.value.clone(),
                    item.description.clone().unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn examples_are_found_by_what_they_do_or_what_they_say() {
        let engine_state = engine_with(SOURCE);
        let index = Index::build(&engine_state);

        let by_description = found(&index, "delete branch", 13);
        assert_eq!(by_description[0].0, "git branch -d <name>");
        // Words match in any order, and the code counts.
        let by_code = found(&index, "archive tar", 11);
        assert_eq!(by_code[0].0, "tar -x -f <archive>");
        assert_eq!(by_code[0].1, "Extract an archive");
    }

    #[test]
    fn the_examples_of_the_command_typed_come_first_and_the_rest_follow() {
        let engine_state = engine_with(SOURCE);
        let index = Index::build(&engine_state);

        let codes = |line: &str| -> Vec<String> {
            found(&index, line, line.len())
                .into_iter()
                .map(|(code, _)| code)
                .collect()
        };
        // Its own examples first. Of the others, those that match all the words: `git`, and
        // then the other subcommands.
        let checkout = codes("git checkout");
        assert_eq!(
            &checkout[..2],
            ["git checkout -b <branch>", "git checkout -"]
        );
        assert!(!checkout.contains(&"git branch -d <name>".to_owned()));
        let git = codes("git");
        assert!(git.contains(&"git checkout -".to_owned()));
        assert!(git.contains(&"git branch -d <name>".to_owned()));
        // The words after the name narrow its own examples.
        assert_eq!(codes("git checkout -b")[0], "git checkout -b <branch>");
    }

    #[test]
    fn taking_an_example_replaces_the_command_and_readies_its_first_placeholder() {
        let engine_state = engine_with(SOURCE);
        let index = Index::build(&engine_state);
        let line = "ls | git checkout | length";
        let completions = index.search(line, 12);
        let item = &completions.items[0];
        assert_eq!(item.value, "git checkout -b <branch>");
        // The command, without the blanks around it.
        assert_eq!((item.span.start, item.span.end), (5, 17));
        let mut taken = line.to_owned();
        taken.replace_range(item.span.start..item.span.end, &item.value);
        assert_eq!(taken, "ls | git checkout -b <branch> | length");
        let cursor = item.cursor.expect("a cursor");
        assert_eq!(&taken[..cursor], "ls | git checkout -b <branch>");

        // On an empty line, it goes at the cursor.
        let empty = index.search("", 0);
        assert_eq!((empty.items[0].span.start, empty.items[0].span.end), (0, 0));
        // An example with nothing to fill leaves the cursor at its end.
        let plain = index.search("git checkout -", 14);
        let back = plain
            .items
            .iter()
            .find(|item| item.value == "git checkout -")
            .expect("it");
        assert_eq!(back.cursor, None);
    }

    #[test]
    fn the_command_under_the_cursor_is_the_pipeline_element_it_is_in() {
        let at = |line: &str, cursor| {
            let range = command_at(line, cursor);
            line[range].to_owned()
        };
        assert_eq!(at("ls | git ch | length", 10), " git ch ");
        assert_eq!(at("ls; git ch", 10), " git ch");
        assert_eq!(at("each { git ch } | wc", 12), " git ch ");
        assert_eq!(at("echo \"a | b\" | git", 18), " git");
        assert_eq!(
            at("git commit -m (date now)", 10),
            "git commit -m (date now)"
        );
        assert_eq!(at("", 0), "");
    }
}
