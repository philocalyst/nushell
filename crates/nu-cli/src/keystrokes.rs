//! Typing into the line editor as the REPL sets it up, with no terminal: the named scenarios of
//! `tests/keystrokes.nuon`, and seeded random walks over words of the differential corpus. After
//! every key, invariants that need no oracle are checked: the cursor is on a character, keys that
//! complete rewrite only the pipeline element at the cursor and never add a second space, taking
//! the same answer twice does not grow the line, Backspace takes an opening back exactly, and the
//! line Enter submits is the line that was shown. A failure prints the shortest keys that replay
//! it.
//!
//! Keys are written as characters and `<tab>`, `<s-tab>`, `<bs>`, `<enter>`, `<left>`, `<right>`,
//! `<end>`. After each key the answers it asked for land, as they do for someone who looks before
//! the next key, unless `<race>` comes before it: then it is pressed before they land.
//! `NU_KEYS_SEEDS` and `NU_KEYS_STEPS` set how many walks run, and how long each is;
//! `NU_KEYS_SHOW` prints what each scenario left.

use crate::{
    completions::CompletionCache,
    eval_source,
    repl::{editor_for_prompt, setup_keybindings},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use nu_config::ConfigFileKind;
use nu_protocol::{
    PipelineData, Span, Value,
    engine::{EngineState, Stack},
};
use reedline::{DefaultPrompt, Reedline, Signal};
use std::{ops::ControlFlow, path::Path, time::Duration};

/// How long a key's answers may take to land.
const SETTLE: Duration = Duration::from_secs(5);

/// The cursor, where a line is written out.
const CURSOR: char = '⌶';

/// A key of a script.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Key {
    Char(char),
    Tab,
    BackTab,
    Backspace,
    Enter,
    Left,
    Right,
    End,
    /// Presses the next key before the answers of this one land.
    Race,
}

const NAMED: [(&str, Key); 8] = [
    ("tab", Key::Tab),
    ("s-tab", Key::BackTab),
    ("bs", Key::Backspace),
    ("enter", Key::Enter),
    ("left", Key::Left),
    ("right", Key::Right),
    ("end", Key::End),
    ("race", Key::Race),
];

/// The keys `script` writes.
fn keys(script: &str) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut rest = script;
    while let Some(c) = rest.chars().next() {
        let named = rest.strip_prefix('<').and_then(|tail| {
            let (name, after) = tail.split_once('>')?;
            let key = NAMED.iter().find(|(known, _)| *known == name)?.1;
            Some((key, after))
        });
        match named {
            Some((key, after)) => (keys.push(key), rest = after),
            None => (keys.push(Key::Char(c)), rest = &rest[c.len_utf8()..]),
        };
    }
    keys
}

/// `keys` written as a script again.
fn script(keys: &[Key]) -> String {
    keys.iter()
        .map(|key| match key {
            Key::Char(c) => c.to_string(),
            key => NAMED
                .iter()
                .find(|(_, named)| named == key)
                .map(|(name, _)| format!("<{name}>"))
                .unwrap_or_default(),
        })
        .collect()
}

fn event(key: Key) -> Option<Event> {
    let code = match key {
        Key::Char(c) => KeyCode::Char(c),
        Key::Tab => KeyCode::Tab,
        Key::BackTab => KeyCode::BackTab,
        Key::Backspace => KeyCode::Backspace,
        Key::Enter => KeyCode::Enter,
        Key::Left => KeyCode::Left,
        Key::Right => KeyCode::Right,
        Key::End => KeyCode::End,
        Key::Race => return None,
    };
    let modifiers = match key {
        Key::BackTab => KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    };
    Some(Event::Key(KeyEvent::new(code, modifiers)))
}

/// The shell's commands and default config, working in `dir`.
fn engine(dir: &Path) -> (EngineState, Stack) {
    let mut engine_state =
        nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
    let mut stack = Stack::new();
    let dir = dir.to_string_lossy().into_owned();
    stack.add_env_var("PWD".into(), Value::string(dir, Span::unknown()));
    for kind in [ConfigFileKind::Env, ConfigFileKind::Config] {
        eval_source(
            &mut engine_state,
            &mut stack,
            kind.default().as_bytes(),
            kind.default_path(),
            PipelineData::empty(),
            false,
        );
    }
    engine_state
        .merge_env(&mut stack)
        .expect("the default env merges");
    (engine_state, stack)
}

/// The line editor of one prompt, and what it was last shown.
struct Driver {
    editor: Reedline,
    prompt: DefaultPrompt,
    /// The lines Enter submitted.
    submitted: Vec<String>,
}

impl Driver {
    fn new(engine_state: &EngineState, stack: &Stack) -> Self {
        let editor = setup_keybindings(engine_state, Reedline::create().headless(100, 30));
        let (editor, _) = editor_for_prompt(
            editor,
            engine_state,
            stack.clone(),
            CompletionCache::default(),
            false,
        );
        Self {
            editor,
            prompt: DefaultPrompt::default(),
            submitted: Vec::new(),
        }
    }

    /// Presses `keys` in one batch, then lets their answers land unless `settle` is off.
    fn press(&mut self, keys: &[Key], settle: bool) {
        let events = keys.iter().copied().filter_map(event).collect();
        let flow = self
            .editor
            .feed(&self.prompt, events)
            .expect("keys are read");
        if let ControlFlow::Break(Signal::Success(line)) = flow {
            self.submitted.push(line);
        }
        if settle {
            self.editor
                .settle(&self.prompt, SETTLE)
                .expect("answers land");
        }
    }

    /// The line, and where the cursor is in it.
    fn state(&self) -> (String, usize) {
        let line = self.editor.current_buffer_contents().to_owned();
        (line, self.editor.current_insertion_point())
    }
}

/// `line` with the cursor drawn in it.
fn drawn(line: &str, cursor: usize) -> String {
    let at = cursor.min(line.len());
    match (line.get(..at), line.get(at..)) {
        (Some(before), Some(after)) => format!("{before}{CURSOR}{after}"),
        _ => format!("{line} (cursor {cursor} is inside a character)"),
    }
}

/// Where the pipeline element the cursor is in starts: after the last `|`, `;`, newline or
/// opening bracket before it.
fn element_start(line: &str, cursor: usize) -> usize {
    line.get(..cursor)
        .and_then(|before| before.rfind(['|', ';', '\n', '(', '{', '[']))
        .map_or(0, |at| at + 1)
}

/// What a key changed: the range of `before` it replaced, and what it put there.
fn change<'a>(before: &str, after: &'a str) -> (std::ops::Range<usize>, &'a str) {
    let prefix = before
        .char_indices()
        .zip(after.chars())
        .find(|((_, a), b)| a != b)
        .map_or(before.len().min(after.len()), |((at, _), _)| at);
    let suffix = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    (
        prefix..before.len() - suffix,
        &after[prefix..after.len() - suffix],
    )
}

/// What the keys pressed so far must not have done.
#[derive(Default)]
struct Watch {
    /// What the last Tab inserted, when it only inserted.
    last_insert: Option<String>,
    /// The line before a space opened something, and the line the space alone would have made.
    opened: Option<(String, String)>,
}

impl Watch {
    /// A fault of pressing `key`, which took the line from `before` to `after`.
    fn check(
        &mut self,
        keys: &[Key],
        (before, at): (&str, usize),
        (after, cursor): (&str, usize),
    ) -> Option<String> {
        let key = *keys.last()?;
        if !after.is_char_boundary(cursor.min(after.len())) || cursor > after.len() {
            return Some(format!("the cursor is off the line: {cursor}"));
        }
        // Typing a space, or deleting what is between two, may leave two side by side.
        let doubles = |line: &str| line.matches("  ").count();
        let own = keys
            .iter()
            .any(|key| matches!(key, Key::Char(' ') | Key::Backspace));
        if !own && doubles(after) > doubles(before) {
            return Some("a second space was added".into());
        }
        let (replaced, inserted) = change(before, after);
        let completes = matches!(key, Key::Tab | Key::BackTab | Key::Enter);
        if completes && !replaced.is_empty() | !inserted.is_empty() {
            let start = element_start(before, at);
            if replaced.start < start {
                return Some(format!(
                    "{key:?} rewrote {:?}, before the element at the cursor",
                    &before[replaced.start..start]
                ));
            }
        }
        // A Tab pressed before its answer takes only what goes on from what is typed.
        if keys.len() > 1 && key == Key::Tab {
            let written = &before[replaced.clone()];
            let unquoted = |text: &str| text.trim_start_matches(['"', '\'', '`']).to_owned();
            if !unquoted(inserted).starts_with(&unquoted(written)) {
                return Some(format!("a Tab before its answer replaced {written:?}"));
            }
        }
        let insert = (key == Key::Tab && replaced.is_empty() && !inserted.is_empty())
            .then(|| inserted.to_owned());
        if insert.is_some() && insert == self.last_insert {
            return Some(format!("Tab inserted {inserted:?} again"));
        }
        self.last_insert = insert;
        if let Some((was, spaced)) = self.opened.take()
            && key == Key::Backspace
            && after != was
            && after != spaced.trim_end()
            && after != spaced
        {
            return Some(format!("Backspace left {after:?}, not {was:?} back"));
        }
        if key == Key::Char(' ') && replaced.is_empty() && inserted.len() > 1 {
            self.opened = Some((before.to_owned(), format!("{before} ")));
        }
        None
    }
}

/// Presses `keys` from an empty line; the first fault, with the line it was found on, if any.
fn run(engine_state: &EngineState, stack: &Stack, keys: &[Key]) -> (Driver, Option<String>) {
    let mut driver = Driver::new(engine_state, stack);
    let mut watch = Watch::default();
    let mut batch = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        if *key == Key::Race {
            continue;
        }
        let (before, at) = driver.state();
        batch.push(*key);
        // A key with `<race>` after it is pressed together with the key after that.
        if keys.get(index + 1) == Some(&Key::Race) {
            continue;
        }
        let pressed = std::mem::take(&mut batch);
        // The line the last key of a batch meets: the characters typed before it, inserted. A
        // batch with another key, or a space that may open something, before its last is not
        // judged.
        let typed: Option<String> = (pressed[..pressed.len() - 1].iter())
            .map(|key| match key {
                Key::Char(c) if *c != ' ' => Some(*c),
                _ => None,
            })
            .collect();
        let Some((before, at)) = typed.and_then(|typed| {
            let mut line = before.clone();
            line.is_char_boundary(at)
                .then(|| line.insert_str(at, &typed))?;
            Some((line, at + typed.len()))
        }) else {
            driver.press(&pressed, true);
            watch = Watch::default();
            continue;
        };
        let submitted = driver.submitted.len();
        driver.press(&pressed, true);
        let (after, cursor) = driver.state();
        if std::env::var_os("NU_KEYS_TRACE").is_some() {
            eprintln!(
                "  {} {:?} → {}",
                script(&pressed),
                drawn(&before, at),
                drawn(&after, cursor)
            );
        }
        if driver.submitted.len() > submitted {
            // A submitted line starts the next prompt empty.
            // Enter runs what is shown, unless a Tab highlighted something to take first.
            let sent = &driver.submitted[submitted];
            let chosen = matches!(
                keys[..index].iter().rev().find(|key| **key != Key::Race),
                Some(Key::Tab | Key::BackTab)
            );
            if *key == Key::Enter && !chosen && sent != &before {
                let fault = format!("Enter submitted {sent:?} for {:?}", drawn(&before, at));
                return (driver, Some(fault));
            }
            watch = Watch::default();
            continue;
        }
        if let Some(fault) = watch.check(&pressed, (&before, at), (&after, cursor)) {
            let fault = format!(
                "{fault}: {} → {}",
                drawn(&before, at),
                drawn(&after, cursor)
            );
            return (driver, Some(fault));
        }
    }
    (driver, None)
}

/// The shortest keys, from `keys`, that still fault.
fn shortest(engine_state: &EngineState, stack: &Stack, keys: &[Key]) -> Vec<Key> {
    let mut keys = keys.to_vec();
    let mut at = 0;
    while at < keys.len() {
        let mut fewer = keys.clone();
        fewer.remove(at);
        if run(engine_state, stack, &fewer).1.is_some() {
            keys = fewer;
        } else {
            at += 1;
        }
    }
    keys
}

/// A demo directory: files, a directory with a space in its name, and a source tree.
fn sandbox() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    for file in [
        "Cargo.toml",
        "README.md",
        "data.csv",
        "src/main.rs",
        "my files/notes.txt",
    ] {
        let path = dir.path().join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a directory");
        }
        std::fs::write(path, "a,b\n1,2\n").expect("a file");
    }
    dir
}

#[test]
fn the_named_scenarios_type_as_they_say() {
    let dir = sandbox();
    let (engine_state, stack) = engine(dir.path());
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/keystrokes.nuon");
    let text = std::fs::read_to_string(&path).expect("the scenarios");
    let scenarios = nuon::from_nuon(&text, None).expect("scenarios are nuon");
    let mut failures = Vec::new();
    for scenario in scenarios.as_list().expect("a list of scenarios") {
        let field = |name: &str| scenario.get_data_by_key(name)?.coerce_into_string().ok();
        let name = field("name").unwrap_or_default();
        let script = field("keys").unwrap_or_default();
        let (driver, fault) = run(&engine_state, &stack, &keys(&script));
        let (line, cursor) = driver.state();
        if std::env::var_os("NU_KEYS_SHOW").is_some() {
            eprintln!(
                "{script:?} → {:?} {:?}",
                drawn(&line, cursor),
                driver.submitted
            );
        }
        let mut faults: Vec<String> = fault.into_iter().collect();
        if let Some(want) = field("line")
            && drawn(&line, cursor) != want
        {
            faults.push(format!(
                "the line is {:?}, not {want:?}",
                drawn(&line, cursor)
            ));
        }
        if let Some(want) = field("submitted")
            && driver.submitted.last() != Some(&want)
        {
            faults.push(format!("submitted {:?}, not {want:?}", driver.submitted));
        }
        for fault in faults {
            failures.push(format!("{name}: {fault}\n    keys: {script}"));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn random_typing_keeps_the_invariants() {
    let seeds = std::env::var("NU_KEYS_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse().ok())
        .unwrap_or(4u64);
    let steps = std::env::var("NU_KEYS_STEPS")
        .ok()
        .and_then(|steps| steps.parse().ok())
        .unwrap_or(40usize);
    let dir = sandbox();
    let (engine_state, stack) = engine(dir.path());
    let corpus = include_str!("../tests/completions/differential/corpus.nu");
    let mut words: Vec<&str> = corpus
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(|line| line.split(|c: char| c.is_whitespace() || c == '"'))
        .filter(|word| !word.is_empty() && word.len() < 20 && !word.contains('⌶'))
        .collect();
    words.sort_unstable();
    words.dedup();
    let mut failures = Vec::new();
    for seed in 1..=seeds {
        // xorshift: the same walk for the same seed, everywhere.
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = |below: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % below.max(1) as u64) as usize
        };
        let mut walk = Vec::new();
        for _ in 0..steps {
            match next(12) {
                0..=4 => {
                    walk.extend(words[next(words.len())].chars().map(Key::Char));
                    walk.push(Key::Char(' '));
                }
                5 | 6 => walk.push(Key::Tab),
                7 => walk.extend([Key::Race, Key::Tab]),
                8 => walk.push([Key::BackTab, Key::Left, Key::Right, Key::End][next(4)]),
                9 | 10 => walk.push(Key::Backspace),
                _ => walk.push(if next(4) == 0 { Key::Enter } else { Key::Tab }),
            }
        }
        if run(&engine_state, &stack, &walk).1.is_some() {
            let short = shortest(&engine_state, &stack, &walk);
            let fault = run(&engine_state, &stack, &short).1.unwrap_or_default();
            failures.push(format!(
                "seed {seed}: {fault}\n    keys: {}",
                script(&short)
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
