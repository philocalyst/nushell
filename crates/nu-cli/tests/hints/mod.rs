//! Tests of the REPL's history hint, [`NuHinter`].
//!
//! What a hint is offered for is in `completions/cases/hints.nuon`; these are what data cannot say:
//! that the hinter asks the history no more than it must, runs no completer, and narrows as a fresh
//! query would.

use crate::completions::cases::Recorded;
use nu_ansi_term::Style;
use nu_cli::NuHinter;
use nu_protocol::engine::{EngineState, StateWorkingSet};
use proptest::{
    collection::vec,
    sample::select,
    test_runner::{Config, TestCaseError, TestRunner},
};
use reedline::{FileBackedHistory, Hinter, History, HistoryItem};
use std::{
    fs,
    sync::{Arc, atomic::Ordering},
};
use tempfile::TempDir;

/// An engine with the shell's commands and what `source` defines.
fn engine(source: &str) -> Arc<EngineState> {
    let mut engine = nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
    let mut working_set = StateWorkingSet::new(&engine);
    nu_parser::parse(&mut working_set, None, source.as_bytes(), false);
    let delta = working_set.render();
    engine.merge_delta(delta).expect("merge_delta");
    Arc::new(engine)
}

/// A working directory holding `dir/`, `file.txt` and `old.zip`, and its path.
fn playground() -> (TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("dir")).expect("mkdir");
    for file in ["file.txt", "old.zip"] {
        fs::write(dir.path().join(file), "x").expect("write");
    }
    let cwd = dir.path().to_string_lossy().into_owned();
    (dir, cwd)
}

/// An in-memory history holding `lines`, oldest first.
fn history(lines: &[&str]) -> FileBackedHistory {
    let mut history = FileBackedHistory::new(10_000).expect("history");
    for line in lines {
        (history.save(HistoryItem::from_command_line(*line))).expect("save");
    }
    history
}

/// The hint for `line`, typed in `cwd` with the cursor at its end.
fn hint_with(hinter: &mut NuHinter, history: &dyn History, line: &str, cwd: &str) -> String {
    hinter.handle(line, line.len(), history, false, cwd)
}

/// Typing on narrows what the history gave without asking it again, and counts it once; a line
/// that does not extend the last one asks again.
#[test]
fn typing_more_narrows_without_asking_history_again() {
    let (_dir, cwd) = playground();
    let mut history = Recorded::default();
    for line in ["open file.txt", "open old.zip", "ls", "open dir"] {
        history.run(line, Some(&cwd), Some(0));
    }
    let mut hinter = NuHinter::new(engine(""), Style::new());
    let mut hint = |line: &str| hint_with(&mut hinter, &history, line, &cwd);
    assert_eq!(hint("o"), "pen dir");
    let searches = history.searches.load(Ordering::Relaxed);
    for (line, hinted) in [("open ", "dir"), ("open f", "ile.txt"), ("open x", "")] {
        assert_eq!(hint(line), hinted, "{line:?}");
    }
    assert_eq!(history.searches.load(Ordering::Relaxed), searches);
    assert_eq!(history.counts.load(Ordering::Relaxed), 1);
    assert_eq!(hint("l"), "s");
    assert!(history.searches.load(Ordering::Relaxed) > searches);
}

/// Another cwd asks the history again, and so does a longer history, once a new prompt shows it.
#[test]
fn a_new_cwd_or_a_longer_history_asks_again() {
    let ((_one, one), (_two, two)) = (playground(), playground());
    let mut history = Recorded::default();
    history.run("open file.txt", Some(&one), Some(0));
    let mut hinter = NuHinter::new(engine(""), Style::new());
    assert_eq!(hint_with(&mut hinter, &history, "open ", &one), "file.txt");
    let searches = history.searches.load(Ordering::Relaxed);
    assert_eq!(hint_with(&mut hinter, &history, "open f", &two), "ile.txt");
    assert!(history.searches.load(Ordering::Relaxed) > searches);
    assert_eq!(hint_with(&mut hinter, &history, "open ", &two), "file.txt");

    // Another session saves a line. It is noticed when the line is emptied, which marks a new
    // prompt, and not counted again before.
    history.run("open old.zip", Some(&two), Some(0));
    for (line, hinted) in [("open o", ""), ("", ""), ("open o", "ld.zip")] {
        assert_eq!(
            hint_with(&mut hinter, &history, line, &two),
            hinted,
            "{line:?}"
        );
    }
    assert_eq!(history.counts.load(Ordering::Relaxed), 2);
}

/// The hinter runs no completer: with no answer to read, every line is real, and the completer is
/// never started for it.
#[test]
fn hinting_never_runs_a_completer() {
    let (dir, cwd) = playground();
    let marker = dir.path().join("ran");
    let mut hinter = NuHinter::new(
        engine(&format!(
            "def branches [] {{ 'x' | save -f {marker:?}; [main] }}
             def tool [target: string@branches] {{}}"
        )),
        Style::new(),
    );
    let lines = history(&["tool main", "tool gone"]);
    assert_eq!(hint_with(&mut hinter, &lines, "tool ", &cwd), "gone");
    assert!(!marker.exists(), "the hinter ran a completer");
}

/// A hint is taken whole, or a token at a time, and continues the text before the cursor.
#[test]
fn hint_completes_by_token_and_by_whole() {
    let (_dir, cwd) = playground();
    let mut hinter = NuHinter::new(engine(""), Style::new());
    let history = history(&["open  file.txt"]);
    assert_eq!(hint_with(&mut hinter, &history, "op", &cwd), "en  file.txt");
    assert_eq!(hinter.complete_hint(), "en  file.txt");
    assert_eq!(hinter.next_hint_token(), "en");
    assert!(hinter.continues_text_before_cursor());
}

/// Typing a line in steps, which narrows the cached candidates, gives the same hint as asking
/// history for that line directly.
#[test]
fn narrowing_matches_asking_fresh() {
    #[rustfmt::skip]
    const LINES: &[&str] = &[
        "cd dir", "cd dir/", "cd gone", "cd ", "open file.txt", "open old.zip", "open gone.txt",
        "open", "rm old.zip", "rm *.zip", "rm *.rar", "ls", "ls dir", "ls gone", "save a.json",
        "touch new", "echo dir", "^cd gone", "cp file.txt z", "mv gone z", "c", "o",
    ];
    let (_dir, cwd) = playground();
    let engine = engine("");

    // Older lines, then up to 150 newer ones matching `c` and little else: more than the cap, so
    // the candidates for a short prefix are truncated and hide older lines a longer prefix finds.
    // The noise lines name missing files, so each is checked and rejected: they use up the
    // validation budget, which must be spent the same whether their verdicts are cached or not.
    let strategy = (
        vec(select(LINES), 0..300),
        0usize..150,
        select(LINES),
        (0usize..16, 0usize..16),
    );
    TestRunner::new(Config::with_cases(512))
        .run(&strategy, |(lines, noise, typed, (a, b))| {
            let (short, long) = (a.min(b).min(typed.len()), a.max(b).min(typed.len()));
            let (short, long) = (&typed[..short], &typed[..long]);
            let noise: Vec<_> = (0..noise).map(|n| format!("cp n{n} z")).collect();
            let lines: Vec<&str> = (lines.into_iter())
                .chain(noise.iter().map(String::as_str))
                .collect();
            let history = history(&lines);

            let mut stepwise = NuHinter::new(engine.clone(), Style::new());
            hint_with(&mut stepwise, &history, short, &cwd);
            let narrowed = hint_with(&mut stepwise, &history, long, &cwd);
            let mut fresh = NuHinter::new(engine.clone(), Style::new());
            let asked = hint_with(&mut fresh, &history, long, &cwd);
            (narrowed == asked).then_some(()).ok_or_else(|| {
                TestCaseError::fail(format!(
                    "{short:?} then {long:?} gave {narrowed:?}, fresh gave {asked:?}"
                ))
            })
        })
        .expect("narrowing must agree with a fresh query");
}
