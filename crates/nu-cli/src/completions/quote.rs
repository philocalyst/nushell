//! Spelling a completion as a Nushell token.
//!
//! Every completion that inserts user data (a file name, a command name, a cell-path member, a
//! module item) goes through [`quote`], which returns text that the parser reads back as exactly
//! the input. Whether a bare word is safe the parser itself says ([`would_misread`]). What the
//! quotes do is fixed: only `'...'` and `"..."` keep a path slot from expanding `~`, globs and
//! `...`; only `"..."` has escapes; single quotes and backticks cannot hold their own delimiter.

use nu_path::expand_tilde;
use nu_protocol::{
    ast::Expr,
    engine::{EngineState, StateWorkingSet},
};
use std::{borrow::Cow, fmt::Write as _, path::is_separator};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

/// The three quote characters Nushell strings use.
pub const QUOTES: [char; 3] = ['"', '\'', '`'];

/// A kind of quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quote {
    /// `"..."`: the only kind with escapes, so the only one that can hold anything.
    Double,
    /// `'...'`: verbatim, and the delimiter of a literal that a glob slot does not expand.
    Single,
    /// `` `...` ``: a bare word that may hold spaces, verbatim, but still expanded by path slots.
    Backtick,
}

impl Quote {
    /// The quote that opens `typed`, if it opens one: the user is inside it while typing.
    pub fn opened_by(typed: &str) -> Option<Self> {
        match typed.chars().next()? {
            '"' => Some(Self::Double),
            '\'' => Some(Self::Single),
            '`' => Some(Self::Backtick),
            _ => None,
        }
    }
}

/// How the token is going to be read.
#[derive(Clone, Copy, Debug, Default)]
pub struct Spelling {
    /// The slot expands globs (a file or glob argument): glob characters must be written in
    /// `'...'` or `"..."`, which the parser keeps literal. Backticks do not.
    pub glob: bool,
    /// The user has opened this quote and is typing inside it: when the text needs quoting, stay in
    /// that quote if it can hold the text. A word that is safe bare is still returned bare.
    pub inside: Option<Quote>,
    /// The text starts with `~/` (or is `~`) and means the home directory, as typed. A bare word or
    /// a backtick word keeps the `~`; a form that cannot expand it gets the home directory spelled
    /// out instead.
    pub home: bool,
    /// The token is a cell-path member: a `.`, `?` or `!` would split it, and a number is an index.
    /// It is bare or in double quotes, the way members are written.
    pub member: bool,
}

/// Spells `text` as a Nushell token that parses back to exactly `text`.
///
/// Chooses the shortest correct form: the bare word when it is safe, else backticks, else single
/// quotes, else double quotes with escapes (see [`Spelling`] for the exceptions). The result is
/// borrowed when the bare word is used.
pub fn quote(text: &str, how: Spelling) -> Cow<'_, str> {
    let home = how.home && is_home_path(text);
    let plain = !text.chars().any(char::is_control);
    // A path slot expands a bare word and a backtick word: a glob pattern matches files, `~` is
    // the home directory and a run of dots is `../..`. Only real quotes keep them literal.
    let expanded_by_slot = (how.glob && nu_glob::is_glob_with_backend(text))
        || (text.starts_with('~') && !home)
        || is_dots(text);

    let double = true;
    let single = plain && !text.contains('\'');
    let backtick = plain && !text.is_empty() && !text.contains('`') && !expanded_by_slot;
    let bare = plain && !expanded_by_slot && !would_misread(text, how.member, home);

    let fits = |quote: Quote| match quote {
        Quote::Double => double,
        Quote::Single => single,
        Quote::Backtick => backtick,
    };
    let form = match how.inside {
        _ if bare => return Cow::Borrowed(text),
        Some(quote) if fits(quote) => quote,
        // A cell-path member is written in double quotes, as `$x."a b"` always was.
        _ if backtick && !how.member => Quote::Backtick,
        _ if single && !how.member => Quote::Single,
        _ => Quote::Double,
    };

    // A real quote does not expand `~`: spell the home directory out.
    if home && form != Quote::Backtick {
        let spelled_out = expand_tilde(text).to_string_lossy().into_owned();
        return Cow::Owned(quote(&spelled_out, Spelling { home: false, ..how }).into_owned());
    }
    Cow::Owned(wrap(text, form))
}

/// `text` inside `quote`, escaped when the quote has escapes.
fn wrap(text: &str, quote: Quote) -> String {
    match quote {
        Quote::Single => format!("'{text}'"),
        Quote::Backtick => format!("`{text}`"),
        Quote::Double => {
            let mut out = String::with_capacity(text.len() + 2);
            out.push('"');
            for c in text.chars() {
                match c {
                    '"' | '\\' => {
                        out.push('\\');
                        out.push(c);
                    }
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c.is_control() => {
                        // Writing to a `String` cannot fail.
                        let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
    }
}

/// Whether `text` is three or more dots: `...` is the grandparent directory in a path, and every
/// longer run is one more level.
fn is_dots(text: &str) -> bool {
    text.len() >= 3 && text.bytes().all(|b| b == b'.')
}

/// Whether `text` is `~` or starts with `~` and a separator: the home directory.
fn is_home_path(text: &str) -> bool {
    text.strip_prefix('~')
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(is_separator))
}

/// Whether a bare word `text` would parse as something other than the string `text`, or as more
/// than one token. This is the one predicate behind "bare when safe": the parser is asked.
///
/// `home` allows a leading `~` (the caller means the home directory); `member` is for a cell-path
/// member, which is bare only if it is made of letters, digits, `_` and `-`.
pub fn would_misread(text: &str, member: bool, home: bool) -> bool {
    let Some(first) = text.chars().next() else {
        return true;
    };
    // What the line reads before the value: a flag or a negative number, an external call, a
    // builtin, an attribute, and in a path slot the home directory or the levels of `...`.
    if matches!(first, '-' | '^' | '%' | '@') || (first == '~' && !home) || is_dots(text) {
        return true;
    }
    if member
        && !text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return true;
    }
    !reads_as_itself(text)
}

/// Whether the parser reads the word `text` as exactly the string `text` where a value is read
/// most strictly, as the operand of a comparison: one word, and not an operator, a number, a
/// date, a range, a filesize or duration, a bool or null, a variable or a subexpression. Each
/// word is asked once.
fn reads_as_itself(text: &str) -> bool {
    static PARSER: LazyLock<EngineState> = LazyLock::new(EngineState::new);
    static READ: LazyLock<Mutex<HashMap<String, bool>>> = LazyLock::new(Mutex::default);
    let lock = || READ.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(&read) = lock().get(text) {
        return read;
    }
    let line = format!("'' == {text}");
    let mut working_set = StateWorkingSet::new(&PARSER);
    let block = nu_parser::parse(&mut working_set, None, line.as_bytes(), false);
    let read = working_set.parse_errors.is_empty()
        && matches!(block.pipelines.as_slice(), [pipeline]
            if matches!(pipeline.elements.as_slice(), [element]
                if matches!(&element.expr.expr, Expr::BinaryOp(_, _, rhs)
                    if matches!(&rhs.expr, Expr::String(read) if read == text))));
    let mut known = lock();
    if known.len() >= 10_000 {
        known.clear();
    }
    known.insert(text.to_owned(), read);
    read
}

#[cfg(test)]
mod tests {
    use super::*;
    use nu_parser::{TokenContents, lex};
    use rstest::rstest;

    fn spell(text: &str) -> String {
        quote(text, Spelling::default()).into_owned()
    }

    fn glob(text: &str) -> String {
        quote(
            text,
            Spelling {
                glob: true,
                ..Spelling::default()
            },
        )
        .into_owned()
    }

    #[rstest]
    #[case::plain("plain.txt", "plain.txt")]
    #[case::hidden(".hidden", ".hidden")]
    #[case::parent("../src/lib.rs", "../src/lib.rs")]
    #[case::version("a-1.2.3.tar.gz", "a-1.2.3.tar.gz")]
    #[case::digit_name("1.txt", "1.txt")]
    #[case::dated("2024-report.pdf", "2024-report.pdf")]
    #[case::date_log("2024-01-15.log", "2024-01-15.log")]
    #[case::backslash("a\\b", "a\\b")]
    #[case::unicode("ünï😀", "ünï😀")]
    #[case::space("a b", "`a b`")]
    #[case::hash("a#b", "a#b")]
    #[case::paren("a(b)", "`a(b)`")]
    #[case::semicolon("a;b", "`a;b`")]
    #[case::pipe("a|b", "`a|b`")]
    #[case::single_quote("it's", "`it's`")]
    #[case::double_quote("a\"b", "`a\"b`")]
    #[case::backtick_and_single("a`b'c", "\"a`b'c\"")]
    #[case::backtick("a`b", "'a`b'")]
    #[case::empty("", "''")]
    #[case::number("123", "`123`")]
    #[case::float("1.5", "`1.5`")]
    #[case::exponent("1e3", "`1e3`")]
    #[case::hex("0x1f", "`0x1f`")]
    #[case::filesize("1kb", "`1kb`")]
    #[case::duration("5sec", "`5sec`")]
    #[case::date("2024-01-15", "`2024-01-15`")]
    #[case::range("1..2", "`1..2`")]
    #[case::infinity("inf", "`inf`")]
    #[case::boolean("true", "`true`")]
    #[case::null("null", "`null`")]
    #[case::equals("=", "`=`")]
    #[case::and_and("&&", "`&&`")]
    #[case::greater(">", ">")]
    #[case::flag("-dash", "`-dash`")]
    #[case::dollar("$x", "`$x`")]
    #[case::tilde("~tilde", "'~tilde'")]
    #[case::caret("^ls", "`^ls`")]
    #[case::percent("%x", "`%x`")]
    #[case::at("@x", "`@x`")]
    #[case::dots("...", "'...'")]
    #[case::tab("a\tb", "\"a\\tb\"")]
    #[case::newline("a\nb", "\"a\\nb\"")]
    #[case::carriage_return("a\rb", "\"a\\rb\"")]
    #[case::nul("a\0b", "\"a\\u{0}b\"")]
    #[case::escape_char("a\u{1b}b", "\"a\\u{1b}b\"")]
    #[case::control_with_quote("\"\t\\", "\"\\\"\\t\\\\\"")]
    fn the_shortest_correct_form(#[case] text: &str, #[case] expected: &str) {
        assert_eq!(spell(text), expected);
    }

    #[rstest]
    #[case::glob_star("a*b", "'a*b'")]
    #[case::glob_bracket("[a] b", "'[a] b'")]
    #[case::glob_question("q?", "'q?'")]
    #[case::glob_with_single_quote("it's*", "\"it's*\"")]
    #[case::glob_with_both("it's`*", "\"it's`*\"")]
    #[case::glob_with_tab("*\t", "\"*\\t\"")]
    #[case::no_glob_stays_short("a b", "`a b`")]
    #[case::curly_is_not_a_glob("{a,b}", "`{a,b}`")]
    fn glob_slots_use_real_quotes(#[case] text: &str, #[case] expected: &str) {
        assert_eq!(glob(text), expected);
    }

    #[rstest]
    #[case::bare_text_stays_bare(Quote::Double, "plain", "plain")]
    #[case::spaced_text_stays_in_double(Quote::Double, "a b", "\"a b\"")]
    #[case::double_escapes(Quote::Double, "a\"b", "\"a\\\"b\"")]
    #[case::stay_in_single(Quote::Single, "a b", "'a b'")]
    #[case::single_cannot_hold_single(Quote::Single, "it's", "`it's`")]
    #[case::stay_in_backtick(Quote::Backtick, "a b", "`a b`")]
    #[case::backtick_cannot_hold_backtick(Quote::Backtick, "a`b", "'a`b'")]
    #[case::backtick_cannot_hold_control(Quote::Backtick, "a\tb", "\"a\\tb\"")]
    fn an_open_quote_is_kept_when_it_can_hold_the_text(
        #[case] inside: Quote,
        #[case] text: &str,
        #[case] expected: &str,
    ) {
        let how = Spelling {
            inside: Some(inside),
            ..Spelling::default()
        };
        assert_eq!(quote(text, how), expected);
    }

    #[test]
    fn a_glob_slot_leaves_an_open_backtick() {
        let how = Spelling {
            glob: true,
            inside: Some(Quote::Backtick),
            ..Spelling::default()
        };
        assert_eq!(quote("a*", how), "'a*'");
    }

    #[test]
    fn the_home_directory_keeps_its_tilde_unless_a_real_quote_is_needed() {
        let home = Spelling {
            home: true,
            glob: true,
            ..Spelling::default()
        };
        assert_eq!(quote("~/plain", home), "~/plain");
        assert_eq!(quote("~/with space", home), "`~/with space`");
        let expanded = expand_tilde("~/a*").to_string_lossy().into_owned();
        assert_eq!(quote("~/a*", home), format!("'{expanded}'"));
        // Not meant as the home directory: literal.
        assert_eq!(quote("~x", home), "'~x'");
    }

    #[rstest]
    #[case("name", "name")]
    #[case("my-key_2", "my-key_2")]
    #[case("é", "é")]
    #[case("1", "\"1\"")]
    #[case("a b", "\"a b\"")]
    #[case("a.b", "\"a.b\"")]
    #[case("a?", "\"a?\"")]
    #[case("", "\"\"")]
    fn cell_path_members(#[case] member: &str, #[case] expected: &str) {
        let how = Spelling {
            member: true,
            ..Spelling::default()
        };
        assert_eq!(quote(member, how), expected);
    }

    /// The bare words the predicate lets through are one lexer item, and the words it refuses
    /// include every word the lexer splits or turns into an operator.
    #[test]
    fn bare_words_are_one_lexer_item() {
        let samples = [
            "plain.txt",
            ".hidden",
            "a=b",
            "a<b",
            "a>b",
            "a,b",
            "a:b",
            "a&b",
            "a*b",
            "a?b",
            "100%",
            "a\\b",
            "e\u{301}",
            "ünï",
            "a!",
            "a@b",
            "a^b",
            "a~b",
            "../x",
            "..",
            ".",
            "1.txt",
            "a$b",
        ];
        for sample in samples {
            assert!(!would_misread(sample, false, false), "{sample:?} is safe");
            let (tokens, error) = lex(sample.as_bytes(), 0, &[], &[], false);
            assert!(error.is_none(), "{sample:?}: {error:?}");
            assert_eq!(tokens.len(), 1, "{sample:?} lexes as {tokens:?}");
            assert_eq!(tokens[0].contents, TokenContents::Item, "{sample:?}");
        }
        for word in [
            "=", "+=", "&&", "o>", "e>|", "a b", "a|b", "a;b", "a(b", "a'b", "-",
        ] {
            assert!(would_misread(word, false, false), "{word:?} is unsafe");
        }
    }

    #[test]
    fn the_open_quote_is_read_from_what_was_typed() {
        assert_eq!(Quote::opened_by("\"ab"), Some(Quote::Double));
        assert_eq!(Quote::opened_by("'ab"), Some(Quote::Single));
        assert_eq!(Quote::opened_by("`ab"), Some(Quote::Backtick));
        assert_eq!(Quote::opened_by("ab"), None);
        assert_eq!(Quote::opened_by(""), None);
    }
}
