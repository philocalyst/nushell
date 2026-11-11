//! Property tests for the completion engine, written against the [`harness`](super::harness)
//! adapter only. Run them with `cargo test -p nu-cli --test tests -- completions::properties`.
//! `PROPTEST_CASES` sets how many cases each property tries. Shrunk failures are saved in
//! `crates/nu-cli/proptest-regressions/` and replayed first on every later run; commit that file
//! together with the fix. `PROPTEST_MAX_SHRINK_ITERS` bounds shrinking.
//!
//! * [`completion_never_panics_and_spans_are_sane`]: any line, any cursor.
//! * [`accepted_file_names_read_back_as_the_file`] (the escaping round trip): accepting a file
//!   candidate yields an argument that is that file, to the parser, to `open` and to `ls`.
//! * [`quote_reads_back_as_the_text`]: the spelling function of every completion (`quote`) writes
//!   a token that the parser reads back as the text, for any text.
//! * [`narrowing_is_monotone_and_cache_coherent`]: typing more never adds candidates, and a
//!   completer that remembers earlier answers agrees with one that does not.
//! * [`fuzzy_ranking_laws`]: exact match first, prefix matches before fuzzy-only matches,
//!   deterministic order.

use super::harness::*;
use nu_cli::quote::{Quote as OpenQuote, Spelling, quote};
use nu_protocol::Value;
use proptest::{
    prelude::*,
    sample::select,
    test_runner::{Config, FileFailurePersistence, TestRunner},
};
use std::{cell::RefCell, collections::HashSet};

/// Run `test` on `default_cases` values of `strategy` (`PROPTEST_CASES` overrides it), saving
/// shrunk failures under `proptest-regressions/`.
fn check<S: Strategy>(
    default_cases: u32,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) {
    let from_env = |name: &str| std::env::var(name).ok().and_then(|v| v.parse().ok());
    let config = Config {
        cases: from_env("PROPTEST_CASES").unwrap_or(default_cases),
        max_shrink_iters: from_env("PROPTEST_MAX_SHRINK_ITERS").unwrap_or(1000),
        source_file: Some(file!()),
        failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
            "proptest-regressions",
        ))),
        ..Config::default()
    };
    if let Err(failure) = TestRunner::new(config).run(&strategy, test) {
        panic!("{failure}");
    }
}

/// Fail the current case with `message`.
fn fail<T>(message: String) -> Result<T, TestCaseError> {
    Err(TestCaseError::fail(message))
}

/// A custom completer `comp` and a command `t` whose argument it completes.
const COMP: &str = "def comp [] { [apple apricot banana 'a b'] }\ndef t [x: string@comp] {}";

// ---- no panics, sane spans ---------------------------------------------------------------------

/// Words and fragments the line grammar draws from. No command in here changes the
/// filesystem, since completion may evaluate sub-expressions.
#[rustfmt::skip]
const FRAGMENTS: &[&str] = &[
    // commands and keywords
    "ls", "open", "cd", "str trim", "str", "where", "each", "get", "select", "sort-by", "from json",
    "help", "use", "def", "let", "mut", "const", "if", "else", "for", "in", "match", "try", "catch",
    "do", "print", "which", "ps", "date now", "std", "alias", "export", "source", "module", "overlay",
    "extern", "fake-cmd", "attr", "@example", "@complete", "|", "|", "|", ";", "=", "=>", ",", ":",
    // flags
    "--help", "-h", "--", "-", "--raw", "-l", "-la", "--all", "--flag", "-f", "--unknown", "...",
    // variables and paths
    "$x", "$env", "$env.", "$nu.", "$", "$in", "$env.PATH", ".", "..", "./", "../", "~", "~/", "/",
    "src/", "nested dir/", "a.b", "0", "1.5", "true", "null", "1kb", "1..2",
    // brackets
    "(", ")", "{", "}", "[", "]", "{|x|", "{|", "(ls ", "[[a b]; [1 2]]", "{a: 1}", "r#'", "'#",
    // quotes, all three kinds, open and closed
    "\"", "'", "`", "\"abc\"", "'abc'", "`abc`", "\"abc", "'abc", "`abc", "\"a b", "'it''s'",
    "\"a\\\"b\"", "$\"", "$\"(", "$'",
    // unicode: accents, combining marks, emoji, CJK, right-to-left, zero width, odd case folding
    "ü", "ünï", "e\u{301}", "e\u{301}\u{302}\u{303}", "😀", "👨\u{200d}👩\u{200d}👧", "日本語", "שלום",
    "\u{200b}", "\u{feff}", "İ", "ß", "ﬁ", "\u{0}", "\u{7f}",
    // comments
    "#", "# c", "#!",
];

/// Separators between fragments: usually a space, sometimes none, a tab or a newline.
const SEPARATORS: &[&str] = &[" ", " ", " ", " ", "", "", "\t", "\n", "\r\n", "  "];

/// A nushell-ish line: fragments of the grammar and junk text, joined by separators.
fn any_line() -> impl Strategy<Value = String> {
    let fragment = prop_oneof![
        8 => select(FRAGMENTS).prop_map(str::to_owned),
        1 => "[ -~]{1,4}",
        1 => "\\PC{1,3}",
    ];
    let piece = (fragment, select(SEPARATORS)).prop_map(|(f, s)| format!("{f}{s}"));
    prop::collection::vec(piece, 0..12).prop_map(|pieces| pieces.concat())
}

/// A line and a raw cursor: usually inside the line (often not on a char boundary), sometimes at
/// its end, sometimes past it.
fn line_and_cursor() -> impl Strategy<Value = (String, usize)> {
    any_line().prop_flat_map(|line| {
        let len = line.len();
        let cursor = prop_oneof![1 => Just(len), 4 => 0..=len, 1 => len..len + 4];
        (Just(line), cursor)
    })
}

/// A world with files, an executable-free `PATH` and a custom completer, shared by the cases.
fn grammar_world() -> World {
    #[rustfmt::skip]
    let files = ["plain.txt", "with space.txt", "it's.txt", "$dollar.txt", "ünï.txt", "emoji😀.txt",
        "nested dir/inner file.txt", "src/main.rs"];
    let mut world = World::new(&files).expect("the grammar world is made of ordinary names");
    (world.run(
        "def comp [] { [apple banana cherry 'a b'] }
         def t [x: string@comp, --flag (-f): string@comp] {}",
    ))
    .expect("the grammar world's definitions");
    world
}

/// Completing any line at any cursor neither panics nor answers with a span that is off the line,
/// off a char boundary or away from the cursor.
#[test]
fn completion_never_panics_and_spans_are_sane() {
    let world = RefCell::new(grammar_world());
    check(64, line_and_cursor(), |(line, cursor)| {
        let candidates = world.borrow_mut().complete(&line, cursor);
        check_spans(&line, cursor, &candidates).or_else(fail)
    });
}

/// Lines that break completers, at every byte offset: the deterministic companion of the
/// property above.
#[rustfmt::skip]
const TRICKY: &[&str] = &[
    "", " ", "\t", "\n", "\r\n", "#", "# a comment", "ls # a comment ", "ls |", "ls|wh", "ls;ls",
    "(ls ", "[1 2 3] | each {|x| $x ", "[1 2 3] | each {|x| $x + ", "ls | where size ",
    "ls | get na", "{a: ", "{a: (ls ", "let x = {a: 1}; $x.", "$env.config.completions.",
    "\"string with $(ls ", "$\"interp ($nu.", "\"unterminated", "'unterminated", "`unterminated",
    "r#'raw string", "ls \"my dir/", "ls 'nested dir/", "ls nested\\ dir/", "ls ~/", "ls ../..",
    "ls --long=t", "ls -- -", "^ls --a", "% ls", "ls o> ", "ls e>| ", "ls | save -f ",
    "use std/ass", "use std [a", "overlay use s", "def f [x: string@co", "def f [--",
    "if true { ls } el", "match $x { {a: $y} => $", "try { ls } catch {|e| $e.m", "@example \"x\" { ",
    "alias f = ^ls; f ", "ls ünï", "ls emoji😀", "ls e\u{301}", "ls \u{200b}", "日本語 ", "ls 日本語/",
    "let 😀 = 1; $😀", "ls \u{0}", "1 + ", "1..", "'a' =~ ", "ls ...[", "ls \\", "ls \\\nl",
    "t ", "t a", "t -", "t --flag ", "t -f a", "fake-cmd ", "fake-cmd -f ",
    "ls | each {|f| $f.type == dir and $f.name == 'a' }",
    "ls | where {|r| $r.size > 1kb } | each { $in.name == 'b' }",
    "try { ls | where type == dir } catch {|e| $e.msg == 'x' }",
    "for x in (ls) { if $x.type == dir { $x.name } else { null } }",
    "let rows = (ls); $rows | each {|r| match $r.type { 'dir' => 1, _ if $r.name == 'x' => 2, _ => 3 } }",
    "ls | do { $in | where type == dir | each {|d| $d.name =~ 'a' } }",
    "$\"abc (ls | where type == dir | get name | where $it == 'g') def\"",
    "ls | each {|a| ls | each {|b| [$a.name $b.name] | where $it == 'x' } }",
    "ls | get name | each {|n| {name: $n, size: ($n | str length)} } | where size > 3",
];

/// Every [`TRICKY`] line has sane spans at every byte offset, inside chars and past the end too.
#[test]
fn tricky_lines_have_sane_spans_at_every_cursor() {
    let mut world = grammar_world();
    for line in TRICKY {
        for cursor in 0..line.len() + 3 {
            let candidates = world.complete(line, cursor);
            check_spans(line, cursor, &candidates).unwrap_or_else(|problem| panic!("{problem}"));
        }
    }
}

/// Lines that would write, wherever it is in them, are never run to complete them, so nothing is
/// made or removed however they are completed.
#[test]
fn completing_never_writes() {
    let mut world = World::new(&["keep.txt", "dir/"]).expect("ordinary names");
    for line in [
        "ls | each {|f| rm $f.name; $f.type == ",
        "touch made.txt; ls | each {|f| $f.type == ",
        "ls | save made.txt; ls | each {|f| $f.type == ",
        "mkdir made; ls | where {|f| $f.type == ",
        "ls | each {|f| (mkdir made) == ",
        "let rows = (rm keep.txt; ls); $rows | where type == ",
        "ls | save made.txt | where type == ",
        "ls out> made.txt | where type == ",
        "ls | where name == (rm keep.txt) | where type == ",
    ] {
        world.complete(line, line.len());
    }
    assert!(world.cwd().join("keep.txt").exists(), "a file was removed");
    for made in ["made.txt", "made"] {
        assert!(!world.cwd().join(made).exists(), "{made} was made");
    }
}

/// What a pipeline read is kept for `debug complete`'s next ask, as for the editor's next
/// keystroke: a file that changes where the cache cannot see it still gives the first answer,
/// while a completer without that cache reads it as it is.
#[test]
fn a_second_ask_does_not_read_again() {
    let mut world = World::new(&[]).expect("an empty directory");
    let toml = world.cwd().join("fixture.toml");
    let manifest = "[package]\nname = 'fixture'\n\n[dependencies]\nserde = '1'\n";
    std::fs::write(&toml, manifest).expect("a fixture to write");
    let trace = "debug complete 'open fixture.toml | get |' \
                 | {outcome: ($in.sources | where name == evaluation | first | get outcome), shown: $in.shown.value}";
    let first = world.eval(trace).expect("debug complete runs");
    std::fs::write(&toml, "[other]\nx = 1\n").expect("a rewrite");
    let second = world.eval(trace).expect("debug complete runs");
    let outcome = |value: &Value| value.get_data_by_key("outcome");
    assert_eq!(outcome(&first), Some(Value::test_string("ran")));
    assert_eq!(outcome(&second), Some(Value::test_string("cached")));
    assert_eq!(
        first.get_data_by_key("shown"),
        second.get_data_by_key("shown")
    );
    let line = "open fixture.toml | get ";
    assert_eq!(values(&world.complete(line, line.len())), ["other"]);
}

// ---- escaping round trip -----------------------------------------------------------------------

/// How the argument is open on the line when the user completes: bare (`""`), or inside an
/// unterminated quote of one of the three kinds.
const QUOTES: &[&str] = &["", "\"", "'", "`"];

/// Whether the user can type `c` literally after `quote` opens the argument and mean it: not a
/// character that closes the quote or escapes (inside a quote), not one that splits the word,
/// opens a sub-expression, a variable or a comment, or is a glob (bare).
fn types(quote: &str, c: char) -> bool {
    match quote {
        "" => !c.is_whitespace() && !"\"'`$#;|()[]{}\\*?<>^".contains(c),
        _ => c != '\n' && !quote.contains(c) && !(quote == "\"" && c == '\\'),
    }
}

/// The text the user has typed after `open `: the directory part and the first `want` chars of
/// `leaf`, cut shorter (down to none) until every char is one that `quote` lets the user type.
/// A bare word cannot start with `-` (a flag), `~` (the home directory) or `=` (an operator)
/// unless it is empty. `None` when even the directory part cannot be typed this way.
fn typed_prefix(quote: &str, dir: &str, leaf: &str, want: usize) -> Option<String> {
    // A lone `~` completes the home directory, in a quote or not; `..` and `...` as the last
    // component walk up; `&&` bare ends the command.
    let typable = |text: &str| {
        let last = text.rsplit('/').next().unwrap_or(text);
        text.chars().all(|c| types(quote, c))
            && !(quote.is_empty() && (text.starts_with(['-', '~', '=']) || text.contains("&&")))
            && text != "~"
            && !(last.len() >= 2 && last.bytes().all(|b| b == b'.'))
    };
    let leaf_chars: Vec<char> = leaf.chars().collect();
    (0..=want.min(leaf_chars.len()))
        .rev()
        .map(|k| format!("{dir}{}", leaf_chars[..k].iter().collect::<String>()))
        .find(|typed| typed.is_empty() || typable(typed))
}

/// Complete `open <typed>` for the file `dir`/`leaf`, accept the candidate that displays that file,
/// and check that the accepted argument is that file: as the parser reads it, as `open` reads it
/// and as `ls` globs it. `Ok(false)` when this quoting cannot express the typed prefix (nothing
/// was checked).
fn round_trip(
    world: &mut World,
    quote: &str,
    dir: &str,
    leaf: &str,
    want: usize,
) -> Result<bool, String> {
    let Some(typed) = typed_prefix(quote, dir, leaf, want) else {
        return Ok(false);
    };
    let rel = format!("{dir}{leaf}");
    let line = format!("open {quote}{typed}");
    let candidates = world.complete(&line, line.len());
    let Some(candidate) = candidates.iter().find(|c| c.display == rel) else {
        let shown: Vec<_> = candidates.iter().map(|c| &c.display).collect();
        return Err(format!(
            "no candidate displays {rel:?} for {line:?}; got {shown:?}"
        ));
    };
    let accepted = accept(&line, candidate);
    let context = format!("{rel:?}: {line:?} + {candidate:?} = {accepted:?}");

    // As the parser reads it: the argument is the file (relative, or made absolute).
    let argument = first_argument(&world.engine, &world.stack, &accepted)
        .map_err(|error| format!("{context}: {error}"))?;
    let absolute = world.cwd().join(&rel).to_string_lossy().into_owned();
    if argument != rel && argument != absolute {
        return Err(format!("{context}: the argument reads as {argument:?}"));
    }
    // Past the parser, whatever the quoting, `open` and `ls` strip control characters such as CR
    // from their argument (`strip_ansi_string_unlikely`) and expand a component of three or more
    // dots to `../..` (`expand_path_with`). The argument above is what completion controls.
    let dots = |c: &str| c.len() >= 3 && c.bytes().all(|b| b == b'.');
    if rel.contains('\r') || rel.split('/').any(dots) {
        return Ok(true);
    }
    // As `open` reads it: it opens that file, and nothing else.
    let rest = &accepted["open".len()..];
    match world.eval(&format!("open --raw{rest} | into string")) {
        Ok(Value::String { val, .. }) if val == format!("content:{rel}") => {}
        other => return Err(format!("{context}: `open` gives {other:?}")),
    }
    // As `ls` globs it: the token matches that file only, which `ls` shows with the control
    // characters of its name as `\u{..}` escapes.
    let listed: String = (rel.chars())
        .map(|c| match c.is_control() {
            true => c.escape_unicode().to_string(),
            false => c.to_string(),
        })
        .collect();
    let names = match world.eval(&format!("ls -D{rest} | get name")) {
        Ok(Value::List { vals, .. }) => vals,
        other => return Err(format!("{context}: `ls` gives {other:?}")),
    };
    match &names[..] {
        [name]
            if name
                .as_str()
                .is_ok_and(|name| name.trim_start_matches("./") == listed) =>
        {
            Ok(true)
        }
        _ => Err(format!("{context}: `ls` matches {names:?}")),
    }
}

/// Round-trip `dir`/`leaf` in `world` in every quoting and at every prefix length, adding what
/// fails to `failures`. Returns how many round trips applied.
fn round_trip_all(world: &mut World, dir: &str, leaf: &str, failures: &mut Vec<String>) -> usize {
    let mut applied = 0;
    for quote in QUOTES {
        for want in 0..=leaf.chars().count() {
            match round_trip(world, quote, dir, leaf, want) {
                Ok(checked) => applied += usize::from(checked),
                Err(error) => failures.push(error),
            }
        }
    }
    applied
}

/// Directories every case world holds a copy of the file in: one plain, one with a space.
const NESTED: [&str; 2] = ["plain/", "dir with space/"];

/// A world with `leaf` at the root and in both [`NESTED`] directories, and decoys a glob in the
/// name would also match. `None` if the filesystem refuses the name.
fn escaping_world(leaf: &str) -> Option<World> {
    let dirs = ["", NESTED[0], NESTED[1]];
    let entries = dirs.map(|dir| format!("{dir}{leaf}"));
    let world = World::new(&entries.each_ref().map(String::as_str)).ok()?;
    let globbed = leaf.replace(['*', '?', '[', ']', '{', '}', ','], "x");
    #[rustfmt::skip]
    let decoys = ["x".to_owned(), "a".to_owned(), "xx".to_owned(), globbed.clone(),
        format!("{globbed}x"), leaf.replace(['[', ']'], ""), leaf.to_lowercase()];
    for dir in dirs {
        for decoy in &decoys {
            // A decoy that the filesystem refuses (it is the file itself, say) is no decoy.
            let _ = world.add(&format!("{dir}{decoy}"));
        }
    }
    Some(world)
}

/// Names that are hard to spell: [`WEIRD`] leaves, names that read as other types, and names
/// built from awkward fragments.
fn file_name() -> impl Strategy<Value = String> {
    #[rustfmt::skip]
    const LITERALS: &[&str] = &[
        "123", "true", "false", "null", "1.5", "-1", "0x1f", "1e3", "inf", "nan", "1kb", "5sec",
        "$nu", "$env", "$x", "r#'x'#", "a b", "a  b", " lead", "trail ", "-", "--", "-a", "--flag",
        "~", "~/x", "~user", "..a", "...", "a..b", "\u{e9}", "e\u{301}", "\u{1e9b}\u{323}",
        "\u{212b}", "\u{c5}", "\u{41}\u{30a}", "ﬁne", "İ", "ß", "😀", "a\u{200b}b", "[a]", "{a,b}",
        "*", "?", "**", "a*b", "a?b", "[", "]", "{", "}", "(", ")", "(a)", "$(ls)", "$(", "'", "''",
        "\"", "\"\"", "`", "``", "\\", "\\n", "a\\ b", "#", "# x", "a;b", "a|b", "a&b", "a<b", "a>b",
        "a=b", "a,b", "%", "!", "!!", "@", "^", "^ls", "+", ".", "a.", "..a", ".a", "tab\there",
        "a\nb",
    ];
    let weird_leaves: Vec<&'static str> =
        WEIRD.iter().filter(|w| !w.contains('/')).copied().collect();
    prop_oneof![
        3 => select(weird_leaves).prop_map(str::to_owned),
        3 => select(LITERALS).prop_map(str::to_owned),
        4 => prop::collection::vec(
            select(&[
                "a", "b", "x", "1", " ", "'", "\"", "`", "$", "#", ";", "|", "*", "?", "[", "]",
                "{", "}", "(", ")", "-", "~", "\\", ".", "&", "=", ",", "%", "!", "@", "^", "<", ">",
                "é", "e\u{301}", "😀", ".txt",
            ][..]),
            1..6,
        )
        .prop_map(|parts| parts.concat()),
    ]
    .prop_filter("cannot be a file name, or is a nested directory", |name| {
        !matches!(name.as_str(), "." | "..")
            && !name.contains(['/', '\0'])
            && NESTED.iter().all(|dir| dir.trim_end_matches('/') != name)
    })
}

/// The escaping round trip over generated names.
#[test]
fn accepted_file_names_read_back_as_the_file() {
    let nested = prop::option::of(select(NESTED.to_vec()));
    let strategy = (file_name(), 0usize..12, select(QUOTES), nested);
    check(64, strategy, |(leaf, want, quote, nested)| {
        let mut world = escaping_world(&leaf)
            .ok_or_else(|| TestCaseError::reject("the filesystem refuses the name"))?;
        let dir = nested.unwrap_or("");
        (round_trip(&mut world, quote, dir, &leaf, want).map(|_| ())).or_else(fail)
    });
}

/// The same round trip over every [`WEIRD`] file at once: each in its own directory depth, every
/// way of quoting, every prefix length. Many names share one world, as they do for a user.
#[test]
fn weird_files_round_trip_in_one_world() {
    let mut world = World::new(WEIRD).expect("WEIRD names fit this filesystem");
    let (mut failures, mut checked) = (Vec::new(), 0);
    for entry in WEIRD.iter().filter(|w| !w.ends_with('/')) {
        let (dir, leaf) =
            (entry.rsplit_once('/')).map_or(("", *entry), |(d, l)| (&entry[..=d.len()], l));
        checked += round_trip_all(&mut world, dir, leaf, &mut failures);
    }
    assert!(checked > 100, "only {checked} round trips applied");
    assert!(
        failures.is_empty(),
        "{} of {} round trips failed:\n{}",
        failures.len(),
        checked + failures.len(),
        failures.join("\n")
    );
}

/// The names the quoting rules single out, each alone at the root with its decoys, round-trip
/// in every quoting: literals, `=`, `&&`, dots only, control whitespace and a leading `~`.
#[test]
fn singled_out_names_are_quoted() {
    let mut failures = Vec::new();
    #[rustfmt::skip]
    let names = ["true", "false", "null", "=", "&&", "...", "....", "tab\there", "new\nline",
        "carriage\rreturn", "~tilde.txt"];
    for name in names {
        let mut world = escaping_world(name).expect("the filesystem holds the name");
        round_trip_all(&mut world, "", name, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---- spelling: `quote` reads back as the text ----------------------------------------------------

/// Text that is hard to spell: unicode, control characters, quotes, sigils, literals.
fn any_text() -> impl Strategy<Value = String> {
    #[rustfmt::skip]
    const PIECES: &[&str] = &[
        "a", "b", "1", "0x", "0b1", "e3", ".", "..", "...", "-", "--", "~", "~/", "$", "$x", "#", "'",
        "\"", "`", "\\", " ", "\t", "\n", "\r", "\0", "\u{7f}", "\u{1b}", "\u{85}", "*", "?",
        "[", "]", "{", "}", "(", ")", "|", ";", "&&", "&", "=", "==", ">", "<", "^", "%", "@", "true",
        "null", "inf", "nan", "kb", "sec", "5sec", "2024-01-15", "T", "r#", "r#'", "é", "e\u{301}",
        "😀", "\u{200b}", ",", ":", "!", "+", "o>", "e>|",
    ];
    prop_oneof![
        3 => any::<String>(),
        6 => prop::collection::vec(select(PIECES), 0..6).prop_map(|pieces| pieces.concat()),
    ]
}

/// A world with commands whose argument slot is a string, any value, a path and a glob.
fn spelling_world() -> World {
    let mut world = World::new(&[]).expect("an empty directory");
    (world.run("def s [x: string] { $x }\ndef a [x: any] { $x }\ndef p [x: path] { $x }\ndef g [x: glob] { $x }"))
        .expect("slot definitions");
    world
}

/// Spell `text` as `how` says and check that every slot reads it back as `text`: a string, `any`
/// (so not a number or a keyword), a path (not expanded), a glob (not expanded, when it has
/// glob characters) and a cell-path member.
fn read_back(world: &World, text: &str, how: Spelling) -> Result<(), String> {
    let token = quote(text, how);
    let context = format!("{text:?} spelled {token} with {how:?}");
    // A path slot also normalizes the separators and dots it is given: out of scope for a name.
    let path = !text.contains(['/', '\\']) && !matches!(text, "." | "..");
    for slot in ["s", "a"].into_iter().chain(path.then_some("p")) {
        let line = format!("{slot} {token}");
        match first_argument(&world.engine, &world.stack, &line) {
            Ok(read) if read == text => {}
            other => return Err(format!("{context}: `{line}` reads {other:?}")),
        }
    }
    // A glob slot keeps the text, and does not expand it unless the spelling says it may.
    if how.glob {
        match world.eval(&format!("g {token}")) {
            Ok(Value::Glob { val, no_expand, .. })
                if val == text && (no_expand || !text.contains(['*', '?', '['])) => {}
            other => return Err(format!("{context}: `g` reads {other:?}")),
        }
    }
    if how.member {
        match world.eval(&format!("{{}} | insert {token} 1 | columns | first")) {
            Ok(Value::String { val, .. }) if val == text => {}
            other => return Err(format!("{context}: as a member it reads {other:?}")),
        }
    }
    Ok(())
}

/// Every way of asking `quote` for a spelling: outside or inside each kind of quote, for a glob
/// slot or not.
fn spellings() -> impl Iterator<Item = Spelling> {
    [
        None,
        Some(OpenQuote::Double),
        Some(OpenQuote::Single),
        Some(OpenQuote::Backtick),
    ]
    .into_iter()
    .flat_map(|inside| {
        [false, true].map(|glob| Spelling {
            glob,
            inside,
            ..Spelling::default()
        })
    })
}

/// The parser reads back what `quote` writes, for any text, in a plain slot and inside each kind
/// of open quote (the spelling stays in it when it can).
#[test]
fn quote_reads_back_as_the_text() {
    let world = spelling_world();
    check(256, any_text(), |text| {
        (spellings().try_for_each(|how| read_back(&world, &text, how))).or_else(fail)
    });
}

/// The same for a cell-path member, whose slot splits on `.`, `?` and `!`, and which is written
/// bare or in double quotes.
#[test]
fn quote_reads_back_as_a_cell_path_member() {
    let world = spelling_world();
    check(128, any_text(), |text| {
        let member = |inside: Option<OpenQuote>| Spelling {
            inside,
            member: true,
            ..Spelling::default()
        };
        ([None, Some(OpenQuote::Double)].into_iter())
            .try_for_each(|inside| read_back(&world, &text, member(inside)))
            .or_else(fail)
    });
}

/// The hand-picked texts that the quoting rules single out, in every slot and spelling.
#[test]
fn quote_reads_back_the_tricky_texts() {
    let world = &spelling_world();
    let tricky = ["", "=", "&&", "...", "~", "~x", "o>", "2>&1", "-", "\u{0}"];
    let failures: Vec<String> = (WEIRD.iter().chain(&tricky))
        .flat_map(|text| spellings().filter_map(move |how| read_back(world, text, how).err()))
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---- narrowing and the cache -------------------------------------------------------------------

/// Words the narrowing property draws prefixes from.
#[rustfmt::skip]
const VOCABULARY: &[&str] = &[
    "alpha", "alphabet", "beta", "gamma", "delta.txt", "delta.rs", "ünï", "plain.txt", "src",
    "with space.txt", "apple", "apricot", "banana", "ls", "let", "str", "std", "fake-cmd", "help",
    "--help", "--raw", "PATH", "config", "completions",
];

/// Sites to complete at: the text before the token. Whether the site completes file names
/// decides the world it is asked in: elsewhere the world has no files, because a site that matches
/// nothing falls back to file names (`known_bug_narrowing_falls_back_to_files` in
/// `cases/regressions.nuon`), and typing more is then allowed to add candidates.
#[rustfmt::skip]
const SITES: &[(bool, &str)] = &[
    (true, "open "), (true, "cd "), (true, "ls "), (false, ""), (false, "ls --"), (false, "$"),
    (false, "$env."), (false, "t "), (false, "ls | where "),
];

/// A site of [`SITES`] and the text typed at it so far: a prefix of a word, or letters. After `$`
/// or `.` the next `.` starts a cell path, a different site, so the text stops before it.
fn site_and_typed() -> impl Strategy<Value = ((bool, &'static str), String)> {
    let typed = prop_oneof![
        3 => select(VOCABULARY).prop_flat_map(|word| {
            let chars: Vec<char> = word.chars().collect();
            // What can be typed as one bare word.
            (0..=chars.len())
                .prop_map(move |k| chars[..k].iter().copied().take_while(|c| types("", *c)).collect::<String>())
        }),
        1 => "[a-z_.]{0,6}",
    ];
    (select(SITES), typed).prop_map(|((files, site), typed)| {
        let typed = match site.ends_with(['$', '.']) {
            true => (typed.chars())
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect(),
            false => typed,
        };
        ((files, site), typed)
    })
}

/// Typing one more character never adds a candidate, a completer asked twice answers the same,
/// and one that has answered the shorter prefixes agrees with a fresh one.
#[test]
fn narrowing_is_monotone_and_cache_coherent() {
    #[rustfmt::skip]
    let files = ["alpha", "alphabet.txt", "beta", "gamma.nu", "delta.txt", "delta.rs", "ünï.txt",
        "plain.txt", "with space.txt", "src/main.rs", "src/mod.rs"];
    let mut with_files = World::new(&files).expect("ordinary names");
    let mut without_files = World::new(&[]).expect("an empty directory");
    for world in [&mut with_files, &mut without_files] {
        world.run(COMP).expect("definitions");
    }
    let worlds = [without_files, with_files];
    check(32, site_and_typed(), |((files, site), typed)| {
        let world = &worlds[usize::from(files)];
        let mut used = world.session();
        let mut previous: Option<(String, HashSet<String>)> = None;
        for end in typed.char_indices().map(|(i, _)| i).chain([typed.len()]) {
            let line = format!("{site}{}", &typed[..end]);
            let answer = used.complete(&line, line.len());
            let again = used.complete(&line, line.len());
            if again != answer {
                return fail(format!(
                    "{line:?}: asking twice differs: {answer:?} vs {again:?}"
                ));
            }
            let fresh = world.session().complete(&line, line.len());
            if fresh != answer {
                return fail(format!(
                    "{line:?}: after {previous:?} the completer says {answer:?}, a fresh one {fresh:?}"
                ));
            }
            // A candidate offered as a replacement for what was typed (a typo correction) is not
            // a match that typing more could narrow.
            let now: HashSet<String> = (answer.iter())
                .filter(|c| end == 0 || c.match_indices.as_deref() != Some(&[]))
                .map(|c| c.value.clone())
                .collect();
            // A word with a leading `-` is a flag, a minus sign or a negative number, depending on
            // the site and on what follows, and a word of three or more dots names a directory
            // further up (`...` is `../..`): no law relates either to its shorter prefixes, only
            // to what a fresh completer says.
            let another_site = |l: &str| {
                let word = &l[site.len()..];
                let last = word.rsplit(['/', '\\']).next().unwrap_or_default();
                word.starts_with('-') || (last.len() >= 3 && last.chars().all(|c| c == '.'))
            };
            if let Some((before_line, before)) = &previous
                && !another_site(before_line)
                && !another_site(&line)
            {
                let added: Vec<_> = now.difference(before).collect();
                if !added.is_empty() {
                    return fail(format!("{line:?} adds {added:?} to {before_line:?}"));
                }
            }
            previous = Some((line, now));
        }
        Ok(())
    });
}

/// With the cursor at the end of the line, the editor completes from the parse the highlighter
/// makes of the line, painted before or after the request. The answer is that of a completer
/// that parses for itself, for any line and cursor.
#[test]
fn completing_from_the_highlighters_parse_gives_the_same_answer() {
    let world = grammar_world();
    let strategy = (line_and_cursor(), any::<bool>());
    check(64, strategy, |((line, cursor), painted_first)| {
        let alone = world.session().complete(&line, cursor);
        let mut sharing = Session::sharing_the_highlighters_parse(&world.engine, &world.stack);
        if painted_first {
            sharing.paint(&line, cursor);
        }
        let shared = sharing.complete(&line, cursor);
        // And once more, from the parse the first request left in the cache.
        let again = sharing.complete(&line, cursor);
        if shared != alone || again != alone {
            return fail(format!(
                "{line:?} at {cursor} (painted first: {painted_first}): alone {alone:?}, sharing {shared:?}, then {again:?}"
            ));
        }
        Ok(())
    });
}

// ---- the cache gives what a fresh completer gives ----------------------------------------------

/// One step of an editing session: a change to the line, or to the disk under it.
#[derive(Debug, Clone)]
enum Step {
    /// Type a character at the end of the token.
    Type(char),
    /// Delete the last character of the token.
    Backspace,
    /// Delete the whole token.
    Clear,
    /// Make a file (or a directory, with a trailing `/`) in the working directory.
    AddFile(&'static str),
    /// Put an executable on `PATH`.
    AddExecutable(&'static str),
}

/// A step of an editing session, mostly typing.
fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        8 => select(&['a', 'b', 'e', 'z', 't', 'r', '.', '/', '-', 's', 'ü'][..]).prop_map(Step::Type),
        4 => Just(Step::Backspace),
        1 => Just(Step::Clear),
        1 => select(&["alpine.txt", "betamax", "src/new.rs", "zeta.txt", "zoo/"][..]).prop_map(Step::AddFile),
        1 => select(&["zeta-new", "zz-top", "alp-exec"][..]).prop_map(Step::AddExecutable),
    ]
}

/// A completer that has answered before, and was kept through edits to the line and to the disk,
/// answers as a fresh one does: what it remembers of `PATH`, of directories and of custom
/// completers never shows.
#[test]
fn a_cached_answer_equals_a_fresh_one_through_edits() {
    #[rustfmt::skip]
    const SITES: &[&str] = &["", "t ", "ls ", "ls src/", "cd ", "open ", "echo (t ", "ls | where "];
    let strategy = (select(SITES), prop::collection::vec(step(), 1..24));
    check(24, strategy, |(site, steps)| {
        #[rustfmt::skip]
        let files = ["alpha", "alphabet.txt", "beta", "gamma.nu", "src/main.rs", "src/mod.rs",
            "ünï.txt"];
        let mut world = World::new(&files)
            .map_err(|_| TestCaseError::reject("the filesystem refuses a name"))?;
        (world.set_path_executables(&["ztool", "zeta-run", "zshell"])).or_else(fail)?;
        world.run(COMP).or_else(fail)?;
        world.age_directories().or_else(fail)?;

        // Long-lived: it keeps what it learns from one step to the next.
        let mut used = world.session();
        let mut token = String::new();
        for step in steps {
            match step {
                Step::Type(c) => token.push(c),
                Step::Backspace => _ = token.pop(),
                Step::Clear => token.clear(),
                // A name that already exists is refused, which is no change.
                Step::AddFile(name) => drop(world.add(name)),
                Step::AddExecutable(name) => world.add_path_executable(name).or_else(fail)?,
            }
            let line = format!("{site}{token}");
            let answer = used.complete(&line, line.len());
            let fresh = world.session().complete(&line, line.len());
            if answer != fresh {
                return fail(format!(
                    "{line:?} after {step:?}: the completer that remembers says {answer:?}, a fresh one {fresh:?}"
                ));
            }
        }
        Ok(())
    });
}

/// What history says may reorder the candidates, never choose them: a completer that has read a
/// history offers the same candidates as one that has not, and the same spans.
#[test]
fn the_history_boost_orders_but_never_selects() {
    #[rustfmt::skip]
    const HISTORY: &[&str] = &["t apple", "t apricot", "t banana", "open alpha", "open beta",
        "cd beta", "alpha-run", "alpha-sun", "ls alphabet.txt", "str trim", "cargo build"];
    let mut world = World::new(&["alpha", "alphabet.txt", "beta", "gamma.nu"]).expect("names");
    (world.run(&format!(
        "{COMP}\ndef alpha-run [] {{}}\ndef alpha-sun [] {{}}"
    )))
    .expect("definitions");
    let strategy = (
        prop::collection::vec(select(HISTORY), 0..40),
        site_and_typed(),
    );
    check(32, strategy, |(history, ((_, site), typed))| {
        let line = format!("{site}{typed}");
        let mut lines = reedline::FileBackedHistory::default();
        for entry in history {
            (reedline::History::save(&mut lines, reedline::HistoryItem::from_command_line(entry)))
                .or_else(|error| fail(error.to_string()))?;
        }
        let mut learned = world.session();
        learned.learn_usage(&lines);
        // What is offered, over which span, matching where: in any order.
        let key = |candidates: &[Candidate]| {
            let mut keys: Vec<_> = (candidates.iter())
                .map(|c| {
                    (
                        c.value.clone(),
                        c.span.start,
                        c.span.end,
                        c.match_indices.clone(),
                    )
                })
                .collect();
            keys.sort();
            keys
        };
        let boosted = learned.complete(&line, line.len());
        let plain = world.session().complete(&line, line.len());
        if key(&boosted) != key(&plain) {
            return fail(format!(
                "{line:?}: the history changed what is offered: {plain:?} vs {boosted:?}"
            ));
        }
        Ok(())
    });
}

// ---- ranking laws ------------------------------------------------------------------------------

/// Distinct lowercase names over a small alphabet, so that fuzzy matches overlap; what is typed
/// (a few letters, or the start of one of the names); and whether a completer offers the names.
fn names_and_typed() -> impl Strategy<Value = (Vec<String>, String, bool)> {
    prop::collection::btree_set("[a-d][a-d_.-]{1,6}", 2..9).prop_flat_map(|set| {
        let names: Vec<String> = set.into_iter().collect();
        let own = names.clone();
        let typed = prop_oneof![
            1 => "[a-d][a-d_.-]{0,3}".boxed(),
            2 => (0..own.len(), 1usize..8)
                .prop_map(move |(i, k)| own[i].chars().take(k).collect::<String>())
                .boxed(),
        ];
        (Just(names), typed, any::<bool>())
    })
}

/// With the `fuzzy` algorithm, for files and for a custom completer alike: the order is
/// deterministic, an exact match is first, and every prefix match comes before every match that
/// is only fuzzy.
#[test]
fn fuzzy_ranking_laws() {
    check(32, names_and_typed(), |(names, typed, via_completer)| {
        // The names are files to `open`, or what a custom completer returns.
        let entries: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut world = World::new(if via_completer { &[] } else { &entries })
            .map_err(|_| TestCaseError::reject("the filesystem refuses a name"))?;
        let quoted: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
        (world.run(&format!(
            "$env.config.completions.algorithm = \"fuzzy\"\ndef comp [] {{ [{}] }}\ndef t [x: string@comp] {{}}",
            quoted.join(" ")
        )))
        .or_else(fail)?;
        let line = format!("{} {typed}", if via_completer { "t" } else { "open" });
        let first = world.complete(&line, line.len());
        // Only the candidates for the argument: with `fuzzy`, `t ` also offers the commands
        // `to csv`, ... as longer spellings of the command word, over a different span.
        let argument = line.len() - typed.len();
        let shown: Vec<&str> = (first.iter())
            .filter(|c| c.span.start == argument)
            .map(|c| c.display.as_str())
            .collect();

        let second = world.complete(&line, line.len());
        if first != second {
            return fail(format!(
                "{line:?} is not deterministic: {first:?} vs {second:?}"
            ));
        }
        if names.contains(&typed) && shown.first() != Some(&typed.as_str()) {
            return fail(format!("{line:?}: the exact match is not first: {shown:?}"));
        }
        let first_fuzzy = shown.iter().position(|d| !d.starts_with(&typed));
        let last_prefix = shown.iter().rposition(|d| d.starts_with(&typed));
        if let (Some(fuzzy), Some(prefix)) = (first_fuzzy, last_prefix)
            && fuzzy < prefix
        {
            return fail(format!(
                "{line:?}: fuzzy-only {:?} is above prefix match {:?}: {shown:?}",
                shown[fuzzy], shown[prefix]
            ));
        }
        Ok(())
    });
}
