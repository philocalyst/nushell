//! Placeholders: the words of an example that the user is to replace, written `<name>` as navi
//! writes them (`git checkout -b <branch>`). The name is letters, digits, `_` and `-`, and
//! nothing else makes one, so a type such as `list<int>` or a redirection is never mistaken
//! for a placeholder.

use std::ops::Range;

/// Whether `c` may be in the name of a placeholder.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-')
}

/// The byte ranges of the placeholders in `text`, in order, each from its `<` to its `>`.
pub(crate) fn find(text: &str) -> Vec<Range<usize>> {
    text.match_indices('<')
        .filter_map(|(start, _)| {
            let name = &text[start + 1..];
            let len = name.find('>').filter(|&len| len > 0)?;
            let wholly_a_name = name[..len].chars().all(is_name_char);
            // `list<int>` is a type: a placeholder starts a word.
            let starts_a_word = text[..start]
                .chars()
                .next_back()
                .is_none_or(|before| !before.is_alphanumeric() && before != '_');
            (wholly_a_name && starts_a_word).then_some(start..start + 1 + len + 1)
        })
        .collect()
}

/// The placeholder the cursor is in or at the end of, when the line is not inside a quote at
/// its start: `echo "a <b> c"` holds text, not a placeholder.
pub(crate) fn at(line: &str, cursor: usize) -> Option<Range<usize>> {
    let found = find(line)
        .into_iter()
        .find(|found| (found.start..=found.end).contains(&cursor))?;
    let unquoted = |quote| line[..found.start].matches(quote).count().is_multiple_of(2);
    ['"', '\'', '`'].into_iter().all(unquoted).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placeholder_is_a_word_in_angle_brackets_and_nothing_else() {
        let found = |text: &str| -> Vec<String> {
            find(text)
                .iter()
                .map(|r| text[r.clone()].to_owned())
                .collect()
        };
        assert_eq!(
            found("git checkout -b <branch> <from-ref>"),
            ["<branch>", "<from-ref>"]
        );
        assert_eq!(found("--name=<value>"), ["<value>"]);
        assert!(found("def f [x: list<int>] {}").is_empty());
        assert!(found("a <b c> <> < d>").is_empty());
    }

    #[test]
    fn the_cursor_in_or_at_the_end_of_one_is_at_it() {
        let line = "git checkout -b <branch> now";
        let found = |cursor| at(line, cursor).map(|r| line[r].to_owned());
        assert_eq!(found(16).as_deref(), Some("<branch>"));
        assert_eq!(found(20).as_deref(), Some("<branch>"));
        assert_eq!(found(24).as_deref(), Some("<branch>"));
        assert_eq!(found(15), None);
        assert_eq!(found(25), None);
        // In a string it is text.
        assert_eq!(at("echo \"a <b> c\"", 9), None);
    }
}
