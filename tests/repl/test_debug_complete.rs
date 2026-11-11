use crate::repl::tests::{TestResult, run_test};
use rstest::rstest;

/// A failing completer is reported as failed, with the error itself, and the sources behind
/// it are asked in its place.
#[test]
fn debug_complete_reports_a_failing_completer_and_the_fallback() -> TestResult {
    run_test(
        "def \"nu-complete boom\" [] { error make {msg: boom} }
        @complete \"nu-complete boom\"
        def --wrapped cmd [...rest] {}
        let r = ('cmd a' | debug complete)
        let failed = $r.sources | where outcome == failed
        [
            ($failed | get name)
            (try { $failed.0.error } catch {|e| $e.msg })
            ($r.sources | where role == fallback | select name outcome)
        ] | to nuon",
        "[[\"custom: nu-complete boom\"], boom, [[name, outcome]; [dynamic, declined], [file, answered]]]",
    )
}

/// The parts of an answer that are not suggestions are listed with their position, and the
/// rest of the answer is still used.
#[test]
fn debug_complete_lists_the_faults_of_an_answer() -> TestResult {
    run_test(
        "def \"nu-complete mixed\" [] { [1 {value: a} {nope: 1} {description: x}] }
        def zzq-faults [x: string@\"nu-complete mixed\"] {}
        let r = ('zzq-faults |' | debug complete)
        let custom = $r.sources | where name == 'custom: nu-complete mixed' | first
        [($custom.faults.at) ($custom.raw | length) ($r.shown.value)] | to nuon",
        "[[2, 3], 4, [\"1\", a]]",
    )
}

/// What is shown is what `commandline complete` completes, in the same order.
#[rstest]
#[case::command("zzq-b")]
#[case::flag("zzq-bar -")]
#[case::custom("zzq-bar --flag a")]
#[case::list("zzq-bar --list ")]
#[case::path("ls Car")]
fn debug_complete_shows_what_commandline_complete_does(#[case] line: &str) -> TestResult {
    run_test(
        &format!(
            "def nu-complete-fruit [] {{ [apple banana] }}
            def zzq-bar [--flag: string@nu-complete-fruit, --list: string@[red green]] {{}}
            let shown = '{line}' | debug complete | get shown
            [($shown == ('{line}' | commandline complete --explain)) ($shown | is-not-empty)] | to nuon"
        ),
        "[true, true]",
    )
}

/// A `|` ending the line marks the cursor; `||` is a pipe before it; `--cursor` wins and leaves
/// the line as it is.
#[rstest]
#[case::marker(
    "'ls |' | debug complete | select line cursor",
    "{line: \"ls \", cursor: 3}"
)]
#[case::marker_after_a_pipe(
    "'ls ||' | debug complete | select line cursor site.kind",
    "{line: \"ls |\", cursor: 4, \"site.kind\": command}"
)]
#[case::flag_wins(
    "'ls |' | debug complete --cursor 1 | select line cursor",
    "{line: \"ls |\", cursor: 1}"
)]
#[case::pipe_inside(
    "'ls | wh' | debug complete | select line cursor",
    "{line: \"ls | wh\", cursor: 7}"
)]
#[case::as_an_argument(
    "debug complete 'ls | wh|' | select line cursor",
    "{line: \"ls | wh\", cursor: 7}"
)]
#[case::flag_in_the_middle(
    "'ls Cargo.toml' | debug complete -c 5 | select line cursor target",
    "{line: \"ls Cargo.toml\", cursor: 5, target: {start: 3, end: 5}}"
)]
fn debug_complete_places_the_cursor(#[case] pipeline: &str, #[case] expected: &str) -> TestResult {
    run_test(&format!("{pipeline} | to nuon"), expected)
}

/// The inputs reported are those a completer that echoes its `place` receives here.
#[test]
fn debug_complete_inputs_are_what_a_completer_receives() -> TestResult {
    run_test(
        "def \"nu-complete echo\" [place: record] { [{value: ($place | to nuon)}] }
        def cmd [--flag(-f) x: string@\"nu-complete echo\"] {}
        let r = ('cmd --flag |' | debug complete)
        [($r.shown.0.value == ($r.inputs.place | to nuon)) ($r.shown | length)] | to nuon",
        "[true, 1]",
    )
}

/// The site names the argument and the completer declared for it.
#[test]
fn debug_complete_describes_the_site() -> TestResult {
    run_test(
        "def \"nu-complete x\" [] { [a] }
        def zzq-site [x: string@\"nu-complete x\"] {}
        'zzq-site |' | debug complete | get site | reject input | to nuon",
        "{kind: positional, command: {name: zzq-site, kind: custom, completer: null}, parameter: {name: x, index: 0, flag: null, shape: string, completer: \"nu-complete x\"}}",
    )
}

/// An `@interactive` completer needs the terminal: it is reported and not run.
#[test]
fn debug_complete_does_not_run_an_interactive_completer() -> TestResult {
    run_test(
        "@interactive
        def \"nu-complete pick\" [] { error make {msg: boom} }
        def cmd [x: string@\"nu-complete pick\"] {}
        'cmd |' | debug complete | get sources | where name == 'custom: nu-complete pick' | get outcome | to nuon",
        "[needs_terminal]",
    )
}

/// Candidates that matching removed are listed with the reason.
#[test]
fn debug_complete_says_why_a_candidate_was_dropped() -> TestResult {
    run_test(
        "def \"nu-complete w\" [] { [apple banana] }
        def zzq-dropped [x: string@\"nu-complete w\"] {}
        let r = ('zzq-dropped app' | debug complete)
        [($r.shown.value) ($r.dropped | select text reason)] | to nuon",
        "[[apple], [[text, reason]; [banana, \"no match for \\\"app\\\"\"]]]",
    )
}

/// A second run is answered from the cache where the first had to look. The cache outlives a run
/// and is shared by the tests of this process, so a test that has a cacheable completer names
/// its command for itself alone.
#[test]
fn debug_complete_says_what_the_cache_answered() -> TestResult {
    run_test(
        "def \"nu-complete kept\" [] { [apple banana] }
        def zzq-kept [x: string@\"nu-complete kept\"] {}
        ['zzq-kept a' 'zzq-kept b'] | each {|line|
            $line | debug complete | get sources | where name == 'custom: nu-complete kept' | get cached.0
        } | to nuon",
        "[miss, hit]",
    )
}
