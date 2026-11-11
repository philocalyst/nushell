//! Completion delegation: a wrapper's command line (`sudo`, `env`, ...) and a completer's
//! `{delegate: {from}}` both complete part of the line as a command line of its own, and
//! their answers replace the right bytes of the line that was typed.
//!
//! Every test runs over [`PROGRAMS`], the only things on `PATH`, and a working directory
//! holding `notes.txt` and `docs/`.

use super::{
    harness::{Session, values},
    support::{merge_input, new_program_engine},
};
use nu_cli::{CompletionEngine, NuCompleter};
use nu_protocol::engine::{EngineState, Stack};
use proptest::{
    collection::vec,
    sample::select,
    test_runner::{Config, TestCaseError, TestRunner},
};
use reedline::Suggestion;
use rstest::rstest;
use std::{env::consts::EXE_SUFFIX, sync::Arc};
use tempfile::TempDir;

/// The programs on `PATH`.
const PROGRAMS: &[&str] = &["git", "gitk", "grep", "sleep"];

/// An engine over the fixture, and the temporary directory it lives in.
struct Fixture {
    root: TempDir,
    engine_state: Arc<EngineState>,
    stack: Arc<Stack>,
}

impl Fixture {
    /// A fixture with `setup` evaluated in it, after loading the standard library if `std`.
    fn new(setup: &str, std: bool) -> Self {
        let (root, mut engine_state, mut stack) = new_program_engine(PROGRAMS);
        if std {
            nu_std::load_standard_library(&mut engine_state).expect("the standard library loads");
        }
        merge_input(setup.as_bytes(), &mut engine_state, &mut stack).expect("setup runs");
        Self {
            root,
            engine_state: Arc::new(engine_state),
            stack: Arc::new(stack),
        }
    }

    /// A fixture whose external completer is `closure`.
    fn with_completer(closure: &str) -> Self {
        let setup = format!("$env.config.completions.external.completer = {closure}");
        Self::new(&setup, false)
    }

    /// What a fresh completer offers for `line` at its end.
    fn complete(&self, line: &str) -> Arc<[Suggestion]> {
        self.complete_at(line, line.len())
    }

    /// What a fresh completer offers for `line` at `cursor`, as given (not clamped to the line).
    fn complete_at(&self, line: &str, cursor: usize) -> Arc<[Suggestion]> {
        NuCompleter::new(Arc::clone(&self.engine_state), Arc::clone(&self.stack))
            .complete_blocking(line, cursor)
    }
}

/// In a parsed file the word under the cursor runs on past it; the answer replaces all of it,
/// as it does for a word that is not delegated.
#[test]
fn a_delegated_word_is_replaced_whole_in_a_file() {
    let fixture = Fixture::with_completer("{|buffer| {delegate: {from: 5}}}");
    let engine = CompletionEngine::new(&fixture.engine_state, &fixture.stack);
    for source in ["xcmd gitZZ one", "xcmd gitZZ", "xcmd sudo gitZZ one"] {
        let cursor = source.find("ZZ").expect("a marker");
        let suggestions = engine.fetch_completions_within_file("file.nu", cursor, source);
        let git = (suggestions.iter())
            .find(|item| item.suggestion.value == format!("git{EXE_SUFFIX}"))
            .unwrap_or_else(|| panic!("no program completion for {source:?}"));
        let span = git.suggestion.span;
        assert_eq!(&source[span.start..span.end], "gitZZ", "{source:?}");
    }
}

/// The sources of a delegation stand for another machine, which is slow to ask: a source asked
/// the same question on the next keystroke is not run again.
#[test]
fn a_delegates_sources_are_not_run_again_for_the_next_keystroke() {
    let log_dir = TempDir::new().expect("a temporary directory");
    let log = log_dir.path().join("asked");
    let fixture = Fixture::with_completer(&format!(
        "{{|buffer| if ($buffer | str starts-with 'xcmd ') {{
            {{delegate: {{from: 5,
                paths: {{|dir| $'paths($dir)(char nl)' | save --append '{log}'; ['rustc-notes', 'rust-toolchain', 'docs/']}},
                commands: {{|| $'commands(char nl)' | save --append '{log}'; ['cat', 'cargo']}},
            }}}}
        }}}}",
        log = log.display()
    ));
    let mut session = Session::new(&fixture.engine_state, &fixture.stack);
    let asked = || std::fs::read_to_string(&log).unwrap_or_default();

    // Typing on within a word asks nothing more.
    for line in ["xcmd cat ", "xcmd cat r", "xcmd cat ru", "xcmd cat rus"] {
        session.complete(line, line.len());
    }
    assert_eq!(asked(), "paths.\n");

    // Another directory is another question, and so is the list of commands.
    for line in ["xcmd cat docs/", "xcmd c", "xcmd ca"] {
        session.complete(line, line.len());
    }
    assert_eq!(asked(), "paths.\npathsdocs/\ncommands\n");

    // The answer is the same as the one that was asked for.
    let kept = session.complete("xcmd cat rust", 13);
    assert_eq!(values(&kept), ["rustc-notes", "rust-toolchain"]);
}

/// What the property tests type: wrappers, options, assignments, quotes and brackets, and
/// characters wider than a byte.
const WORDS: &[&str] = &[
    "sudo", "doas", "env", "nice", "nohup", "time", "timeout", "^watch", "xargs", "exec", "-u",
    "-n", "-Eu", "-", "--", "--user=x", "--user", "-é", "A=1", "=", "é=", "5", "gi", "git", "é",
    "'q", "\"", "$x", "(", ")", "{", "}", "[", "|", ";", "..", "~/", "/", "", "ls", "let",
    "extern", "xcmd",
];

/// Whatever is typed, wherever the cursor is, completing neither panics nor answers outside the
/// line: not for the wrappers, and not for a completer that delegates from anywhere. Every
/// position of a few wrapper lines is covered, not just the ones a random draw reaches.
#[test]
fn completing_arbitrary_wrapper_lines_never_panics() {
    let plain = Fixture::new("", false);
    let delegating = Fixture::with_completer(
        "{|buffer|
            let bytes = ($buffer | encode utf8 | length)
            {delegate: {from: ($bytes * 7 // 10 - 2)}}
        }",
    );
    let on_the_line = |line: &str, cursor: usize, fixtures: &[&Fixture]| {
        for fixture in fixtures {
            for suggestion in fixture.complete_at(line, cursor).iter() {
                if line
                    .get(suggestion.span.start..suggestion.span.end)
                    .is_none()
                {
                    return Err(format!("{line:?} at {cursor}: {suggestion:?}"));
                }
            }
        }
        Ok(())
    };
    for line in [
        "sudo -Eu root env A=1 timeout 5 nice -n 5 gi é",
        "^watch -n 2 xargs -I {} git ch",
        "exec -a é git",
    ] {
        for cursor in 0..=line.len() + 1 {
            on_the_line(line, cursor, &[&plain]).unwrap_or_else(|problem| panic!("{problem}"));
        }
    }

    let strategy = (vec(select(WORDS), 0..9), 0usize..64, 0usize..3);
    TestRunner::new(Config::with_cases(256))
        .run(&strategy, |(words, cursor, spaces)| {
            let line = words.join(&" ".repeat(spaces + 1));
            let cursor = cursor.min(line.len() + 2);
            on_the_line(&line, cursor, &[&plain, &delegating]).map_err(TestCaseError::fail)
        })
        .expect("completion stays on the line");
}

/// The standard library's `ssh` extern completes the remote command on the remote host. The
/// `ssh` here is a script that logs how it was run and answers as a host would; nothing
/// reaches the network.
#[cfg(unix)]
mod ssh {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, path::PathBuf};

    /// A fixture with a fake `ssh` that runs `behaviour` (a shell script body after logging),
    /// and the log.
    fn fixture_with_ssh(behaviour: &str) -> (Fixture, PathBuf) {
        let fixture = Fixture::new("use std/completions *", true);
        let log = fixture.root.path().join("ssh.log");
        let script = fixture.root.path().join("bin").join("ssh");
        let body = format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\n{behaviour}\n",
            log.display()
        );
        std::fs::write(&script, body).expect("a fake ssh");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("an executable ssh");
        (fixture, log)
    }

    /// A host whose home directory holds `etc/` and `motd`, and whose commands are `foo` and
    /// `fob` (listed twice).
    const HOST: &str = "case \"$*\" in
        *'ls -1Ap'*) printf 'etc/\\nmotd\\n' ;;
        *compgen*) printf 'foo\\nfob\\nfoo\\n' ;;
    esac";

    /// The remote command, its arguments and its files complete from what the host answers, and
    /// replace the last word of the line, whatever comes before the remote command.
    #[rstest]
    #[case::command("ssh host fo", &["fob", "foo"])]
    #[case::connection_options("ssh -p 2222 -i key -o X=y host fo", &["fob", "foo"])]
    #[case::attached_option("ssh -p2222 host fo", &["fob", "foo"])]
    #[case::options_after_the_host("ssh host -t fo", &["fob", "foo"])]
    #[case::user_and_host("ssh me@host fo", &["fob", "foo"])]
    #[case::wrapped_remote("ssh host sudo fo", &["fob", "foo"])]
    #[case::dashed_remote_arguments("ssh host ls -la et", &["etc/"])]
    #[case::remote_files("ssh host cat mo", &["motd"])]
    #[case::after_a_pipe("echo hi | ssh host fo", &["fob", "foo"])]
    fn the_remote_line_completes_from_the_remote_host(
        #[case] line: &str,
        #[case] expected: &[&str],
    ) {
        let (fixture, _) = fixture_with_ssh(HOST);
        let suggestions = fixture.complete(line);
        let token = line.rsplit(' ').next().unwrap_or_default();
        let offered: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(offered, expected, "{line:?}");
        for suggestion in suggestions.iter() {
            let replaced = line.get(suggestion.span.start..suggestion.span.end);
            assert_eq!(replaced, Some(token), "{line:?}: {suggestion:?}");
        }
    }

    /// The listing of remote commands runs in batch mode, so it never asks for a password, and
    /// repeats only the options that choose how to connect.
    #[test]
    fn the_remote_listing_never_asks_and_repeats_only_the_connection_options() {
        let (fixture, log) = fixture_with_ssh(HOST);
        // `-v` and `-t` do not choose how to connect; `-p`, `-i`, `-J`, `-o`, `-F` and `-l` do.
        fixture.complete("ssh -v -t -p 2222 -i key -J jump -o X=y -F cfg -l me host fo");

        let logged = std::fs::read_to_string(log).expect("ssh was run");
        assert_eq!(
            logged.lines().next(),
            Some(
                "-n -o BatchMode=yes -o ConnectTimeout=3 -p 2222 -i key -J jump -o X=y -F cfg \
                 -l me host bash -c 'compgen -c' 2>/dev/null || sh -c 'IFS=:; for d in $PATH; \
                 do ls -1 \"$d\"; done' 2>/dev/null"
            )
        );
    }

    /// The directory of a remote file is listed quoted for the remote shell, `~/` left outside
    /// the quotes so that it still means the home directory.
    #[test]
    fn a_directory_is_quoted_for_the_remote_shell() {
        let (fixture, log) = fixture_with_ssh(HOST);
        let listed = [
            ("dir/x", "'dir/'"),
            ("`my dir/x", "'my dir/'"),
            ("\"it's/x", "'it'\\''s/'"),
            ("~/sub/x", "~/'sub/'"),
            ("~/x", "~/''"),
            ("/x", "'/'"),
            ("x", "'.'"),
        ];
        for (word, _) in listed {
            fixture.complete(&format!("ssh host cat {word}"));
        }

        let logged = std::fs::read_to_string(log).expect("ssh was run");
        let listings: Vec<_> = (logged.lines())
            .filter_map(|line| line.split(" host ").nth(1))
            .collect();
        assert_eq!(listings, listed.map(|(_, dir)| format!("ls -1Ap -- {dir}")));
    }

    /// Whatever goes wrong on the way to the host, there is nothing to complete: not the local
    /// files, not the local commands.
    #[rstest]
    #[case::refused("exit 255")]
    #[case::no_output("true")]
    fn a_failing_ssh_offers_nothing(#[case] behaviour: &str) {
        let (fixture, _) = fixture_with_ssh(behaviour);
        for line in [
            "ssh host fo",
            "ssh host gi",
            "ssh host cat no",
            "ssh host cat do",
        ] {
            assert!(fixture.complete(line).is_empty(), "{line:?}");
        }
    }

    /// The options and values before the destination, and the destination itself, are not the
    /// remote command, so ssh is not run for them.
    #[rstest]
    #[case::option_value("ssh -p ")]
    #[case::option("ssh -")]
    #[case::cluster_value("ssh -tl ")]
    #[case::host("ssh ho")]
    #[case::not_ssh("scp host fo")]
    fn only_the_remote_command_is_asked_of_the_host(#[case] line: &str) {
        let (fixture, log) = fixture_with_ssh(HOST);
        fixture.complete(line);
        assert!(!log.exists(), "{line:?} ran ssh");
    }
}
