//! What the shell's own documentation of its configuration says of a setting or an environment
//! variable, for where `$env.config.…` and `$env.…` are completed: the comments that
//! `config nu --doc` and `config env --doc` write above each one.

use nu_config::ConfigFileKind;
use std::{collections::HashMap, sync::OnceLock};

/// The documentation of each setting, by its path after `$env.`: `config.completions.quick`,
/// `PROMPT_COMMAND`.
fn docs() -> &'static HashMap<String, String> {
    static DOCS: OnceLock<HashMap<String, String>> = OnceLock::new();
    DOCS.get_or_init(|| {
        let mut docs = HashMap::new();
        for kind in [ConfigFileKind::Config, ConfigFileKind::Env] {
            // The comment lines right above a line that sets one are its documentation.
            let mut comments: Vec<&str> = Vec::new();
            for line in kind.doc().lines().map(str::trim) {
                if let Some(comment) = line.strip_prefix('#') {
                    comments.push(comment.strip_prefix(' ').unwrap_or(comment));
                    continue;
                }
                let path = line
                    .strip_prefix("$env.")
                    .and_then(|rest| rest.split([' ', '=']).next())
                    .filter(|path| !path.is_empty());
                if let Some(path) = path
                    && !comments.is_empty()
                {
                    docs.entry(path.to_owned())
                        .or_insert_with(|| comments.join("\n").trim().to_owned());
                }
                comments.clear();
            }
        }
        docs
    })
}

/// What the documentation says of the setting at `path` after `$env.`: its first line, without
/// the name and type it starts with (`completions.quick (bool): `), which the completion shows
/// already, and the lines after it.
pub(crate) fn doc(path: &str) -> Option<(String, String)> {
    let doc = docs().get(path)?;
    let (first, rest) = doc.split_once('\n').unwrap_or((doc, ""));
    let first = match first.split_once("): ") {
        Some((named, said))
            if named
                .split_once(" (")
                .is_some_and(|(name, _)| !name.contains(' ')) =>
        {
            said
        }
        _ => first,
    };
    Some((first.trim().to_owned(), rest.trim().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_setting_is_documented_by_the_comments_above_it() {
        let (first, rest) = doc("config.completions.quick").expect("documented");
        assert!(first.starts_with("Controls auto-selection"), "{first}");
        assert!(rest.contains("Default: true"), "{rest}");
        assert!(doc("PROMPT_COMMAND").is_some());
        assert!(doc("config.no_such_setting").is_none());
    }
}
