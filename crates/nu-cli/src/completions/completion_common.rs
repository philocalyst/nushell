use super::{
    cache::Entry,
    quote::{Quote, Spelling, quote},
    rank::Score,
};
use crate::completions::{
    Candidates, Context, SemanticSuggestion,
    delegation::{Delegation, Listed},
    to_reedline_span,
    usage::Used,
};
use nu_engine::env_to_string;
use nu_path::dots::expand_ndots;
use nu_path::{expand_to_real_path, home_dir};
use nu_protocol::{Span, SuggestionKind, engine::StateWorkingSet};
use nu_utils::IgnoreCaseExt;
use nu_utils::get_ls_colors;
use reedline::Suggestion;
use std::borrow::Cow;
use std::fmt::Write;
use std::path::{Component, MAIN_SEPARATOR as SEP, Path, PathBuf, is_separator};
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Default)]
pub struct PathBuiltFromString {
    cwd: PathBuf,
    parts: Vec<MatchedPart>,
    isdir: bool,
    /// How the last of `parts` ranked.
    score: Option<Score>,
}

#[derive(Clone, Default)]
pub struct MatchedPart {
    text: String,
    match_indices: Vec<usize>,
}

/// Recursively goes through paths that match a given `partial`.
/// * `built_paths`: State struct for a valid matching path built so far.
/// * `ctx`: The options to match with, and the directory listings the cache holds.
/// * `want_directory`: Whether we want only directories as completion matches.
///   Some commands like `cd` can only be run on directories whereas others
///   like `ls` can be run on regular files as well.
/// * `isdir`: whether the current partial path has a trailing slash.
///   Parsing a path string into a pathbuf loses that bit of information.
/// * `enable_exact_match`: Whether all previous components of the path matched a directory
///   exactly, so that one more exact match narrows to it.
fn complete_rec(
    partial: &[&str],
    built_paths: &[PathBuiltFromString],
    ctx: &Context,
    want_directory: bool,
    isdir: bool,
    enable_exact_match: bool,
) -> Vec<PathBuiltFromString> {
    let has_more = !partial.is_empty() && (partial.len() > 1 || isdir);

    if let Some((&base, rest)) = partial.split_first()
        && base.chars().all(|c| c == '.')
        && has_more
    {
        let built_paths: Vec<_> = built_paths
            .iter()
            .map(|built| {
                let mut built = built.clone();
                built.parts.push(MatchedPart {
                    text: base.to_string(),
                    match_indices: Vec::new(),
                });
                built.isdir = true;
                built
            })
            .collect();
        return complete_rec(
            rest,
            &built_paths,
            ctx,
            want_directory,
            isdir,
            enable_exact_match,
        );
    }

    let options = ctx.options;
    let prefix = partial.first().unwrap_or(&"");

    // What each path built so far holds, and the entries that may match, as indices into it.
    let mut listings: Vec<(&PathBuiltFromString, Arc<[Entry]>)> = Vec::new();
    let mut candidates = Candidates::<(usize, usize)> {
        needle: Some((*prefix).to_owned()),
        // Entries of one directory are ranked by when the user named them or was in them.
        used: Some(Used::Path {
            directories: want_directory,
        }),
        ..Candidates::default()
    };
    for built in built_paths {
        let mut path = built.cwd.clone();
        for part in &built.parts {
            path.push(part.text.as_str());
        }

        let Some(listing) = ctx.cache.listing(&path, ctx.trace) else {
            continue;
        };
        if let [_] = built_paths {
            candidates.beneath = Some(path);
        }
        for (index, entry) in listing.iter().enumerate() {
            if !want_directory || entry.is_dir {
                candidates.push_as(entry.name.clone(), (listings.len(), index));
            }
        }
        listings.push((built, listing));
    }
    let found = candidates.ranked(ctx);
    let entry_of = |&(at, index): &(usize, usize)| (listings[at].0, &listings[at].1[index]);

    // Don't show longer completions if we have a single exact match (#13204, #14794).
    // Only relevant for case insensitive matching: there may be more than one.
    if enable_exact_match && has_more {
        let mut exact = found
            .iter()
            .map(|found| entry_of(&found.item))
            .filter(|(_, entry)| {
                if options.case_sensitive {
                    entry.name == *prefix
                } else {
                    entry.name.eq_ignore_case(prefix)
                }
            });
        if let (Some((built, entry)), None) = (exact.next(), exact.next()) {
            let mut built = built.clone();
            built.isdir = entry.is_dir;
            built.parts.push(MatchedPart {
                text: entry.name.clone(),
                match_indices: (0..entry.name.graphemes(true).count()).collect(),
            });
            return complete_rec(&partial[1..], &[built], ctx, want_directory, isdir, true);
        }
    }

    let completion_iter = found.into_iter().map(|found| {
        let (built, entry) = entry_of(&found.item);
        let mut built = built.clone();
        built.isdir = entry.is_dir;
        built.parts.push(MatchedPart {
            text: entry.name.clone(),
            match_indices: found.indices,
        });
        built.score = found.score;
        built
    });

    if has_more {
        completion_iter
            .flat_map(|completion| {
                complete_rec(
                    &partial[1..],
                    &[completion],
                    ctx,
                    want_directory,
                    isdir,
                    false,
                )
            })
            .collect()
    } else {
        completion_iter.collect()
    }
}

#[derive(Debug)]
enum OriginalCwd {
    None,
    Home,
    Prefix(String),
}

pub fn surround_remove(partial: &str) -> String {
    for c in ['`', '"', '\''] {
        if partial.starts_with(c) {
            let ret = partial.strip_prefix(c).unwrap_or(partial);
            return match ret.split(c).collect::<Vec<_>>()[..] {
                [inside] => inside.to_string(),
                [inside, outside] if inside.ends_with(is_separator) => format!("{inside}{outside}"),
                _ => ret.to_string(),
            };
        }
    }
    partial.to_string()
}

/// Sort hidden entries (file names starting with `.`) after visible ones; stable, so
/// order within each group is preserved.
fn hidden_files_last(items: &mut [SemanticSuggestion]) {
    items.sort_by_key(|item| {
        Path::new(&item.suggestion.value)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
    });
}

/// Path completion for files, or for directories only when `directories_only`. Directories
/// also alone when the cursor is mid-path, which readjusts the view to the directory it is in.
///
/// The candidates are ranked as they are found, one path component at a time, so they are
/// offered as they are.
pub(crate) fn paths(ctx: &Context, directories_only: bool) -> Candidates {
    let AdjustView {
        prefix,
        span,
        readjusted,
    } = adjust_if_intermediate(ctx.prefix_str().as_ref(), ctx.working_set, ctx.span);
    let directories_only = directories_only || readjusted;

    let mut items = match ctx.delegation {
        Some(delegation) if delegation.sources.paths.is_some() => {
            remote_paths(ctx, delegation, &prefix, span, directories_only)
        }
        _ => {
            let cwd = ctx
                .working_set
                .permanent_state
                .cwd_as_string(Some(ctx.stack))
                .unwrap_or_default();
            complete_item(directories_only, span, &prefix, &[&cwd], ctx)
        }
    };

    hidden_files_last(&mut items);
    let label = if directories_only {
        "directory"
    } else {
        "file"
    };
    for item in &mut items {
        item.source = Some(Cow::Borrowed(label));
    }
    Candidates::kept(items)
}

/// Path completion against the directories of another machine, which the delegating
/// completer's `paths` closure lists: `prefix` is split at its last `/`, the directory part is
/// what the closure is asked for, and the rest is matched against the names it returns
/// (directories end in `/`). The directory is passed as typed, `.` when there is none.
fn remote_paths(
    ctx: &Context,
    delegation: &Delegation,
    prefix: &str,
    span: Span,
    directories_only: bool,
) -> Vec<SemanticSuggestion> {
    let cleaned = match delegation.sources.inside {
        Some(inside) => unescape_remote(&surround_remove(prefix), inside),
        None => surround_remove(prefix),
    };
    // `/` is ASCII, so splitting after it is always on a char boundary.
    let (dir, leaf) = cleaned.split_at(cleaned.rfind('/').map_or(0, |at| at + 1));
    let listed = if dir.is_empty() { "." } else { dir };
    let Listed {
        names: entries,
        origin,
    } = delegation
        .directory_entries(ctx, listed)
        .unwrap_or_default();

    let mut candidates = Candidates::<(String, bool)> {
        needle: Some(leaf.to_owned()),
        ..Candidates::default()
    };
    for entry in entries.iter() {
        let is_dir = entry.ends_with('/');
        if directories_only && !is_dir {
            continue;
        }
        // Matched without the slash that marks a directory, but offered with it.
        let name = entry.trim_end_matches('/').to_owned();
        candidates.push_as(name, (entry.clone(), is_dir));
    }

    let dir_width = dir.graphemes(true).count();
    candidates
        .ranked(ctx)
        .into_iter()
        .filter_map(|found| {
            let (entry, is_dir) = found.item;
            let path = format!("{dir}{entry}");
            let spelled = if delegation.literal {
                quote(
                    &path,
                    Spelling {
                        home: true,
                        ..Spelling::default()
                    },
                )
                .into_owned()
            } else {
                remote_word(&path, delegation.sources.inside)?
            };
            let (value, display_override) = if spelled == path {
                (path, None)
            } else {
                (spelled, Some(path))
            };
            Some(SemanticSuggestion {
                suggestion: Suggestion {
                    value,
                    span: to_reedline_span(span, ctx.offset),
                    display_override,
                    match_indices: Some(
                        found.indices.into_iter().map(|at| at + dir_width).collect(),
                    ),
                    // A directory goes on with what is in it.
                    append_whitespace: !is_dir,
                    ..Suggestion::default()
                },
                kind: Some(if is_dir {
                    SuggestionKind::Directory
                } else {
                    SuggestionKind::File
                }),
                origin,
                ..Default::default()
            })
        })
        .collect()
}

/// `path` spelled to be inserted in a line for another machine: quoted for the shell that reads
/// it there, then for Nushell, which passes the inner quotes on. Inside a string already opened
/// (`ssh host 'cat f`), the word is spelled without quotes of its own, which the string might not
/// hold, and escaped for the string. `None` when the string cannot hold the name at all.
fn remote_word(path: &str, inside: Option<Quote>) -> Option<String> {
    let Some(quote_kind) = inside else {
        let remote = posix_quote(path);
        return Some(quote(&remote, Spelling::default()).into_owned());
    };

    let escaped = posix_escape(path)?;
    match quote_kind {
        // Verbatim strings that cannot hold their own delimiter.
        Quote::Single if escaped.contains('\'') => None,
        Quote::Backtick if escaped.contains('`') => None,
        Quote::Double => Some(escaped.replace('\\', "\\\\").replace('"', "\\\"")),
        Quote::Single | Quote::Backtick => Some(escaped),
    }
}

/// The suggestions for the paths that match `partial`, ranked by their last component.
///
/// # Parameters
/// * `cwds` - A list of directories in which to search. The only reason this isn't a single string
///   is because dotnu_completions searches in multiple directories at once
/// * `ctx` - Where the options, the environment and the cached directory listings come from
pub fn complete_item(
    want_directory: bool,
    span: nu_protocol::Span,
    partial: &str,
    cwds: &[impl AsRef<str>],
    ctx: &Context,
) -> Vec<SemanticSuggestion> {
    let (engine_state, stack) = (ctx.working_set.permanent_state, ctx.stack);
    // The user may be typing inside a quote: stay in it.
    let inside = Quote::opened_by(partial);
    let cleaned_partial = surround_remove(partial);
    let isdir = cleaned_partial.ends_with(is_separator);
    let expanded_partial = expand_ndots(Path::new(&cleaned_partial));
    let should_collapse_dots = expanded_partial != Path::new(&cleaned_partial);
    let mut partial = expanded_partial.to_string_lossy().to_string();

    // Echo back whichever separator the user last typed; Windows accepts both.
    let path_separator = if cfg!(windows) {
        cleaned_partial
            .chars()
            .rfind(|c: &char| is_separator(*c))
            .unwrap_or(SEP)
    } else {
        SEP
    };

    // Handle the trailing dot case
    if cleaned_partial.ends_with(&format!("{path_separator}.")) {
        let _ = write!(partial, "{path_separator}.");
    }

    let cwd_pathbufs: Vec<_> = cwds
        .iter()
        .map(|cwd| Path::new(cwd.as_ref()).to_path_buf())
        .collect();
    let ls_colors = (engine_state.config.completions.use_ls_colors
        && engine_state.config.use_ansi_coloring.get(engine_state))
    .then(|| {
        let ls_colors_env_str = stack
            .get_env_var(engine_state, "LS_COLORS")
            .and_then(|v| env_to_string("LS_COLORS", v, engine_state, stack).ok());
        get_ls_colors(ls_colors_env_str)
    });

    let mut cwds = cwd_pathbufs.clone();
    let mut prefix_len = 0;
    let mut original_cwd = OriginalCwd::None;

    let mut components = Path::new(&partial).components().peekable();
    match components.peek().cloned() {
        Some(c @ Component::Prefix(..)) => {
            // windows only by definition
            cwds = vec![[c, Component::RootDir].iter().collect()];
            prefix_len = c.as_os_str().len();
            original_cwd = OriginalCwd::Prefix(c.as_os_str().to_string_lossy().into_owned());
        }
        Some(c @ Component::RootDir) => {
            // This is kind of a hack. When joining an empty string with the rest,
            // we add the slash automagically
            cwds = vec![PathBuf::from(c.as_os_str())];
            prefix_len = 1;
            original_cwd = OriginalCwd::Prefix(String::new());
        }
        Some(Component::Normal(home)) if home.to_string_lossy() == "~" => {
            cwds = home_dir()
                .map(|dir| vec![dir.into()])
                .unwrap_or(cwd_pathbufs);
            prefix_len = 1;
            original_cwd = OriginalCwd::Home;
        }
        _ => {}
    };

    // `prefix_len` is a byte length from a `Component` that may not land on a char
    // boundary of `partial`, so slice fallibly to avoid a panic on user input.
    let after_prefix = partial.get(prefix_len..).unwrap_or_default();
    let partial: Vec<_> = after_prefix
        .strip_prefix(is_separator)
        .unwrap_or(after_prefix)
        .split(is_separator)
        .filter(|s| !s.is_empty())
        .collect();

    complete_rec(
        partial.as_slice(),
        &cwds
            .into_iter()
            .map(|cwd| PathBuiltFromString {
                cwd,
                parts: Vec::new(),
                isdir: false,
                score: None,
            })
            .collect::<Vec<_>>(),
        ctx,
        want_directory,
        isdir,
        // A directory named exactly is the one meant, however loosely the last part matches:
        // `crates/nu-cli/src/comp` stays in `crates/nu-cli/src/`.
        true,
    )
    .into_iter()
    .map(|mut p| {
        if should_collapse_dots {
            p = collapse_ndots(p);
        }
        let is_dir = p.isdir;

        let mut path = match &original_cwd {
            OriginalCwd::None => String::new(),
            OriginalCwd::Home => format!("~{path_separator}"),
            OriginalCwd::Prefix(s) => format!("{s}{path_separator}"),
        };
        let mut match_index_offset = path.graphemes(true).count();
        let mut match_indices = Vec::new();
        for (i, part) in p.parts.iter().enumerate() {
            path.push_str(&part.text);
            for ind in &part.match_indices {
                match_indices.push(ind + match_index_offset);
            }
            match_index_offset += part.text.graphemes(true).count();
            if i != p.parts.len() - 1 {
                path.push(path_separator);
                // One grapheme, not `len_utf8()`: this offset counts graphemes.
                match_index_offset += 1;
            }
        }
        if p.isdir {
            path.push(path_separator);
        }

        let real_path = expand_to_real_path(&path);
        let metadata = std::fs::symlink_metadata(&real_path).ok();
        let style = ls_colors.as_ref().map(|lsc| {
            lsc.style_for_path_with_metadata(&real_path, metadata.as_ref())
                .map(lscolors::Style::to_nu_ansi_term_style)
                .unwrap_or_default()
        });
        let spelling = Spelling {
            glob: true,
            inside,
            home: matches!(original_cwd, OriginalCwd::Home),
            ..Spelling::default()
        };
        let (value, display_override) = match quote(&path, spelling) {
            Cow::Borrowed(_) => (path, None),
            Cow::Owned(quoted) => (quoted, Some(path)),
        };
        SemanticSuggestion {
            score: p.score,
            suggestion: Suggestion {
                value,
                style,
                span: to_reedline_span(span, ctx.offset),
                display_override,
                match_indices: Some(match_indices),
                // A directory goes on with what is in it.
                append_whitespace: !is_dir,
                ..Suggestion::default()
            },
            kind: Some(if is_dir {
                SuggestionKind::Directory
            } else {
                SuggestionKind::File
            }),
            ..Default::default()
        }
    })
    .collect()
}

pub struct AdjustView {
    pub prefix: String,
    pub span: Span,
    pub readjusted: bool,
}

pub fn adjust_if_intermediate(
    prefix: &str,
    working_set: &StateWorkingSet,
    mut span: nu_protocol::Span,
) -> AdjustView {
    let span_contents = String::from_utf8_lossy(working_set.get_span_contents(span)).into_owned();
    let mut prefix = prefix.to_string();

    // A difference of 1 because of the cursor's unicode code point in between.
    // Using .chars().count() because unicode and Windows. Saturating: the prefix can run
    // past the span's end into a trailing gap (`ls | where ⌶`), leaving nothing to readjust.
    let readjusted = span_contents
        .chars()
        .count()
        .saturating_sub(prefix.chars().count())
        > 1;
    if readjusted {
        let remnant: String = span_contents
            .chars()
            .skip(prefix.chars().count() + 1)
            .take_while(|&c| !is_separator(c))
            .collect();
        prefix.push_str(&remnant);
        span = Span::new(span.start, span.start + prefix.chars().count() + 1);
    }
    AdjustView {
        prefix,
        span,
        readjusted,
    }
}

/// Collapse multiple ".." components into n-dots.
///
/// It performs the reverse operation of `expand_ndots`, collapsing sequences of ".." into n-dots,
/// such as "..." and "....".
///
/// The resulting path will use platform-specific path separators, regardless of what path separators were used in the input.
fn collapse_ndots(path: PathBuiltFromString) -> PathBuiltFromString {
    let mut result = PathBuiltFromString {
        parts: Vec::with_capacity(path.parts.len()),
        isdir: path.isdir,
        cwd: path.cwd,
        score: path.score,
    };

    let mut dot_count = 0;

    for part in path.parts {
        if &part.text == ".." {
            dot_count += 1;
        } else {
            if dot_count > 0 {
                result.parts.push(MatchedPart {
                    text: ".".repeat(dot_count + 1),
                    match_indices: Vec::new(),
                });
                dot_count = 0;
            }
            result.parts.push(part);
        }
    }

    // Add any remaining dots
    if dot_count > 0 {
        result.parts.push(MatchedPart {
            text: ".".repeat(dot_count + 1),
            match_indices: Vec::new(),
        });
    }

    result
}

/// `text` as one word for a POSIX shell: bare when it holds only characters no
/// shell treats specially, else in single quotes, each `'` written `'\''`.
fn posix_quote(text: &str) -> Cow<'_, str> {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_./~@%+=:,-".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("'{}'", text.replace('\'', "'\\''")))
    }
}

/// The name a word typed inside a string stands for: the string reads its escapes (a double
/// quote does), then the remote shell reads the backslashes left.
fn unescape_remote(typed: &str, inside: Quote) -> String {
    /// Every backslash dropped, keeping the character it escapes.
    fn unescape(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            out.push(if c == '\\' {
                chars.next().unwrap_or(c)
            } else {
                c
            });
        }
        out
    }
    match inside {
        Quote::Double => unescape(&unescape(typed)),
        Quote::Single | Quote::Backtick => unescape(typed),
    }
}

/// `text` as one word for a POSIX shell without quotes: every character it treats specially
/// escaped by a backslash. `None` for a newline, which a backslash does not escape.
fn posix_escape(text: &str) -> Option<String> {
    let safe = |c: char| c.is_alphanumeric() || "_./~@%+=:,-".contains(c);
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' {
            return None;
        }
        if !safe(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    Some(escaped)
}

#[cfg(test)]
mod remote_quote_tests {
    use super::{Quote, posix_escape, posix_quote, remote_word, unescape_remote};

    #[test]
    fn remote_words_are_quoted_for_the_shell_that_reads_them() {
        assert_eq!(posix_quote("logs/app.log"), "logs/app.log");
        assert_eq!(posix_quote("a;b"), "'a;b'");
        assert_eq!(posix_quote("it's"), "'it'\\''s'");
        assert_eq!(posix_quote("$(rm)"), "'$(rm)'");
        assert_eq!(posix_quote(""), "''");
    }

    #[test]
    fn a_word_inside_a_string_is_valid_for_the_string_and_the_shell() {
        assert_eq!(posix_escape("my notes").as_deref(), Some("my\\ notes"));
        assert_eq!(posix_escape("a\nb"), None);
        // What was inserted reads back as the name.
        assert_eq!(unescape_remote("my\\ no", Quote::Single), "my no");
        assert_eq!(unescape_remote("my\\\\ no", Quote::Double), "my no");

        let single = Some(Quote::Single);
        assert_eq!(remote_word("a;b c", single).as_deref(), Some("a\\;b\\ c"));
        // A single-quoted string cannot hold a quote, so the name cannot be written in it.
        assert_eq!(remote_word("it's", single), None);

        // A double-quoted string reads its backslashes and quotes first.
        let double = Some(Quote::Double);
        assert_eq!(
            remote_word("my notes", double).as_deref(),
            Some("my\\\\ notes")
        );
        assert_eq!(
            remote_word("say\"x\"", double).as_deref(),
            Some("say\\\\\\\"x\\\\\\\"")
        );
    }
}
