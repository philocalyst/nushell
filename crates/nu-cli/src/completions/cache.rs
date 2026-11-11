//! What completion remembers between requests, keyed by what was asked.
//!
//! The text typed up to the cursor is not a sound key: the same text completes differently in
//! another directory, and a longer text asks the same question as a shorter one. These caches
//! remember the answer to the expensive questions instead, and every request ranks what they
//! hold again, so narrowing the token, or deleting from it, costs a match and not a scan:
//!
//! * the executables on `PATH`, keyed by the `PATH` directories and their modification times;
//! * the entries of a directory, keyed by its path and modification time;
//! * what a custom completer returned, for a completer that the typed token cannot change (see
//!   [`AnswerKey`]), keyed by the completer and the line before the token;
//! * what a source closure of a delegated line listed (see [`SourceKey`]), keyed by the closure
//!   and what it was asked.
//!
//! The `cache_size` setting is the number of custom answers kept. A `cache_size` of `0` keeps
//! nothing of the four.

use super::{
    trace::{Lookup, Trace},
    usage::{HistoryReader, UsageIndex},
};
use lru::LruCache;
use nu_protocol::{
    BlockId, Span, Value,
    engine::{Closure, EngineState, Stack},
};
use nu_utils::time::Instant;
use reedline::History;
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

/// Custom completer answers kept when `cache_size` is not set.
pub(crate) const DEFAULT_CACHE_SIZE: usize = 100;
/// Directories whose entries are kept.
const LISTINGS: usize = 64;
/// Lists of names that delegated source closures returned, which are kept.
const SOURCES: usize = 32;
/// Pipelines whose evaluation is kept.
const EVALUATIONS: usize = 16;
/// How long what a pipeline evaluated to is reused: long enough for the keystrokes of a word,
/// short enough that the data it read is still the data there is.
const EVALUATION_LIFETIME: Duration = Duration::from_secs(5);
/// How long a custom completer's answer is reused. What it lists, such as the branches of a
/// repository, can change without anything the cache can see changing.
const ANSWER_LIFETIME: Duration = Duration::from_secs(30);
/// How long the failure of a delegated source's closure is reused.
const FAILURE_LIFETIME: Duration = Duration::from_secs(5);
/// A directory last modified less than this before it was read may be modified again within the
/// resolution of the clock without its modification time changing, so it is read again next time.
const SETTLE: Duration = Duration::from_secs(2);

/// The environment a cached answer was computed against.
///
/// Answers depend on cwd, `PATH`, known declarations, and `$env.config`, which change between
/// prompts while the question does not.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CacheEnv(u64);

impl CacheEnv {
    /// Fingerprint the completion-relevant parts of `engine_state`/`stack`, once per
    /// completer.
    pub(crate) fn of(engine_state: &EngineState, stack: &Stack) -> Self {
        let mut hasher = DefaultHasher::new();

        engine_state.num_decls().hash(&mut hasher);
        stack
            .get_env_var(engine_state, "PATH")
            .map(|path| path.to_expanded_string(":", &stack.get_config(engine_state)))
            .hash(&mut hasher);

        let cwd = engine_state.cwd(Some(stack)).ok();
        // The cwd mtime, so adding/removing files invalidates stale file completions.
        cwd.as_ref()
            .and_then(|cwd| fs::metadata(cwd).ok()?.modified().ok())
            .hash(&mut hasher);
        cwd.hash(&mut hasher);

        engine_state.config_epoch().hash(&mut hasher);
        // Stack-local config changes do not bump the engine epoch.
        stack.config.is_some().hash(&mut hasher);
        if let Some(config) = &stack.config {
            format!("{config:?}").hash(&mut hasher);
        }

        Self(hasher.finish())
    }
}

/// An entry of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    /// Whether it is a directory, or a link to one.
    pub(crate) is_dir: bool,
}

/// The executables a `PATH` holds: every entry of its directories, in `PATH` order. Whether an
/// entry is executable is for the reader to ask the file, which can change without the
/// directory showing it.
#[derive(Debug, Default)]
pub(crate) struct PathEntries {
    dirs: Vec<PathBuf>,
    /// File names, each with the index of its directory in `dirs`.
    names: Vec<(String, usize)>,
}

impl PathEntries {
    /// Each file name and its path.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, PathBuf)> {
        self.names
            .iter()
            .map(|(name, dir)| (name.as_str(), self.dirs[*dir].join(name)))
    }

    /// Reads the directories that can be read.
    fn scan(dirs: &[PathBuf]) -> Self {
        let mut scan = Self::default();
        for dir in dirs {
            let Ok(read) = fs::read_dir(dir) else {
                continue;
            };
            let index = scan.dirs.len();
            scan.dirs.push(dir.clone());
            scan.names.extend(
                read.filter_map(Result::ok)
                    .filter_map(|entry| Some((entry.file_name().into_string().ok()?, index))),
            );
        }
        scan
    }
}

/// When a directory was last modified, as far as the file system says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    /// There is no such directory, which is as it will stay until it is made: a `PATH` often
    /// names directories that do not exist.
    Absent,
    /// The file system does not say.
    Unknown,
    At(SystemTime),
}

impl Stamp {
    fn of(path: &Path) -> Self {
        match fs::metadata(path) {
            Err(_) => Self::Absent,
            Ok(metadata) => metadata.modified().map_or(Self::Unknown, Self::At),
        }
    }

    /// Whether a directory with this stamp, read at `at`, will show a change to it: it was
    /// modified far enough before the read that a later change is not within the same tick of
    /// the clock and leaves its time as it was.
    fn settled(self, at: SystemTime) -> bool {
        match self {
            Self::Absent => true,
            Self::Unknown => false,
            Self::At(modified) => at.duration_since(modified).is_ok_and(|age| age >= SETTLE),
        }
    }
}

/// What was read of the `PATH` directories, and what they looked like.
struct ScannedPath {
    stamp: Vec<(PathBuf, Stamp)>,
    at: SystemTime,
    entries: Arc<PathEntries>,
}

/// A directory as it was read.
struct Listing {
    stamp: Stamp,
    at: SystemTime,
    entries: Arc<[Entry]>,
}

/// What a custom completer's answer is keyed by.
///
/// Only a completer that takes none of `token`, `place` and `buffer` is cached: it is called with
/// nothing the token could change, so its answer is the same for every token after the same line.
/// What it returned is narrowed against the token afterwards, as an uncached one is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AnswerKey {
    /// The completer's block.
    pub(crate) block: BlockId,
    /// The line before the token being completed.
    pub(crate) before: Arc<str>,
}

/// What a source closure of a delegated line is asked: the closure, with the values it captured,
/// and the arguments it is called with. Two calls to a closure made by the same completer on
/// successive keystrokes are the same question, though each call makes a closure of its own.
#[derive(Clone)]
pub(crate) struct SourceKey {
    /// The closure, as a value so that it compares and hashes by block and captures, not by span.
    closure: Value,
    args: Vec<String>,
}

impl SourceKey {
    /// The question `closure` asks of `args`.
    pub(crate) fn new(closure: &Closure, args: &[&str]) -> Self {
        Self {
            closure: Value::closure(closure.clone(), Span::unknown()),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }
}

impl PartialEq for SourceKey {
    fn eq(&self, other: &Self) -> bool {
        self.closure.strict_eq(&other.closure) && self.args == other.args
    }
}

impl Eq for SourceKey {}

impl Hash for SourceKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.closure.hash(state);
        self.args.hash(state);
    }
}

/// The names a source closure listed, or why it listed none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Names {
    pub(crate) list: Arc<[String]>,
    /// What the closure failed with, which the status line tells the user.
    pub(crate) failure: Option<Arc<str>>,
}

impl Names {
    /// How long these are reused: a failure is tried again sooner, as the user may have fixed it.
    fn lifetime(&self) -> Duration {
        if self.failure.is_some() {
            FAILURE_LIFETIME
        } else {
            ANSWER_LIFETIME
        }
    }
}

/// The names a source closure listed, and when.
struct Listed {
    names: Names,
    at: Instant,
}

/// What evaluating a pipeline to complete from its data came to.
#[derive(Debug, Clone)]
pub(crate) enum Read {
    /// The data it gave, clipped to what completion reads.
    Data(Arc<Value>),
    /// It ran past its time budget.
    TimedOut,
    /// It failed, with the error's text.
    Failed(String),
}

/// An evaluation, and when it was made.
struct Evaluated {
    read: Read,
    at: Instant,
}

/// A custom completer's answer, and when it was given.
struct Answer {
    value: Value,
    at: Instant,
}

/// The custom answers, valid for one environment.
struct Answers {
    env: Option<CacheEnv>,
    entries: LruCache<AnswerKey, Answer>,
}

struct Inner {
    /// Whether anything is kept: `cache_size = 0` says no.
    enabled: AtomicBool,
    answers: Mutex<Answers>,
    path: Mutex<Option<ScannedPath>>,
    listings: Mutex<LruCache<PathBuf, Listing>>,
    /// What the source closures of delegated lines listed, whatever the environment: they stand
    /// for another machine.
    sources: Mutex<LruCache<SourceKey, Listed>>,
    /// What the history said the last time it was read; see [`CompletionCache::usage`].
    usage: Mutex<Option<Arc<UsageIndex>>>,
    /// Whether a thread is reading the history for `usage`.
    reading: AtomicBool,
    /// What the pipelines read to complete from their data came to; see [`CacheView::evaluation`].
    evaluations: Mutex<LruCache<(CacheEnv, String), Evaluated>>,
}

/// Cross-prompt completion cache, shared by the completers that are built for each prompt.
#[derive(Clone)]
pub(crate) struct CompletionCache {
    inner: Arc<Inner>,
}

impl Default for CompletionCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_SIZE)
    }
}

/// A lock that a panic elsewhere has not made unusable: completion never fails for a cache.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl CompletionCache {
    /// A cache keeping `size` custom answers; `0` keeps nothing.
    pub(crate) fn new(size: usize) -> Self {
        let capacity = |n: usize| NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: Arc::new(Inner {
                enabled: AtomicBool::new(size > 0),
                answers: Mutex::new(Answers {
                    env: None,
                    entries: LruCache::new(capacity(size)),
                }),
                path: Mutex::new(None),
                listings: Mutex::new(LruCache::new(capacity(LISTINGS))),
                sources: Mutex::new(LruCache::new(capacity(SOURCES))),
                usage: Mutex::new(None),
                reading: AtomicBool::new(false),
                evaluations: Mutex::new(LruCache::new(capacity(EVALUATIONS))),
            }),
        }
    }

    /// Resize in place; `0` keeps nothing and forgets what was kept. Reapplied each prompt.
    pub(crate) fn set_capacity(&self, size: usize) {
        let inner = &self.inner;
        inner.enabled.store(size > 0, Ordering::Relaxed);
        let mut answers = lock(&inner.answers);
        match NonZeroUsize::new(size) {
            Some(size) => answers.entries.resize(size),
            None => {
                answers.entries.clear();
                answers.env = None;
                *lock(&inner.path) = None;
                lock(&inner.listings).clear();
                lock(&inner.sources).clear();
                lock(&inner.evaluations).clear();
            }
        }
    }

    fn enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }

    /// What the history last said about how much the user has used each command, argument and
    /// directory, if it was read.
    pub(crate) fn usage(&self) -> Option<Arc<UsageIndex>> {
        lock(&self.inner.usage).clone()
    }

    /// Brings [`Self::usage`] up to date with `history`. It is read again only when the history
    /// has a different number of lines than when it was last read, so a prompt that adds nothing
    /// to it reuses the index of the one before. With a `reader` the history is read on a
    /// thread of its own, and the index of the requests that come first is the one before; without
    /// one it is read here.
    pub(crate) fn learn_usage(
        &self,
        history: &dyn History,
        reader: Option<&HistoryReader>,
        is_command: impl Fn(&str) -> bool + Send + 'static,
    ) {
        let len = history.count_all().unwrap_or_default();
        if self.usage().is_some_and(|usage| usage.len() == len) {
            return;
        }
        let Some(reader) = reader else {
            *lock(&self.inner.usage) = Some(Arc::new(UsageIndex::scan(history, is_command)));
            return;
        };
        // One reading at a time: the next prompt asks again if this one is out of date by then.
        if self.inner.reading.swap(true, Ordering::AcqRel) {
            return;
        }
        let (inner, reader) = (Arc::clone(&self.inner), Arc::clone(reader));
        let spawned = std::thread::Builder::new()
            .name("completion-usage".into())
            .spawn(move || {
                if let Some(history) = reader() {
                    let usage = Arc::new(UsageIndex::scan(history.as_ref(), is_command));
                    *lock(&inner.usage) = Some(usage);
                }
                inner.reading.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.inner.reading.store(false, Ordering::Release);
        }
    }
}

/// Tell a trace, if there is one, what the cache said to the source being asked.
fn note(trace: Option<&Trace>, lookup: Lookup) {
    if let Some(trace) = trace {
        trace.cached(lookup);
    }
}

/// The cache as one completer sees it: with the fingerprint of the environment it runs in, which
/// decides what of the custom answers it may use.
#[derive(Clone)]
pub(crate) struct CacheView {
    cache: CompletionCache,
    env: CacheEnv,
}

impl CacheView {
    /// `cache`, for requests made in the environment `env`.
    pub(crate) fn new(cache: CompletionCache, env: CacheEnv) -> Self {
        Self { cache, env }
    }

    /// A view that keeps nothing: every question is read again.
    pub(crate) fn disabled() -> Self {
        Self::new(CompletionCache::new(0), CacheEnv::default())
    }

    /// The entries of the `PATH` directories `dirs`, read again when one of them was modified.
    pub(crate) fn path_entries(&self, dirs: &[PathBuf], trace: Option<&Trace>) -> Arc<PathEntries> {
        if !self.cache.enabled() {
            return Arc::new(PathEntries::scan(dirs));
        }
        let stamp: Vec<_> = dirs
            .iter()
            .map(|dir| (dir.clone(), Stamp::of(dir)))
            .collect();
        let mut kept = lock(&self.cache.inner.path);
        if let Some(scanned) = kept.as_ref()
            && scanned.stamp == stamp
            && stamp.iter().all(|(_, stamp)| stamp.settled(scanned.at))
        {
            note(trace, Lookup::Hit);
            return Arc::clone(&scanned.entries);
        }
        note(trace, Lookup::Miss);
        let at = SystemTime::now();
        let entries = Arc::new(PathEntries::scan(dirs));
        *kept = Some(ScannedPath {
            stamp,
            at,
            entries: Arc::clone(&entries),
        });
        entries
    }

    /// The entries of the directory `dir`, read again when it was modified. `None` when it
    /// cannot be read.
    pub(crate) fn listing(&self, dir: &Path, trace: Option<&Trace>) -> Option<Arc<[Entry]>> {
        let read = || -> Option<Arc<[Entry]>> {
            Some(
                fs::read_dir(dir)
                    .ok()?
                    .filter_map(Result::ok)
                    .map(|entry| Entry {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        is_dir: entry.path().is_dir(),
                    })
                    .collect(),
            )
        };
        if !self.cache.enabled() {
            return read();
        }

        let stamp = Stamp::of(dir);
        {
            let mut listings = lock(&self.cache.inner.listings);
            if let Some(listing) = listings.get(dir)
                && listing.stamp == stamp
                && stamp.settled(listing.at)
            {
                note(trace, Lookup::Hit);
                return Some(Arc::clone(&listing.entries));
            }
        }
        note(trace, Lookup::Miss);
        let at = SystemTime::now();
        let entries = read()?;
        lock(&self.cache.inner.listings).put(
            dir.to_path_buf(),
            Listing {
                stamp,
                at,
                entries: Arc::clone(&entries),
            },
        );
        Some(entries)
    }

    /// The names that `key`'s closure listed, if they are still fresh. They are not tied to the
    /// environment: they are what another machine said.
    pub(crate) fn listed(&self, key: &SourceKey, trace: Option<&Trace>) -> Option<Names> {
        if !self.cache.enabled() {
            return None;
        }
        let mut sources = lock(&self.cache.inner.sources);
        let fresh = sources
            .get(key)
            .filter(|listed| listed.at.elapsed() < listed.names.lifetime())
            .map(|listed| listed.names.clone());
        if fresh.is_none() {
            sources.pop(key);
        }
        note(
            trace,
            if fresh.is_some() {
                Lookup::Hit
            } else {
                Lookup::Miss
            },
        );
        fresh
    }

    /// Keeps `names` as what `key`'s closure listed.
    pub(crate) fn remember_listed(&self, key: SourceKey, names: Names) {
        if self.cache.enabled() {
            lock(&self.cache.inner.sources).put(
                key,
                Listed {
                    names,
                    at: Instant::now(),
                },
            );
        }
    }

    /// What evaluating the pipeline `text` came to in this environment, if that is still fresh.
    pub(crate) fn evaluation(&self, text: &str) -> Option<Read> {
        if !self.cache.enabled() {
            return None;
        }
        let key = (self.env, text.to_owned());
        let mut evaluations = lock(&self.cache.inner.evaluations);
        let fresh = evaluations
            .get(&key)
            .filter(|kept| kept.at.elapsed() < EVALUATION_LIFETIME)
            .map(|kept| kept.read.clone());
        if fresh.is_none() {
            evaluations.pop(&key);
        }
        fresh
    }

    /// Keeps `read` as what evaluating the pipeline `text` came to in this environment.
    pub(crate) fn remember_evaluation(&self, text: &str, read: Read) {
        if self.cache.enabled() {
            lock(&self.cache.inner.evaluations).put(
                (self.env, text.to_owned()),
                Evaluated {
                    read,
                    at: Instant::now(),
                },
            );
        }
    }

    /// The custom answer kept for `key` in this environment, if it is still fresh.
    pub(crate) fn answer(&self, key: &AnswerKey, trace: Option<&Trace>) -> Option<Value> {
        if !self.cache.enabled() {
            return None;
        }
        let mut answers = lock(&self.cache.inner.answers);
        answers.live(self.env);
        let fresh = answers
            .entries
            .get(key)
            .filter(|answer| answer.at.elapsed() < ANSWER_LIFETIME)
            .map(|answer| answer.value.clone());
        if fresh.is_none() {
            answers.entries.pop(key);
        }
        note(
            trace,
            if fresh.is_some() {
                Lookup::Hit
            } else {
                Lookup::Miss
            },
        );
        fresh
    }

    /// Keeps `value` as the answer for `key` in this environment.
    pub(crate) fn remember(&self, key: AnswerKey, value: Value) {
        if !self.cache.enabled() {
            return;
        }
        let mut answers = lock(&self.cache.inner.answers);
        answers.live(self.env);
        answers.entries.put(
            key,
            Answer {
                value,
                at: Instant::now(),
            },
        );
    }

    /// How many custom answers are kept for this environment, for tests.
    #[cfg(test)]
    pub(crate) fn answers_kept(&self) -> usize {
        let mut answers = lock(&self.cache.inner.answers);
        answers.live(self.env);
        answers.entries.len()
    }
}

impl Answers {
    /// Forgets the answers of another environment: one stale generation clears in O(1).
    fn live(&mut self, env: CacheEnv) {
        if self.env != Some(env) {
            self.env = Some(env);
            self.entries.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nu_protocol::VarId;

    /// A view over a cache that keeps `size` answers.
    fn view(size: usize) -> CacheView {
        CacheView::new(CompletionCache::new(size), CacheEnv::default())
    }

    /// Sets the modification time of the directory `dir`.
    fn touch(dir: &Path, time: SystemTime) {
        fs::File::open(dir)
            .and_then(|dir| dir.set_modified(time))
            .expect("a directory's modification time can be set");
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        let mut names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn a_directory_is_read_again_only_when_it_was_modified() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        fs::write(dir.path().join("one"), "").expect("a file");
        touch(dir.path(), hour_ago);
        let view = view(10);

        let first = view.listing(dir.path(), None).expect("a listing");
        assert_eq!(names(&first), ["one"]);

        // A change the directory does not show is not seen: the listing is the one kept.
        fs::write(dir.path().join("two"), "").expect("a file");
        touch(dir.path(), hour_ago);
        let again = view.listing(dir.path(), None).expect("a listing");
        assert!(Arc::ptr_eq(&first, &again));

        // One it shows is.
        touch(dir.path(), hour_ago + Duration::from_secs(60));
        let changed = view.listing(dir.path(), None).expect("a listing");
        assert_eq!(names(&changed), ["one", "two"]);
    }

    #[test]
    fn a_directory_modified_just_now_is_not_trusted() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let view = view(10);
        fs::write(dir.path().join("one"), "").expect("a file");
        assert_eq!(
            names(&view.listing(dir.path(), None).expect("a listing")),
            ["one"]
        );
        // Within the clock's resolution of the first read, so its time may not change.
        fs::write(dir.path().join("two"), "").expect("a file");
        assert_eq!(
            names(&view.listing(dir.path(), None).expect("a listing")),
            ["one", "two"]
        );
    }

    #[test]
    fn an_unreadable_directory_has_no_listing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        assert!(
            view(10)
                .listing(&dir.path().join("missing"), None)
                .is_none()
        );
    }

    #[test]
    fn nothing_is_kept_when_the_cache_is_off() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        touch(dir.path(), SystemTime::now() - Duration::from_secs(3600));
        let view = view(0);
        let first = view.listing(dir.path(), None).expect("a listing");
        assert!(!Arc::ptr_eq(
            &first,
            &view.listing(dir.path(), None).expect("a listing")
        ));
        let key = AnswerKey {
            block: BlockId::new(1),
            before: Arc::from(""),
        };
        view.remember(key.clone(), Value::test_int(1));
        assert_eq!(view.answer(&key, None), None);
    }

    #[test]
    fn the_path_is_scanned_again_when_a_directory_on_it_changes() {
        let bin = tempfile::tempdir().expect("a temporary directory");
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        fs::write(bin.path().join("tool"), "").expect("a file");
        touch(bin.path(), hour_ago);
        let dirs = vec![bin.path().to_path_buf(), PathBuf::from("/does/not/exist")];
        let view = view(10);

        let first = view.path_entries(&dirs, None);
        assert_eq!(
            first.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            ["tool"]
        );
        assert!(Arc::ptr_eq(&first, &view.path_entries(&dirs, None)));

        fs::write(bin.path().join("other"), "").expect("a file");
        touch(bin.path(), hour_ago + Duration::from_secs(60));
        let mut again: Vec<_> = view
            .path_entries(&dirs, None)
            .iter()
            .map(|(name, path)| (name.to_owned(), path))
            .collect();
        again.sort();
        assert_eq!(
            again,
            [
                ("other".to_owned(), bin.path().join("other")),
                ("tool".to_owned(), bin.path().join("tool"))
            ]
        );

        // Another `PATH` is another question.
        assert!(!Arc::ptr_eq(&first, &view.path_entries(&dirs[..1], None)));
    }

    #[test]
    fn answers_are_valid_for_one_environment_and_are_forgotten_in_least_used_order() {
        let cache = CompletionCache::new(2);
        let here = CacheView::new(cache.clone(), CacheEnv::default());
        let key = |before: &str| AnswerKey {
            block: BlockId::new(1),
            before: Arc::from(before),
        };
        here.remember(key("a"), Value::test_int(1));
        here.remember(key("b"), Value::test_int(2));
        assert_eq!(here.answer(&key("a"), None), Some(Value::test_int(1)));
        here.remember(key("c"), Value::test_int(3));
        // `b` was the least recently used.
        assert_eq!(here.answer(&key("b"), None), None);
        assert_eq!(here.answer(&key("c"), None), Some(Value::test_int(3)));

        // Another environment sees none of it, and takes the cache over.
        let elsewhere = CacheView::new(cache, CacheEnv(1));
        assert_eq!(elsewhere.answer(&key("a"), None), None);
        assert_eq!(here.answer(&key("c"), None), None);
    }
    #[test]
    fn the_history_is_read_on_another_thread_when_it_can_be_opened_again() {
        use reedline::{FileBackedHistory, History, HistoryItem};
        fn history(lines: &[&str]) -> FileBackedHistory {
            let mut history = FileBackedHistory::default();
            for line in lines {
                history
                    .save(HistoryItem::from_command_line(*line))
                    .expect("an in-memory history saves");
            }
            history
        }
        let cache = CompletionCache::new(10);
        let editors = history(&["ls", "cd", "ls"]);
        let reader: HistoryReader =
            Arc::new(|| Some(Box::new(history(&["ls", "cd", "ls"])) as Box<dyn reedline::History>));
        assert!(cache.usage().is_none());
        cache.learn_usage(&editors, Some(&reader), |_| false);
        // The index lands once the other thread has read it; nothing waits for it.
        let landed = (0..500).find_map(|_| {
            std::thread::sleep(Duration::from_millis(10));
            cache.usage()
        });
        assert_eq!(landed.map(|usage| usage.len()), Some(3));

        // A history of the same length is not read again; a longer one is.
        let longer = history(&["ls", "cd", "ls", "pwd"]);
        cache.learn_usage(&longer, None, |_| false);
        assert_eq!(cache.usage().map(|usage| usage.len()), Some(4));
    }

    #[test]
    fn a_listing_is_asked_by_the_closure_and_what_it_captured_and_survives_the_environment() {
        let cache = CompletionCache::new(10);
        let view = CacheView::new(cache.clone(), CacheEnv::default());
        let closure = |captured: i64, span: Span| Closure {
            block_id: BlockId::new(3),
            captures: vec![(VarId::new(0), Value::int(captured, span))],
        };
        let key = |captured: i64, span: Span, args: &[&str]| {
            SourceKey::new(&closure(captured, span), args)
        };
        let names = Names {
            list: Arc::from(vec!["one".to_owned()]),
            failure: None,
        };
        view.remember_listed(key(1, Span::new(0, 1), &["."]), names.clone());

        // Another call of the completer makes another closure: equal, though its spans are not.
        let again = view.listed(&key(1, Span::new(10, 20), &["."]), None);
        assert_eq!(again, Some(names));
        // A different capture (another host) or another argument (another directory) is another
        // question.
        assert_eq!(view.listed(&key(2, Span::unknown(), &["."]), None), None);
        assert_eq!(view.listed(&key(1, Span::unknown(), &["etc/"]), None), None);
        // What another machine said does not depend on this one's environment.
        let elsewhere = CacheView::new(cache, CacheEnv(7));
        assert!(
            elsewhere
                .listed(&key(1, Span::unknown(), &["."]), None)
                .is_some()
        );
        // A cache that keeps nothing keeps no listing.
        let off = CacheView::new(CompletionCache::new(0), CacheEnv::default());
        off.remember_listed(key(1, Span::unknown(), &["."]), Names::default());
        assert_eq!(off.listed(&key(1, Span::unknown(), &["."]), None), None);
    }
}
