//! What taking a suggestion does to the line beyond putting its value in: how much of the word
//! at the cursor it replaces, whether a space follows it, and the closure a command that takes
//! only a closure opens.

use super::{HelpFor, SemanticSuggestion};
use nu_parser::parse_probed;
use nu_protocol::{
    DeclId, SuggestionKind, SyntaxShape, Type,
    engine::{EngineState, Expect, StateWorkingSet},
};
use std::path::is_separator;

/// Characters that end a word on the line, whatever is being completed.
const WORD_ENDS: &[char] = &[
    '|', ';', '(', ')', '{', '}', '[', ']', ',', '\'', '"', '`', '=', ':', '@',
];

/// How far the word `item` completes goes on past the cursor, in `after`, the text that follows
/// the cursor on its line. `replaced` is the part of the word before the cursor.
///
/// A suggestion replaces the whole word, not only what is before the cursor: taking `size` at
/// `si⌶ze` makes `size`, not `sizeze`. The word is a name, a path up to its next separator, a
/// cell path's member, or a quoted string to its closing quote. A character that ends the word
/// is replaced too when the suggestion ends with it, as `src/` does `/`.
pub(crate) fn rest_of_word(after: &str, replaced: &str, item: &SemanticSuggestion) -> usize {
    if let Some(quote) = replaced
        .chars()
        .next()
        .filter(|c| matches!(c, '\'' | '"' | '`'))
    {
        let open = !replaced[quote.len_utf8()..].contains(quote);
        return match after.find(quote) {
            Some(at) if open && !after[..at].contains('\n') => at + quote.len_utf8(),
            _ => 0,
        };
    }
    let path = matches!(
        item.kind,
        Some(SuggestionKind::File | SuggestionKind::Directory)
    );
    let member = matches!(item.kind, Some(SuggestionKind::CellPath));
    let value = &item.suggestion.value;
    for (at, c) in after.char_indices() {
        let ends = c.is_whitespace()
            || WORD_ENDS.contains(&c)
            || (path && is_separator(c))
            || (member && matches!(c, '.' | '?' | '!'));
        if ends {
            return if value.ends_with(c) && !c.is_whitespace() {
                at + c.len_utf8()
            } else {
                at
            };
        }
    }
    after.len()
}

/// Whether a value ends an argument, so that a space should follow it once it is taken: not when
/// it ends in a character that more is written after, as a directory's `/`, `user@`, `key=` or
/// `$env.` do.
pub(crate) fn ends_an_argument(value: &str, kind: Option<&SuggestionKind>) -> bool {
    if matches!(kind, Some(SuggestionKind::Directory)) {
        return false;
    }
    !value.ends_with(|c: char| {
        is_separator(c) || matches!(c, '@' | '=' | ':' | '.' | ',' | '(' | '[' | '{' | '$')
    })
}

/// Whether a value of type `ty` may go on with a cell path, as a record's or a table's does:
/// then no space follows it, for a `.` to be typed. A list of plain values, such as `PATH`, is
/// piped on more often than indexed.
pub(crate) fn has_members(ty: &Type) -> bool {
    match ty {
        Type::List(inner) => has_members(inner),
        ty => matches!(
            ty,
            Type::Record(_) | Type::Table(_) | Type::Any | Type::Custom(_)
        ),
    }
}

/// The closure `item` opens once taken, as text to put after its value and where the cursor goes
/// in it, when it is a command that takes only a closure (`each`) or a flag that takes one: a
/// body to write, ` { ⌶ }`, or ` {|key, value| ⌶ }` for a closure whose parameters must be named,
/// named as the command's examples name them. `None` for anything else, and when `follows`, the
/// rest of the line, already goes on with the command's arguments.
pub(crate) fn closure_opening(
    item: &SemanticSuggestion,
    engine_state: &EngineState,
    follows: &str,
) -> Option<(String, usize)> {
    let next = follows.trim_start().chars().next();
    if !next.is_none_or(|c| matches!(c, '|' | ')' | '}' | ';')) {
        return None;
    }
    // A command declared on the line itself is not the engine's yet, and is left as it is.
    let known = |decl_id: DeclId| decl_id.get() < engine_state.num_decls();
    let (decl_id, shape) = match &item.kind {
        Some(SuggestionKind::Command(_, Some(decl_id))) if known(*decl_id) => {
            let signature = engine_state.get_decl(*decl_id).signature();
            match &*signature.required_positional {
                [only] => (*decl_id, only.shape.clone()),
                _ => return None,
            }
        }
        Some(SuggestionKind::Flag) => {
            let HelpFor::Decl(decl_id) = item.help_at.as_ref()?.command else {
                return None;
            };
            if !known(decl_id) {
                return None;
            }
            let signature = engine_state.get_decl(decl_id).signature();
            let name = item.suggestion.value.trim_start_matches('-');
            let flag = signature.named.iter().find(|flag| {
                flag.long == name || flag.short.is_some_and(|short| name == short.to_string())
            })?;
            (decl_id, flag.arg.clone()?)
        }
        _ => return None,
    };
    let (text, cursor) = opening_of(&shape, decl_id, engine_state)?;
    Some((format!(" {text}"), cursor + 1))
}

/// How a closure or block of `shape`, an argument of the command `decl_id`, is opened: the text
/// and where the cursor goes in it. A body to write, `{ ⌶ }`, or `{|key, value| ⌶ }` for a closure
/// whose parameters must be named, named as the command's examples name them.
fn opening_of(
    shape: &SyntaxShape,
    decl_id: DeclId,
    engine_state: &EngineState,
) -> Option<(String, usize)> {
    let params = match shape {
        SyntaxShape::Block => 0,
        SyntaxShape::Closure(params) => params.as_ref().map_or(0, |params| params.len()),
        _ => return None,
    };
    // With one parameter or none the body reads its input as `$in`.
    if params < 2 {
        return Some(("{  }".to_owned(), 2));
    }
    let examples = engine_state.get_decl(decl_id).examples();
    let named = examples.iter().find_map(|example| {
        let (_, after) = example.example.split_once("{|")?;
        let (names, _) = after.split_once('|')?;
        (!names.trim().is_empty() && names.split(',').count() == params)
            .then(|| names.trim().to_owned())
    });
    Some(match named {
        Some(names) => {
            let open = format!("{{|{names}| ");
            let cursor = open.len();
            (format!("{open} }}"), cursor)
        }
        // The parameters are for the user to name first.
        None => ("{|| }".to_owned(), 2),
    })
}

/// What `line` can only go on with now that a space was typed `pos` bytes into it, and where the
/// cursor goes in that: the closure or block a command waits for when no other kind of argument
/// can come there, the parameters of a `def`, or the keyword a statement needs, `= ` of `let x`.
/// `None` for anything that could still go another way, and where the line already goes on.
pub(crate) fn what_must_follow(
    engine_state: &EngineState,
    line: &str,
    pos: usize,
) -> Option<(String, usize)> {
    let before = line.get(..pos)?;
    let goes_on = !line[pos..]
        .trim_start()
        .chars()
        .next()
        .is_none_or(|c| matches!(c, '|' | ')' | '}' | ';'));
    // Only the one space just typed opens, at the end of what is written: a second space, or
    // a space before more of the line, is the user's own.
    let spaced = before.strip_suffix(char::is_whitespace);
    if goes_on
        || before.trim().is_empty()
        || spaced.is_none_or(|rest| rest.ends_with(char::is_whitespace))
    {
        return None;
    }
    let mut working_set = StateWorkingSet::new(engine_state);
    let (_, probe) = parse_probed(&mut working_set, None, before.as_bytes(), pos);
    // Only the next positional of a call is waited for, where nothing else can be written, such
    // as a flag's value; an operator may follow a value, but only a condition is left open.
    let found = (probe.found.iter()).filter(|found| !matches!(found.what, Expect::Operator { .. }));
    let narrowest = found.clone().map(|found| found.within.len()).min();
    let mut found = found.filter(|found| {
        Some(found.within.len()) == narrowest
            && !matches!(
                found.what,
                Expect::Flag { .. } | Expect::Command { parent: Some(_) }
            )
    });
    // The slot is empty at the cursor: the cursor is not inside an argument already begun, such
    // as the closure `each { |row| ⌶` writes.
    let (Some(found), None) = (found.next(), found.next()) else {
        return None;
    };
    if found.span.start < probe.at {
        return None;
    }
    let Expect::Positional {
        decl_id,
        index,
        shape,
    } = &found.what
    else {
        return None;
    };
    let signature = working_set.get_decl(*decl_id).signature();
    // A statement's `=` is optional to the signature, though never to what is meant.
    let statement = matches!(signature.name.as_str(), "let" | "mut" | "const");
    // An argument that goes on through operators may not be over.
    let open_ended = |shape: &SyntaxShape| match shape {
        SyntaxShape::MathExpression | SyntaxShape::RowCondition => true,
        SyntaxShape::Keyword(_, inner) => matches!(**inner, SyntaxShape::MathExpression),
        _ => false,
    };
    if (*index >= signature.required_positional.len() && !statement)
        || (signature.required_positional.iter().take(*index)).any(|arg| open_ended(&arg.shape))
    {
        return None;
    }
    match shape {
        SyntaxShape::Keyword(keyword, _) => {
            let keyword = String::from_utf8_lossy(keyword);
            Some((format!("{keyword} "), keyword.len() + 1))
        }
        SyntaxShape::Signature => Some(("[]".to_owned(), 1)),
        SyntaxShape::MatchBlock => Some(("{  }".to_owned(), 2)),
        shape if decl_id.get() < engine_state.num_decls() => {
            opening_of(shape, *decl_id, engine_state)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::Suggestion;

    fn item(value: &str, kind: SuggestionKind) -> SemanticSuggestion {
        SemanticSuggestion {
            suggestion: Suggestion {
                value: value.to_owned(),
                ..Suggestion::default()
            },
            kind: Some(kind),
            ..SemanticSuggestion::default()
        }
    }

    #[test]
    fn the_rest_of_the_word_is_replaced_too() {
        let column = item("size", SuggestionKind::CellPath);
        assert_eq!(rest_of_word("ze > 1k", "si", &column), 2);
        assert_eq!(
            rest_of_word("ze.b", "si", &column),
            2,
            "a member ends at `.`"
        );

        let branch = item(
            "origin/main",
            SuggestionKind::Value(nu_protocol::Type::String),
        );
        assert_eq!(rest_of_word("in/x | y", "origin/ma", &branch), 4);

        let file = item("src/main.rs", SuggestionKind::File);
        assert_eq!(rest_of_word("in.rs", "src/ma", &file), 5);
        assert_eq!(rest_of_word("in/lib.rs", "src/ma", &file), 2);
        let dir = item("src/main/", SuggestionKind::Directory);
        assert_eq!(
            rest_of_word("in/lib.rs", "src/ma", &dir),
            3,
            "a directory takes the separator it ends with"
        );

        let quoted = item("'my file.txt'", SuggestionKind::File);
        assert_eq!(rest_of_word("le.txt' x", "'my fi", &quoted), 7);
        assert_eq!(rest_of_word("", "", &column), 0);
    }

    #[test]
    fn a_space_follows_only_a_finished_argument() {
        let string = SuggestionKind::Value(nu_protocol::Type::String);
        assert!(ends_an_argument("rethink/hints", Some(&string)));
        assert!(ends_an_argument("Cargo.toml", Some(&SuggestionKind::File)));
        assert!(!ends_an_argument(
            "crates/",
            Some(&SuggestionKind::Directory)
        ));
        assert!(!ends_an_argument("root@", Some(&string)));
        assert!(!ends_an_argument("PATH=", Some(&string)));
    }
}
