//! How much, and how recently, the user has used each command, argument and directory, read from
//! the history, to rank candidates the way zoxide ranks directories.
//!
//! The index reads the most recent [`SCANNED`] history lines. Every command head (and a two-word
//! head such as `str trim`, when that is a command the engine knows), every argument word and
//! every directory the user was in is kept with the time it was used, and a candidate's
//! *frecency* is the sum of the [`weight`] of each of its uses: a use in the last hour counts
//! four times, one in the last day twice, one in the last week half and an older one a quarter.
//!
//! - A command is ranked by its own uses.
//! - An argument is ranked by the uses it had as an argument of the same command (`git checkout`
//!   and `main`), and by the uses it had under any other at [`OTHER_COMMAND`] of the weight. A use
//!   in the directory the user is in now counts [`HERE`] times.
//! - A path is ranked by the times it was named in a command line, resolved against the directory
//!   that line ran in. A directory asked for as one (`cd `) also counts the times the user was in
//!   it, and half of those in the directories below it.
//!
//! What a history without times (the plain text file) has is counted with every use at weight one.
//!
//! A candidate's frecency becomes [`Score::used`](super::rank::Score::used), on a logarithmic
//! scale so that the first uses count most. Where text is typed it is at most [`CAP`], so a better
//! match still wins over a favourite; where nothing is, the scale has no cap and recency leads.

use super::rank::Boost;
use nu_path::expand_path_with;
use reedline::{History, HistoryItem, SearchDirection, SearchQuery};
use std::{
    borrow::Cow,
    collections::HashMap,
    path::{Path, PathBuf, is_separator},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// History lines read, newest first.
const SCANNED: i64 = 5000;
/// The most a candidate's use adds to its score where text is typed.
const CAP: i32 = 40;
/// The most it adds where nothing is typed: far past what any history reaches, so that the order
/// of the candidates used is their frecency's.
const UNCAPPED: i32 = 10_000;
/// Tokens longer than this are text, not names, and are not counted.
const LONGEST_WORD: usize = 64;
/// How much a use under another command counts, against one under the command asked about.
const OTHER_COMMAND: f64 = 0.25;
/// How much a use in the directory the user is in counts, against one elsewhere.
const HERE: f64 = 2.0;
/// How much the time in a directory below counts for a directory.
const BELOW: f64 = 0.5;
/// The most directories above a path that are credited for naming it.
const ANCESTORS: usize = 8;

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;

/// The time of a use whose time is not known.
const NOT_TIMED: i64 = i64::MIN;
/// The directory of a use whose directory is not known.
const NO_DIR: u32 = u32::MAX;

/// A path as the index keys it: lowercased where the file system does not tell cases apart, as
/// the history may have been written from a shell that spelled a directory otherwise.
fn keyed(path: &str) -> Cow<'_, str> {
    if cfg!(any(target_os = "macos", windows)) {
        Cow::Owned(path.to_lowercase())
    } else {
        Cow::Borrowed(path)
    }
}

/// Opens a handle on the history of its own, to read it from another thread than the line
/// editor's.
pub(crate) type HistoryReader = Arc<dyn Fn() -> Option<Box<dyn History>> + Send + Sync>;

/// How much a use `age` seconds old counts, as zoxide weighs it.
pub(crate) fn weight(age: i64) -> f64 {
    if age < HOUR {
        4.0
    } else if age < DAY {
        2.0
    } else if age < WEEK {
        0.5
    } else {
        0.25
    }
}

/// The seconds since the Unix epoch.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_secs()).ok())
        .unwrap_or_default()
}

/// One use of a name.
#[derive(Debug, Clone, Copy)]
struct Use {
    /// When, in seconds since the Unix epoch, or [`NOT_TIMED`].
    at: i64,
    /// The command head it was given to, as an id of the index.
    head: u32,
    /// The directory it was used in, as an id of the index, or [`NO_DIR`].
    dir: u32,
}

impl Use {
    /// The weight of this use at `now`: one where it has no time.
    fn weight(&self, now: i64) -> f64 {
        if self.at == NOT_TIMED {
            1.0
        } else {
            weight(now.saturating_sub(self.at))
        }
    }
}

/// A time spent in a directory, or in one below it.
#[derive(Debug, Clone, Copy)]
struct Visit {
    at: i64,
    /// In the directory itself, rather than below it.
    exact: bool,
}

/// Names of the index, as small ids.
#[derive(Debug, Default)]
struct Names(HashMap<String, u32>);

impl Names {
    /// The id of `name`, adding it the first time it is seen.
    fn id(&mut self, name: &str) -> u32 {
        let next = u32::try_from(self.0.len()).unwrap_or(NO_DIR - 1);
        *self.0.entry(name.to_owned()).or_insert(next)
    }

    fn get(&self, name: &str) -> Option<u32> {
        self.0.get(name).copied()
    }
}

/// What the history says about how much each command, argument and directory is used.
#[derive(Debug, Default)]
pub(crate) struct UsageIndex {
    /// Lines in the history this index was read from.
    len: i64,
    /// Uses of each command head: `ls`, and `git checkout` when that is a known command.
    heads: HashMap<String, Vec<Use>>,
    /// Uses of each argument word, with the head it was given to.
    words: HashMap<String, Vec<Use>>,
    /// Uses of each path named as an argument, resolved against the directory it was used in,
    /// and of the directories above it.
    named: HashMap<String, Vec<Use>>,
    /// The times the user was in each directory, and in the ones below it.
    visits: HashMap<String, Vec<Visit>>,
    /// The heads in [`Use::head`].
    head_names: Names,
    /// The directories in [`Use::dir`].
    dir_names: Names,
}

/// Which kind of candidate a boost is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Used {
    /// A command name.
    Command,
    /// An argument value.
    Argument,
    /// An entry of a directory, which [`Asking::beneath`] names. `directories` when only a
    /// directory is wanted, as for `cd`.
    Path { directories: bool },
}

/// What a candidate is asked for, which decides which of its uses count.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Asking<'a> {
    /// The command the candidate is an argument of.
    pub(crate) head: Option<&'a str>,
    /// The directory the user is in.
    pub(crate) cwd: Option<&'a str>,
    /// The directory the candidates are entries of.
    pub(crate) beneath: Option<&'a Path>,
    /// Whether nothing is typed, so that recency leads.
    pub(crate) nothing_typed: bool,
    /// The time to weigh the uses at, in seconds since the Unix epoch.
    pub(crate) now: i64,
}

impl UsageIndex {
    /// Reads the newest [`SCANNED`] lines of `history`. `is_command` says whether a two-word
    /// head such as `str trim` names a command; a history that cannot be read is an empty index.
    pub(crate) fn scan(history: &dyn History, is_command: impl Fn(&str) -> bool) -> Self {
        let query = SearchQuery {
            limit: Some(SCANNED),
            ..SearchQuery::everything(SearchDirection::Backward, None)
        };
        let lines = history.search(query).unwrap_or_default();
        Self::from_items(&lines, history.count_all().unwrap_or_default(), is_command)
    }

    /// The index of `lines`, newest first, which are the newest of a history of `len` lines.
    fn from_items(lines: &[HistoryItem], len: i64, is_command: impl Fn(&str) -> bool) -> Self {
        let mut index = Self {
            len,
            ..Self::default()
        };
        let mut visited: HashMap<&str, Vec<i64>> = HashMap::new();
        for item in lines {
            let at = item
                .start_timestamp
                .map_or(NOT_TIMED, |at| at.to_millis().div_euclid(1000));
            let cwd = item.cwd.as_deref().filter(|cwd| !cwd.is_empty());
            index.count_line(&item.command_line, at, cwd, &is_command);
            if let Some(cwd) = cwd.filter(|_| at != NOT_TIMED) {
                visited.entry(cwd).or_default().push(at);
            }
        }
        for (dir, times) in visited {
            index.count_visits(dir, &times);
        }
        index
    }

    /// The number of history lines this index was read from, to tell when it is out of date.
    pub(crate) fn len(&self) -> i64 {
        self.len
    }

    /// Counts the commands of one history line, run at `at` in `cwd`: each pipeline stage is a
    /// head and its arguments.
    fn count_line(
        &mut self,
        line: &str,
        at: i64,
        cwd: Option<&str>,
        is_command: &impl Fn(&str) -> bool,
    ) {
        let dir = cwd.map_or(NO_DIR, |cwd| self.dir_names.id(&keyed(cwd)));
        for stage in line.split(['|', ';', '\n']) {
            let mut words = stage
                .split_whitespace()
                .map(|word| word.trim_matches(['"', '\'', '`']))
                .filter(|word| !word.is_empty() && word.len() <= LONGEST_WORD);
            let Some(head) = words.next().map(|head| head.trim_start_matches(['^', '%'])) else {
                continue;
            };
            let mut head_id = self.head_names.id(head);
            let used = |head, dir| Use { at, head, dir };
            self.heads
                .entry(head.to_owned())
                .or_default()
                .push(used(head_id, dir));

            let mut arguments = words.filter(|word| !word.starts_with(['-', '$', '(', '{', '[']));
            if let Some(second) = arguments.next() {
                // The word is an argument of the head alone, and then the arguments of the two
                // words are the command's own.
                self.count_word(second, used(head_id, dir), cwd);
                let two_words = format!("{head} {second}");
                if is_command(&two_words) {
                    head_id = self.head_names.id(&two_words);
                    self.heads
                        .entry(two_words)
                        .or_default()
                        .push(used(head_id, dir));
                }
            }
            for word in arguments {
                self.count_word(word, used(head_id, dir), cwd);
            }
        }
    }

    /// Counts one use of the argument `word`, which was run in `cwd` when it is known: as a word,
    /// and as the path it names there.
    fn count_word(&mut self, word: &str, used: Use, cwd: Option<&str>) {
        self.words.entry(word.to_owned()).or_default().push(used);
        let Some(cwd) = cwd else {
            return;
        };
        let mut credited = expand_path_with(word, cwd, true);
        for _ in 0..=ANCESTORS {
            self.named
                .entry(keyed(&credited.to_string_lossy()).into_owned())
                .or_default()
                .push(used);
            // The directories between the one the word was used in and the path hold it.
            match credited.parent() {
                Some(parent)
                    if parent.parent().is_some() && !Path::new(cwd).starts_with(parent) =>
                {
                    credited = parent.to_path_buf();
                }
                _ => break,
            }
        }
    }

    /// Counts the `times` the user was in `dir`, for it and the directories above it.
    fn count_visits(&mut self, dir: &str, times: &[i64]) {
        for (steps, ancestor) in Path::new(dir).ancestors().enumerate() {
            if ancestor.parent().is_none() {
                break;
            }
            let visits = self
                .visits
                .entry(keyed(&ancestor.to_string_lossy()).into_owned())
                .or_default();
            visits.extend(times.iter().map(|&at| Visit {
                at,
                exact: steps == 0,
            }));
        }
    }

    /// The frecency of the command head `name`.
    fn command(&self, name: &str, now: i64) -> f64 {
        self.heads
            .get(name)
            .map_or(0.0, |uses| uses.iter().map(|used| used.weight(now)).sum())
    }

    /// The frecency of `uses` as arguments, weighed by the command and the directory they were
    /// given in.
    fn argument(&self, uses: Option<&Vec<Use>>, asking: &Asking) -> f64 {
        let head = asking.head.and_then(|head| self.head_names.get(head));
        let here = asking.cwd.and_then(|cwd| self.dir_names.get(&keyed(cwd)));
        uses.map_or(0.0, |uses| {
            uses.iter()
                .map(|used| {
                    let command = match (asking.head, head) {
                        (None, _) => 1.0,
                        (Some(_), Some(head)) if used.head == head => 1.0,
                        _ => OTHER_COMMAND,
                    };
                    let place = if here == Some(used.dir) { HERE } else { 1.0 };
                    used.weight(asking.now) * command * place
                })
                .sum()
        })
    }

    /// The frecency of the directory `path`: the times the user was in it, and in the ones below.
    fn visited(&self, path: &str, now: i64) -> f64 {
        self.visits.get(path).map_or(0.0, |visits| {
            visits
                .iter()
                .map(|visit| {
                    let share = if visit.exact { 1.0 } else { BELOW };
                    weight(now.saturating_sub(visit.at)) * share
                })
                .sum()
        })
    }

    /// The frecency of `name`, as a candidate of `kind`.
    pub(crate) fn frecency(&self, kind: Used, name: &str, asking: &Asking) -> f64 {
        match kind {
            Used::Command => self.command(name, asking.now),
            Used::Argument => self.argument(self.words.get(name), asking),
            Used::Path { directories } => {
                let Some(path) = asking.beneath.map(|beneath| beneath.join(name)) else {
                    return 0.0;
                };
                let path = path.to_string_lossy();
                let path = keyed(path.trim_end_matches(is_separator));
                let path = path.as_ref();
                let named = self.argument(self.named.get(path), asking);
                if directories {
                    named + self.visited(path, asking.now)
                } else {
                    named
                }
            }
        }
    }

    /// The boost for `name`, as a candidate of `kind`.
    pub(crate) fn boost(&self, kind: Used, name: &str, asking: &Asking) -> Boost {
        let cap = if asking.nothing_typed { UNCAPPED } else { CAP };
        Boost {
            used: scaled(self.frecency(kind, name, asking), cap),
            git: 0,
        }
    }
}

/// `8 * log2(1 + frecency)`, at most `cap`.
fn scaled(frecency: f64, cap: i32) -> i32 {
    let points = 8.0 * (1.0 + frecency).log2();
    (points as i32).min(cap)
}

/// A reader of the history `engine_state` is configured with, if it has one that can be opened a
/// second time: the plain text file, or the SQLite database.
pub(crate) fn history_reader(
    engine_state: &nu_protocol::engine::EngineState,
) -> Option<HistoryReader> {
    use nu_protocol::HistoryFileFormat;
    let config = engine_state.history_config()?;
    let path: PathBuf = engine_state.history_path()?;
    let open = move || -> Option<Box<dyn History>> {
        match config.file_format {
            HistoryFileFormat::Plaintext => {
                reedline::FileBackedHistory::with_file(config.max_size as usize, path.clone())
                    .ok()
                    .map(|history| Box::new(history) as Box<dyn History>)
            }
            #[cfg(feature = "sqlite")]
            HistoryFileFormat::Sqlite => {
                reedline::SqliteBackedHistory::with_file(path.clone(), None, None)
                    .ok()
                    .map(|history| Box::new(history) as Box<dyn History>)
            }
            #[cfg(not(feature = "sqlite"))]
            HistoryFileFormat::Sqlite => None,
        }
    };
    Some(Arc::new(open))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::{FileBackedHistory, HistoryTimestamp};

    fn history(lines: &[&str]) -> FileBackedHistory {
        let mut history = FileBackedHistory::new(2 * SCANNED as usize).expect("a valid capacity");
        for line in lines {
            history
                .save(HistoryItem::from_command_line(*line))
                .expect("an in-memory history saves");
        }
        history
    }

    const NOW: i64 = 1_800_000_000;

    /// The index of lines given as `(seconds ago, directory, line)`, as it was at [`NOW`].
    fn timed(lines: &[(i64, &str, &str)]) -> UsageIndex {
        let items: Vec<_> = lines
            .iter()
            .rev()
            .map(|(ago, cwd, line)| {
                let mut item = HistoryItem::from_command_line(*line);
                item.start_timestamp = Some(HistoryTimestamp::from_millis((NOW - ago) * 1000));
                item.cwd = Some((*cwd).to_owned());
                item
            })
            .collect();
        UsageIndex::from_items(&items, items.len() as i64, |name| name == "git checkout")
    }

    fn asking(head: Option<&str>) -> Asking<'_> {
        Asking {
            head,
            now: NOW,
            ..Asking::default()
        }
    }

    #[test]
    fn heads_and_arguments_are_counted() {
        let history = history(&["cargo build --release", "ls src | str trim", "cargo build"]);
        let index = UsageIndex::scan(&history, |name| name == "str trim");
        assert_eq!(index.len(), 3);
        assert_eq!(index.heads.get("cargo").map(Vec::len), Some(2));
        assert_eq!(index.heads.get("ls").map(Vec::len), Some(1));
        assert_eq!(index.words.get("build").map(Vec::len), Some(2));
        assert_eq!(index.words.get("src").map(Vec::len), Some(1));
        // `--release` is a flag, not an argument word; `str trim` is a command the engine knows.
        assert!(!index.words.contains_key("--release"));
        assert_eq!(index.heads.get("str trim").map(Vec::len), Some(1));
        assert_eq!(index.heads.get("str").map(Vec::len), Some(1));
    }

    #[test]
    fn a_two_word_head_counts_when_it_is_a_command() {
        // An in-memory history does not save a line twice in a row.
        let history = history(&["str trim", "ls", "str trim"]);
        let index = UsageIndex::scan(&history, |name| name == "str trim");
        assert_eq!(index.heads.get("str trim").map(Vec::len), Some(2));
    }

    #[test]
    fn a_use_counts_by_how_old_it_is() {
        // The weights are zoxide's, and change at the hour, the day and the week.
        assert_eq!(weight(0), 4.0);
        assert_eq!(weight(HOUR - 1), 4.0);
        assert_eq!(weight(HOUR), 2.0);
        assert_eq!(weight(DAY - 1), 2.0);
        assert_eq!(weight(DAY), 0.5);
        assert_eq!(weight(WEEK - 1), 0.5);
        assert_eq!(weight(WEEK), 0.25);
        assert_eq!(weight(100 * WEEK), 0.25);
        // A use from the future (a clock set back) counts as a fresh one.
        assert_eq!(weight(-5), 4.0);
    }

    #[test]
    fn frecency_adds_the_weights_of_the_uses() {
        let index = timed(&[
            (10, "/p", "ls a"),
            (2 * HOUR, "/p", "ls b"),
            (3 * DAY, "/p", "ls c"),
            (30 * DAY, "/p", "ls d"),
            (30 * DAY, "/p", "rm e"),
        ]);
        let ls = index.frecency(Used::Command, "ls", &asking(None));
        assert_eq!(ls, 4.0 + 2.0 + 0.5 + 0.25);
        assert_eq!(index.frecency(Used::Command, "rm", &asking(None)), 0.25);
        assert_eq!(index.frecency(Used::Command, "mv", &asking(None)), 0.0);
    }

    #[test]
    fn a_word_given_to_a_command_is_lifted_there_and_less_elsewhere() {
        let index = timed(&[
            (10, "/p", "git checkout main"),
            (2 * HOUR, "/p", "rm main"),
            (30, "/p", "rm other"),
        ]);
        let word = |name, head| index.frecency(Used::Argument, name, &asking(head));
        // `main` was given to `git checkout` a moment ago and to `rm` two hours ago.
        assert_eq!(
            word("main", Some("git checkout")),
            4.0 + 2.0 * OTHER_COMMAND
        );
        assert_eq!(word("main", Some("rm")), 2.0 + 4.0 * OTHER_COMMAND);
        // `other` was only given to `rm`: after `git checkout` it counts for less.
        assert_eq!(word("other", Some("rm")), 4.0);
        assert_eq!(word("other", Some("git checkout")), 4.0 * OTHER_COMMAND);
        // With no command to ask for, every use counts in full.
        assert_eq!(word("main", None), 6.0);
    }

    #[test]
    fn a_word_used_in_this_directory_counts_more() {
        let index = timed(&[(10, "/here", "vim notes"), (10, "/there", "vim notes")]);
        let notes = |cwd| {
            let asking = Asking {
                cwd,
                ..asking(Some("vim"))
            };
            index.frecency(Used::Argument, "notes", &asking)
        };
        assert_eq!(notes(Some("/here")), 4.0 * HERE + 4.0);
        assert_eq!(notes(Some("/elsewhere")), 8.0);
        assert_eq!(notes(None), 8.0);
    }

    #[test]
    fn a_directory_is_lifted_by_the_times_the_user_was_in_it_and_below_it() {
        let index = timed(&[
            (10, "/p/app/src", "ls a"),
            (20, "/p/app/src", "ls b"),
            (30, "/p/app", "ls c"),
            (30 * DAY, "/p/docs", "ls d"),
        ]);
        let visits = |kind, name| {
            let asking = Asking {
                beneath: Some(Path::new("/p")),
                ..asking(Some("cd"))
            };
            index.frecency(kind, name, &asking)
        };
        let directory = Used::Path { directories: true };
        // `app` was in once itself, and twice from a directory below it.
        assert_eq!(visits(directory, "app"), 4.0 + 2.0 * 4.0 * BELOW);
        assert_eq!(visits(directory, "docs"), 0.25);
        assert_eq!(visits(directory, "elsewhere"), 0.0);
        // Only a directory asked for as one counts the times the user was in it.
        assert_eq!(visits(Used::Path { directories: false }, "app"), 0.0);
    }

    #[test]
    fn a_path_named_in_a_line_counts_for_the_directories_above_it() {
        let index = timed(&[(10, "/p", "hx crates/lib/src/main.rs")]);
        let named = |beneath, name| {
            let asking = Asking {
                beneath: Some(Path::new(beneath)),
                ..asking(Some("hx"))
            };
            index.frecency(Used::Path { directories: false }, name, &asking)
        };
        assert_eq!(named("/p/crates/lib/src", "main.rs"), 4.0);
        assert_eq!(named("/p/crates", "lib"), 4.0);
        assert_eq!(named("/p", "crates"), 4.0);
        assert_eq!(named("/p", "other"), 0.0);
    }

    #[test]
    fn the_boost_grows_slowly_and_is_capped() {
        let boost = |count| scaled(f64::from(count), CAP);
        assert_eq!(boost(0), 0);
        assert_eq!(boost(1), 8);
        assert!(boost(3) > boost(1));
        assert!(boost(7) < 2 * boost(3));
        assert_eq!(boost(u32::MAX), CAP);
    }

    #[test]
    fn nothing_typed_lifts_without_the_cap() {
        let lines: Vec<String> = (0..200).map(|n| format!("ls {n}")).collect();
        let timed: Vec<_> = lines.iter().map(|line| (10, "/p", line.as_str())).collect();
        let index = self::timed(&timed);
        let boost = |nothing_typed| {
            let asking = Asking {
                nothing_typed,
                ..asking(None)
            };
            index.boost(Used::Command, "ls", &asking).used
        };
        assert_eq!(boost(false), CAP);
        assert!(boost(true) > CAP);
    }

    #[test]
    fn a_name_never_used_gets_no_boost() {
        let index = UsageIndex::default();
        assert_eq!(
            index.boost(Used::Command, "ls", &asking(None)),
            Boost::default()
        );
    }

    #[test]
    fn only_the_newest_lines_are_read() {
        let lines: Vec<String> = (0..SCANNED + 10).map(|n| format!("cmd{n}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let index = UsageIndex::scan(&history(&refs), |_| false);
        assert_eq!(index.heads.len(), SCANNED as usize);
        assert!(index.heads.contains_key(&format!("cmd{}", SCANNED + 9)));
        assert!(!index.heads.contains_key("cmd0"));
    }
}
