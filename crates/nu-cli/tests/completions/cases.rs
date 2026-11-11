//! The completion suite's table-like cases, as data: each file in `cases/` is a NUON record of
//! what its cases share and the `cases` themselves, and this runner completes each one through the
//! line editor's completer and checks what it offers.
//!
//! A file's record may hold:
//! - `fixture`: the directory of `tests/fixtures` the cases run in, as the cwd. Without one they
//!   run in a fresh directory holding the `world` entries (a name ending in `/` is a directory).
//!   A case may name its own `fixture`, `world` and `programs`.
//! - `programs`: empty executables that are the only programs on the `PATH`.
//! - `prelude`: Nushell source run before every case, to define commands or set the config.
//! - `paths`: whether the line and the values are paths, written with `/` for this platform's
//!   separator (a case may say so too).
//!
//! A case holds the `line` (or `lines` that all expect the same, or `line_from`, Nushell source
//! that makes it), the cursor `at` (a byte offset or a list of them, the end of the line unless
//! given), the `test` it was written as, its own `setup` source run after the prelude (`std: true`:
//! after loading the standard library), a `history` the completer learns from (lines, or records of
//! a `line`, the `cwd` it ran in, the seconds `ago` and the `exit` status; or `history_from`,
//! source that makes it), a line the completer completes `before` it, the `os` family (or the `os`,
//! such as `linux`) it is for, `ignore` (why it is skipped), and any of these checks:
//! - `hint`: the history hint for the line instead (`answered: false`: before the completer has
//!   answered for it);
//! - `values`: what is offered, all of it, in order (or `values_from`, source that makes them);
//!   `set`: the same, in any order; `count`, `at_least` and `at_most`: how many;
//! - `has`, `lacks` and `within`: values that are offered, that are not, and all there may be;
//!   `as_paths` and `no_paths`: values that are and are not offered as a path is, undescribed;
//! - `first`: the value offered first; `order`: values that are offered in this order;
//! - `same_as`: a line that is offered the same values;
//! - `descriptions`: the descriptions, in order; `types`: the same, cut at ` · `; `displays`: what
//!   is shown of each;
//! - `spans`: the `[start, end]` each suggestion replaces, in order;
//! - `taken`: the line once the first suggestion is taken, as Tab takes a lone one, or a record of
//!   the line each suggestion it names by what is shown leaves (`⌶` marks the cursor it places);
//! - `replaces`: the text every suggestion replaces (and that there are some); `last_span`: what
//!   the last one replaces; `indices`: the characters of each that matched what was typed;
//! - `interactive`: whether the completer runs on the line editor's thread (`@interactive`);
//! - `opens`: what the line can only go on with after the space just typed (`{  }` for `each`,
//!   `= ` for `let x`), `null` for nothing;
//! - `setup_error`: what the setup fails with, instead of completing;
//! - `ends_at_cursor`: that nothing is replaced past the cursor;
//! - `message`: why nothing is offered, as the hint line says (`null`: nothing is said), and
//!   `message_has`: what it says among the rest;
//! - `traced` and `traced_is`: a pipeline over what `debug complete` reports, and what it gives;
//! - `evaluated`: the outcomes of running the pipeline before the cursor, as `debug complete`
//!   traces them, and `reason`: what the first of them says.
//!
//! `{cwd}`, `{pid}`, `{exe}`, `{fixtures}`, `{user}` and `{group}` in a line or an expected value
//! stand for the directory the case runs in (`{cwd/}`: written with `/`), this process, the suffix
//! of programs, `tests/fixtures`, and the user running it and their primary group. A case with
//! `cases` is that many cases, each over what it says. Run one file with
//! `cargo test -p nu-cli --test tests -- completions::cases::<file>`.

use super::harness::{World, eval_source};
use nu_ansi_term::Style;
use nu_cli::{LatestAnswer, NuCompleter, NuHinter};
use nu_protocol::{
    Value,
    engine::{EngineState, Stack},
};
use nu_test_support::prelude::*;
use reedline::{
    CommandLineSearch, Hinter, History, HistoryItem, HistoryItemId, HistorySessionId,
    SearchDirection, SearchQuery, Suggestion,
};
use std::cmp::Ordering::{Greater, Less};
use std::env::consts::{EXE_SUFFIX, FAMILY, OS};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{path::MAIN_SEPARATOR_STR, sync::Arc};

/// A `#[test]` per case file, named after it.
macro_rules! case_files {
    ($($file:ident),* $(,)?) => {$(
        #[test]
        fn $file() {
            run_file(stringify!($file));
        }
    )*};
}

case_files!(
    answers,
    closures,
    custom,
    delegation,
    externals,
    files,
    hints,
    modules,
    paths,
    pipelines,
    regressions,
    system,
    taking,
    usage,
    values,
);

/// Runs every case of `cases/<name>.nuon` and fails with all the cases that do not hold.
fn run_file(name: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/completions/cases")
        .join(format!("{name}.nuon"));
    let text = std::fs::read_to_string(&path).expect("the case file is readable");
    let suite = nuon::from_nuon(&text, None).expect("the case file is NUON");
    let cases = field(&suite, "cases").expect("the case file has `cases`");
    let failures: Vec<String> = (cases.as_list().expect("`cases` is a list").iter())
        .flat_map(
            |case| match field(case, "cases").and_then(|cases| cases.as_list().ok()) {
                // Cases that share what the case around them says, each over it.
                Some(inner) => (inner.iter())
                    .map(|inner| {
                        let mut merged = case.as_record().cloned().unwrap_or_default();
                        merged.remove("cases");
                        merged.extend(inner.as_record().cloned().unwrap_or_default());
                        Value::test_record(merged)
                    })
                    .collect(),
                None => vec![case.clone()],
            },
        )
        .filter_map(|case| {
            let why = run_case(&suite, &case).err()?;
            let test = text_of(&case, "test").unwrap_or_default();
            let line = text_of(&case, "line").unwrap_or_default();
            Some(format!("{test} ({line:?}): {why}"))
        })
        .collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// The world a case runs in: a fixture directory, or a fresh one holding the `world` entries with
/// only the `programs` on its `PATH`.
fn world_of(suite: &Value, case: &Value) -> Result<World, String> {
    let fixture = match field(case, "world") {
        Some(_) => text_of(case, "fixture"),
        None => text_of(case, "fixture").or_else(|| text_of(suite, "fixture")),
    };
    if let Some(fixture) = fixture {
        // `""` is the fixtures directory itself.
        return World::at(match fixture {
            "" => FIXTURES.to_path_buf(),
            fixture => FIXTURES.join(fixture),
        });
    }
    let entries = (texts_of(case, "world")?.or(texts_of(suite, "world")?)).unwrap_or_default();
    let mut world = World::new(&entries.iter().map(String::as_str).collect::<Vec<_>>())?;
    if let Some(programs) = texts_of(case, "programs")?.or(texts_of(suite, "programs")?) {
        world.set_path_executables(&programs.iter().map(String::as_str).collect::<Vec<_>>())?;
    }
    Ok(world)
}

/// Runs `case` of `suite`, at each of its lines and cursors, and says why it does not hold.
fn run_case(suite: &Value, case: &Value) -> Result<(), String> {
    if text_of(case, "os").is_some_and(|os| ![FAMILY, OS].contains(&os))
        || field(case, "ignore").is_some()
    {
        return Ok(());
    }
    let mut world = world_of(suite, case)?;
    let cwd = world.cwd().to_string_lossy().into_owned();
    let World { engine, stack, .. } = &mut world;
    if flag(case, "std") {
        nu_std::load_standard_library(engine).map_err(|e| e.to_string())?;
    }
    if let Some(prelude) = text_of(suite, "prelude") {
        eval_source(engine, stack, &substitute(prelude, &cwd))?;
    }
    if let Some(setup) = text_of(case, "setup") {
        let ran = eval_source(engine, stack, &substitute(setup, &cwd));
        match (ran, text_of(case, "setup_error")) {
            (Err(error), Some(want)) if error.contains(want) => return Ok(()),
            (Err(error), _) => return Err(error),
            (Ok(_), Some(want)) => return Err(format!("the setup runs, without {want:?}")),
            (Ok(_), None) => {}
        }
    }
    let lines = match text_of(case, "line_from") {
        Some(source) => vec![
            (eval_source(engine, stack, source)?.coerce_into_string())
                .map_err(|e| e.to_string())?,
        ],
        None => {
            (texts_of(case, "lines")?.or(texts_of(case, "line")?)).ok_or("a case has a `line`")?
        }
    };
    let paths = (field(suite, "paths").or_else(|| field(case, "paths")))
        .is_some_and(|paths| paths.as_bool().unwrap_or(false));
    let lines: Vec<String> = lines
        .iter()
        .map(|line| localize(line, &cwd, paths))
        .collect();
    let cursors = match field(case, "at") {
        Some(Value::List { vals, .. }) => vals.iter().map(Some).collect(),
        at => vec![at],
    };
    let mut completer = NuCompleter::new(Arc::new(engine.clone()), Arc::new(stack.clone()));
    let history = match text_of(case, "history_from") {
        Some(source) => Recorded::new(&eval_source(engine, stack, source)?, &cwd)?,
        None => (field(case, "history").map(|history| Recorded::new(history, &cwd)))
            .transpose()?
            .unwrap_or_default(),
    };
    if field(case, "hint").is_none() {
        completer.learn_usage(&history);
    }
    if let Some(before) = text_of(case, "before") {
        completer.complete_blocking(before, before.len());
    }
    let mut faults = Vec::new();
    for line in &lines {
        for at in &cursors {
            let at = match at {
                Some(at) => usize::try_from(at.as_int().map_err(|e| e.to_string())?)
                    .map_err(|_| "`at` is negative")?,
                None => line.len(),
            };
            let checked = match text_of(case, "hint") {
                Some(hint) => check_hint(case, engine, stack, &history, &cwd, line, hint),
                None => check(case, &mut completer, engine, stack, &cwd, paths, line, at),
            };
            if let Err(why) = checked {
                faults.push(match (lines.len(), cursors.len()) {
                    (1, 1) => why,
                    _ => format!("{line:?} at {at}: {why}"),
                });
            }
        }
    }
    faults
        .is_empty()
        .then_some(())
        .ok_or_else(|| faults.join("; "))
}

/// Checks that the history hint for `line`, typed in `cwd` after `history`, is `want`: once the
/// completer has answered for the line, which the hint reads, unless the case is `answered: false`.
fn check_hint(
    case: &Value,
    engine: &EngineState,
    stack: &Stack,
    history: &Recorded,
    cwd: &str,
    line: &str,
    want: &str,
) -> Result<(), String> {
    let engine = Arc::new(engine.clone());
    let latest = LatestAnswer::default();
    if field(case, "answered").is_none_or(|answered| answered.as_bool().unwrap_or(true)) {
        NuCompleter::new(Arc::clone(&engine), Arc::new(stack.clone()))
            .with_latest(latest.clone())
            .complete_blocking(line, line.len());
    }
    let mut hinter = NuHinter::new(engine, Style::new()).with_answers(latest);
    let got = hinter.handle(line, line.len(), history, false, cwd);
    (got == want)
        .then_some(())
        .ok_or_else(|| format!("hinted {got:?}, not {want:?}"))
}

/// Checks what `completer` offers for `line` at `at` against what `case` expects, its paths
/// written for this platform when `paths` says so.
#[allow(clippy::too_many_arguments)]
fn check(
    case: &Value,
    completer: &mut NuCompleter,
    engine: &mut EngineState,
    stack: &mut Stack,
    cwd: &str,
    paths: bool,
    line: &str,
    at: usize,
) -> Result<(), String> {
    // As the line editor leaves it for a completer that reads `commandline`.
    if let Ok(mut repl) = engine.repl_state.lock() {
        (repl.buffer, repl.cursor_pos) = (line.to_owned(), at);
    }
    let (offered, message) = completer.complete_blocking_with_message(line, at);
    let expected = |name: &str| -> Result<Option<Vec<String>>, String> {
        let values = texts_of(case, name)?;
        Ok(values.map(|values| values.iter().map(|v| localize(v, cwd, paths)).collect()))
    };
    let values: Vec<String> = offered.iter().map(|item| item.value.clone()).collect();
    let mut faults = Vec::new();
    let want = match text_of(case, "values_from") {
        Some(source) => Some(texts(&eval_source(engine, stack, source)?)?),
        None => expected("values")?,
    };
    if let Some(want) = want
        && want != values
    {
        faults.push(format!("offered {values:?}, not {want:?}"));
    }
    if let Some(mut want) = expected("set")? {
        let mut got = values.clone();
        got.sort();
        want.sort();
        if got != want {
            faults.push(format!("offered {values:?}, not {want:?} in any order"));
        }
    }
    // How the count may not compare with each bound.
    let count = values.len() as i64;
    for (name, refused, what) in [
        ("count", [Less, Greater], "not"),
        ("at_least", [Less, Less], "fewer than"),
        ("at_most", [Greater, Greater], "more than"),
    ] {
        if let Some(bound) = field(case, name).and_then(|bound| bound.as_int().ok())
            && refused.contains(&count.cmp(&bound))
        {
            faults.push(format!("offered {count} values, {what} {bound}"));
        }
    }
    // Whether each value is offered at all, or (`as_path`) offered as a path is: undescribed.
    for (name, wanted, as_path) in [
        ("has", true, false),
        ("lacks", false, false),
        ("as_paths", true, true),
        ("no_paths", false, true),
    ] {
        for value in expected(name)?.unwrap_or_default() {
            let found = match as_path {
                true => {
                    (offered.iter()).any(|item| item.value == value && item.description.is_none())
                }
                false => values.contains(&value),
            };
            if found != wanted {
                faults.push(format!("{value:?} breaks `{name}` in {values:?}"));
            }
        }
    }
    if let Some(within) = expected("within")? {
        for stray in values.iter().filter(|value| !within.contains(value)) {
            faults.push(format!("{stray:?} is not one of {within:?}"));
        }
    }
    if let Some(order) = expected("order")? {
        let places: Vec<_> = (order.iter())
            .map(|value| values.iter().position(|offered| offered == value))
            .collect();
        if places.iter().any(Option::is_none) || !places.is_sorted() {
            faults.push(format!(
                "{order:?} are not offered in that order in {values:?}"
            ));
        }
    }
    if let Some(other) = text_of(case, "same_as") {
        let theirs = completer.complete_blocking(other, other.len());
        let theirs: Vec<String> = theirs.iter().map(|item| item.value.clone()).collect();
        if theirs != values {
            faults.push(format!("offered {values:?}, but {theirs:?} for {other:?}"));
        }
    }
    if let Some(first) = expected("first")?.and_then(|first| first.into_iter().next())
        && values.first() != Some(&first)
    {
        faults.push(format!("{values:?} does not start with {first:?}"));
    }
    if let Some(want) = expected("displays")? {
        let got: Vec<&str> = offered.iter().map(Suggestion::display_value).collect();
        if got != want {
            faults.push(format!("shown as {got:?}, not {want:?}"));
        }
    }
    for (name, cut) in [("descriptions", false), ("types", true)] {
        let Some(want) = texts_of(case, name)? else {
            continue;
        };
        let got: Vec<String> = (offered.iter())
            .map(|item| item.description.clone().unwrap_or_default())
            .map(|text| match (cut, text.split_once(" · ")) {
                (true, Some((ty, _))) => ty.to_owned(),
                _ => text,
            })
            .collect();
        if got != want {
            faults.push(format!("{name} are {got:?}, not {want:?}"));
        }
    }
    if let (Some(traced), Some(want)) = (text_of(case, "traced"), field(case, "traced_is")) {
        let got = debug_complete(engine, stack, line, at, traced)?;
        if got != *want {
            faults.push(format!("traced as {got:?}, not {want:?}"));
        }
    }
    if let Some(want) = texts_of(case, "evaluated")? {
        // The outcome of each evaluation `debug complete` traces, and the error it gives.
        let pipe = "get sources | where name == evaluation \
                    | each {|s| [$s.outcome ($s.error? | default '' | into string)] }";
        let traced = debug_complete(engine, stack, line, at, pipe)?;
        let (mut outcomes, mut reasons) = (Vec::new(), Vec::new());
        for pair in traced.as_list().map_err(|e| e.to_string())? {
            let [outcome, reason] =
                <[String; 2]>::try_from(texts(pair)?).map_err(|_| "an outcome and its reason")?;
            outcomes.push(outcome);
            reasons.push(reason);
        }
        if outcomes != want {
            faults.push(format!("evaluated as {outcomes:?}, not {want:?}"));
        }
        if let Some(reason) = text_of(case, "reason")
            && !reasons.first().is_some_and(|said| said.contains(reason))
        {
            faults.push(format!("the evaluation says {reasons:?}, not {reason:?}"));
        }
    }
    let span_of = |item: &Suggestion| {
        let [start, end] = [item.span.start, item.span.end].map(|at| Value::test_int(at as i64));
        Value::test_list(vec![start, end])
    };
    if let Some(spans) = field(case, "spans") {
        let got = Value::test_list(offered.iter().map(span_of).collect());
        if got != *spans {
            faults.push(format!("replacing {got:?}, not {spans:?}"));
        }
    }
    if let Some(want) = field(case, "last_span")
        && offered.last().map(span_of).as_ref() != Some(want)
    {
        let got = offered.last().map(|item| item.span);
        faults.push(format!("the last replaces {got:?}, not {want:?}"));
    }
    if let Some(taken) = field(case, "taken") {
        // The first suggestion, or each one a record names by what it shows.
        let wanted: Vec<(Option<&str>, String)> = match taken.as_record() {
            Ok(record) => (record.iter())
                .map(|(shown, line)| Ok((Some(shown.as_str()), line.coerce_string()?)))
                .collect::<Result<_, nu_protocol::ShellError>>()
                .map_err(|e| e.to_string())?,
            Err(_) => vec![(None, taken.coerce_string().map_err(|e| e.to_string())?)],
        };
        for (shown, want) in wanted {
            let item = match shown {
                Some(shown) => offered.iter().find(|item| item.display_value() == shown),
                None => offered.first(),
            };
            let got = item.map(|item| take(line, item));
            if got.as_deref() != Some(want.as_str()) {
                faults.push(format!("taking {shown:?} gives {got:?}, not {want:?}"));
            }
        }
    }
    if let Some(indices) = field(case, "indices") {
        let matched = |item: &Suggestion| match &item.match_indices {
            Some(at) => Value::test_list(at.iter().map(|&i| Value::test_int(i as i64)).collect()),
            None => Value::test_nothing(),
        };
        let got = Value::test_list(offered.iter().map(matched).collect());
        if got != *indices {
            faults.push(format!("matched at {got:?}, not {indices:?}"));
        }
    }
    if let Some(inline) = field(case, "interactive").and_then(|inline| inline.as_bool().ok())
        && completer.is_interactive_at(line, at) != inline
    {
        faults.push(format!("runs inline: {}, not {inline}", !inline));
    }
    if let Some(want) = field(case, "opens") {
        let opens = reedline::Completer::opening(completer, line, at).map(|opens| opens.text);
        if opens.as_deref() != want.as_str().ok() {
            faults.push(format!("opens {opens:?}, not {want:?}"));
        }
    }
    if let Some(token) = expected("replaces")?.and_then(|token| token.into_iter().next()) {
        if offered.is_empty() {
            faults.push(format!("nothing is offered to replace {token:?}"));
        }
        for item in offered.iter() {
            let replaced = line.get(item.span.start..item.span.end);
            if replaced != Some(token.as_str()) {
                let value = &item.value;
                faults.push(format!("{value:?} replaces {replaced:?}, not {token:?}"));
            }
        }
    }
    if flag(case, "ends_at_cursor")
        && let Some(item) = offered.iter().find(|item| item.span.end != at)
    {
        faults.push(format!("{item:?} replaces past the cursor at {at}"));
    }
    let says = |want: Option<&str>, whole: bool| match (&message, want) {
        (Some(said), Some(want)) if whole => said == want,
        (Some(said), Some(want)) => said.contains(want),
        (said, want) => said.is_none() && want.is_none(),
    };
    for (name, whole) in [("message", true), ("message_has", false)] {
        if let Some(want) = field(case, name)
            && !says(want.as_str().ok(), whole)
        {
            faults.push(format!("the message is {message:?}, not {name} {want:?}"));
        }
    }
    faults
        .is_empty()
        .then_some(())
        .ok_or_else(|| faults.join("; "))
}

/// What `pipe` makes of what `debug complete` reports for `line` at `at`.
fn debug_complete(
    engine: &mut EngineState,
    stack: &mut Stack,
    line: &str,
    at: usize,
    pipe: &str,
) -> Result<Value, String> {
    stack.add_env_var("CASE_LINE".into(), Value::test_string(line));
    let source = format!("debug complete --cursor {at} $env.CASE_LINE | {pipe}");
    eval_source(engine, stack, &source)
}

/// `value` with its placeholders (see the module docs) filled in for a case run in `cwd`, and
/// with `/` written as this platform's separator when it is a path.
fn localize(value: &str, cwd: &str, path: bool) -> String {
    let value = substitute(value, cwd);
    match path {
        true => value.replace('/', MAIN_SEPARATOR_STR),
        false => value,
    }
}

/// `value` with `{cwd}`, `{cwd/}`, `{pid}`, `{exe}`, `{fixtures}`, `{user}` and `{group}` filled
/// in for a case run in `cwd`.
fn substitute(value: &str, cwd: &str) -> String {
    let value = (value.replace("{cwd}", cwd))
        .replace("{cwd/}", &cwd.replace('\\', "/"))
        .replace("{pid}", &std::process::id().to_string())
        .replace("{exe}", EXE_SUFFIX)
        .replace("{fixtures}", &FIXTURES.to_string_lossy());
    #[cfg(unix)]
    let value = {
        use nix::unistd::{Group, Uid, User};
        let user = User::from_uid(Uid::current()).ok().flatten();
        let group = (user.as_ref()).and_then(|user| Group::from_gid(user.gid).ok().flatten());
        let value = match user {
            Some(user) => value.replace("{user}", &user.name),
            None => value,
        };
        match group {
            Some(group) => value.replace("{group}", &group.name),
            None => value,
        }
    };
    value
}

/// A history of what a case says was run, and where, how long ago and how it ended: a line, or a
/// record of its `line`, the `cwd` it ran in (relative to the case's, unless absolute), the
/// seconds `ago` and the `exit` status. It answers searches by prefix and cwd as reedline's do, and
/// counts how often it is asked.
#[derive(Default)]
pub(crate) struct Recorded {
    items: Vec<HistoryItem>,
    pub(crate) searches: AtomicUsize,
    pub(crate) counts: AtomicUsize,
}

impl Recorded {
    /// The history a case's `history` describes, for a case run in `cwd`.
    fn new(history: &Value, cwd: &str) -> Result<Self, String> {
        let now = std::time::SystemTime::now();
        let mut recorded = Self::default();
        for entry in history.as_list().map_err(|e| e.to_string())? {
            if let Ok(line) = entry.as_str() {
                recorded.run(line, None, None);
                continue;
            }
            let line = text_of(entry, "line").ok_or("a history entry has a `line`")?;
            let ran_in = text_of(entry, "cwd").map(|dir| match dir {
                "." => cwd.to_owned(),
                dir if dir.starts_with('/') => dir.to_owned(),
                dir => format!("{cwd}{MAIN_SEPARATOR_STR}{dir}"),
            });
            let int = |name| field(entry, name).and_then(|value| value.as_int().ok());
            recorded.run(line, ran_in.as_deref(), int("exit"));
            if let (Some(item), Some(ago)) = (recorded.items.last_mut(), int("ago")) {
                let ago = std::time::Duration::from_secs(ago.unsigned_abs());
                item.start_timestamp = Some((now - ago).into());
            }
        }
        Ok(recorded)
    }

    /// Records that `line` was run in `cwd` and ended with `exit_status`.
    pub(crate) fn run(&mut self, line: &str, cwd: Option<&str>, exit_status: Option<i64>) {
        self.items.push(HistoryItem {
            id: Some(HistoryItemId::new(self.items.len() as i64)),
            cwd: cwd.map(str::to_owned),
            exit_status,
            ..HistoryItem::from_command_line(line)
        });
    }
}

impl History for Recorded {
    fn save(&mut self, item: HistoryItem) -> reedline::Result<HistoryItem> {
        self.run(&item.command_line, item.cwd.as_deref(), item.exit_status);
        Ok(self.items.last().cloned().unwrap_or(item))
    }
    fn load(&self, id: HistoryItemId) -> reedline::Result<HistoryItem> {
        let item = usize::try_from(id.0).ok().and_then(|at| self.items.get(at));
        Ok(item
            .cloned()
            .unwrap_or_else(|| HistoryItem::from_command_line("")))
    }
    fn count(&self, query: SearchQuery) -> reedline::Result<i64> {
        self.counts.fetch_add(1, Ordering::Relaxed);
        Ok(self.search(query)?.len() as i64)
    }
    fn search(&self, query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        self.searches.fetch_add(1, Ordering::Relaxed);
        let mut found: Vec<HistoryItem> = (self.items.iter())
            .filter(|item| match &query.filter.command_line {
                Some(CommandLineSearch::Prefix(prefix)) => item.command_line.starts_with(prefix),
                _ => true,
            })
            .filter(|item| {
                (query.filter.cwd_exact.as_ref()).is_none_or(|cwd| item.cwd.as_ref() == Some(cwd))
            })
            .cloned()
            .collect();
        if matches!(query.direction, SearchDirection::Backward) {
            found.reverse();
        }
        found.truncate(query.limit.map_or(usize::MAX, |limit| limit as usize));
        Ok(found)
    }
    fn update(
        &mut self,
        _: HistoryItemId,
        _: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        Ok(())
    }
    fn clear(&mut self) -> reedline::Result<()> {
        self.items.clear();
        Ok(())
    }
    fn delete(&mut self, _: HistoryItemId) -> reedline::Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn session(&self) -> Option<HistorySessionId> {
        None
    }
}

/// `line` once `item` is taken: its value over the span it replaces, with the space it asks for,
/// and `⌶` where it places the cursor, if it does short of the end.
fn take(line: &str, item: &Suggestion) -> String {
    let (start, end) = (
        item.span.start.min(line.len()),
        item.span.end.min(line.len()),
    );
    let mut taken = format!("{}{}", &line[..start], item.value);
    match item.cursor {
        Some(at) if taken.len() + line.len() - end >= at && line.is_char_boundary(end) => {
            taken.push_str(&line[end..]);
            if at < taken.len() && taken.is_char_boundary(at) {
                taken.insert(at, '⌶');
            }
        }
        _ => {
            if item.append_whitespace {
                taken.push(' ');
            }
            taken.push_str(&line[end..]);
        }
    }
    taken
}

/// The field `name` of `record`, if it is a record that has one.
fn field<'a>(record: &'a Value, name: &str) -> Option<&'a Value> {
    record.as_record().ok()?.get(name)
}

/// The field `name` of `record`, if it is a text.
fn text_of<'a>(record: &'a Value, name: &str) -> Option<&'a str> {
    field(record, name)?.as_str().ok()
}

/// Whether the field `name` of `record` is `true`.
fn flag(record: &Value, name: &str) -> bool {
    field(record, name).is_some_and(|value| value.as_bool().unwrap_or(false))
}

/// The texts of a value that is a text or a list of them.
fn texts(value: &Value) -> Result<Vec<String>, String> {
    match value {
        Value::String { val, .. } => Ok(vec![val.clone()]),
        Value::List { vals, .. } => (vals.iter())
            .map(|val| val.coerce_string().map_err(|e| e.to_string()))
            .collect(),
        _ => Err(format!("{value:?} is neither a text nor a list")),
    }
}

/// The texts of the field `name` of `record`, a text or a list of them.
fn texts_of(record: &Value, name: &str) -> Result<Option<Vec<String>>, String> {
    (field(record, name).map(texts).transpose()).map_err(|e| format!("`{name}`: {e}"))
}
