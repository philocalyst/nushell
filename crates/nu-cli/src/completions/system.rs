//! Values the system holds: the processes that run, for `kill`.
//!
//! What the system lists costs a system call or a directory lookup, so the lists are read at
//! most once in a while (see [`Timed`]) and every request narrows the same list again.

use crate::completions::{Candidates, Context, SemanticSuggestion, to_reedline_span};
use nu_protocol::{Filesize, FilesizeFormatter, SuggestionKind, Type};
use nu_utils::time::Instant;
use reedline::Suggestion;
use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

/// A list read from the system, kept for a while after it was read.
struct Timed<T> {
    lifetime: Duration,
    kept: Mutex<Option<(Instant, Arc<T>)>>,
}

impl<T> Timed<T> {
    const fn new(lifetime: Duration) -> Self {
        Self {
            lifetime,
            kept: Mutex::new(None),
        }
    }

    /// The list, read with `read` when it is older than its lifetime. A request that comes in
    /// while another reads waits for it rather than reading the list again.
    fn get(&self, read: impl FnOnce() -> T) -> Arc<T> {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((read_at, list)) = kept.as_ref()
            && read_at.elapsed() < self.lifetime
        {
            return list.clone();
        }
        let list = Arc::new(read());
        *kept = Some((Instant::now(), list.clone()));
        list
    }
}

/// How long the process list is reused: a process that has just started is found by the next
/// keystroke, and typing a name does not list the processes again for each letter.
static PROCESSES: Timed<Vec<Process>> = Timed::new(Duration::from_secs(2));

/// A running process.
struct Process {
    pid: i32,
    name: String,
    command: String,
    /// Resident memory, in bytes.
    memory: Option<u64>,
    /// Whether the user running the shell owns it, and so can signal it.
    own: bool,
}

/// The running processes, those of the user first, the newest of each first: a process started
/// a moment ago is the likelier one to be stopped.
fn read_processes() -> Vec<Process> {
    let me = current_uid();
    let mut found: Vec<Process> = nu_system::collect_proc(Duration::ZERO, true)
        .into_iter()
        .map(|process| Process {
            pid: process.pid(),
            name: process.name().to_owned(),
            command: process.command().to_owned(),
            memory: process.mem_size(),
            #[cfg(unix)]
            own: me.is_some() && process.user_id() == me,
            #[cfg(not(unix))]
            own: me.is_some(),
        })
        .collect();
    found.sort_by_key(|process| (!process.own, std::cmp::Reverse(process.pid)));
    found
}

/// The id of the user running the shell.
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    Some(nix::unistd::Uid::current().as_raw())
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

/// The processes by id, with the name beside it: the id is what is inserted, and either is
/// what is typed to find it. Whichever the typed text starts like leads the line, so the part
/// that matched is the first thing read.
pub(crate) fn processes(ctx: &Context) -> Candidates {
    let numeric = !ctx.prefix.is_empty() && ctx.prefix.iter().all(u8::is_ascii_digit);
    let span = to_reedline_span(ctx.span, ctx.offset);
    let mut candidates = Candidates {
        ordered: true,
        ..Candidates::default()
    };

    for process in PROCESSES.get(read_processes).iter() {
        let shown = match numeric {
            true => format!("{}  {}", process.pid, process.name),
            false => format!("{}  {}", process.name, process.pid),
        };
        let memory = process.memory.map(|bytes| {
            FilesizeFormatter::new()
                .precision(1)
                .format(Filesize::new(bytes.try_into().unwrap_or(i64::MAX)))
                .to_string()
        });
        // The command line says what the name does not; without one it only repeats it.
        let description = (process.command != process.name).then_some(process.command.as_str());
        let description = [memory.as_deref(), description]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("  ");

        candidates.push_as(
            shown.clone(),
            SemanticSuggestion {
                suggestion: Suggestion {
                    value: process.pid.to_string(),
                    display_override: Some(shown),
                    description: (!description.is_empty()).then_some(description),
                    span,
                    append_whitespace: true,
                    ..Suggestion::default()
                },
                kind: Some(SuggestionKind::Value(Type::Int)),
                ..Default::default()
            },
        );
    }
    candidates
}
