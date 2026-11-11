use crate::completions::{ExampleIndex, example_preview};
use nu_protocol::{
    Config,
    engine::{EngineState, Stack},
};
use reedline::{Completer, Completions, Preview, Reply, Request, Suggestion};
use std::{
    sync::{Arc, OnceLock},
    thread,
};

/// The help menu: a search through the examples of every command, by what they do or what
/// they say. Taking one puts it on the line in place of the command under the cursor, with
/// the cursor on its first `<placeholder>`, ready to be completed.
///
/// The examples of the command being typed come first, then everyone else's.
pub struct NuHelpCompleter {
    engine_state: Arc<EngineState>,
    stack: Stack,
    /// The examples, built when the menu is first asked and shared with the next prompt's.
    index: Arc<OnceLock<Arc<ExampleIndex>>>,
}

impl NuHelpCompleter {
    pub fn new(engine_state: Arc<EngineState>, _config: Arc<Config>) -> Self {
        Self {
            engine_state,
            stack: Stack::new(),
            index: Arc::default(),
        }
    }
}

impl Completer for NuHelpCompleter {
    fn complete(&mut self, request: Request<'_>, reply: Reply<Completions>) {
        let (line, cursor) = (request.line.to_owned(), request.cursor);
        if let Some(index) = self.index.get() {
            reply.send(index.search(&line, cursor));
            return;
        }
        // Reading the tldr pages takes a moment, which the keystroke does not wait for.
        let (engine_state, slot) = (Arc::clone(&self.engine_state), Arc::clone(&self.index));
        thread::spawn(move || {
            let index = slot.get_or_init(|| ExampleIndex::of(&engine_state));
            if reply.is_current() {
                reply.send(index.search(&line, cursor));
            }
        });
    }

    fn preview(
        &mut self,
        completions: &Completions,
        index: usize,
        width: u16,
        reply: Reply<Preview>,
    ) {
        reply.send(example_preview(
            &self.engine_state,
            &self.stack,
            completions,
            index,
            width,
        ));
    }

    /// The preview of an example is what it gives, so a click on any row of it takes the
    /// example, as accepting it does.
    fn preview_clicked(
        &mut self,
        completions: &Completions,
        index: usize,
        _row: usize,
    ) -> Option<Suggestion> {
        completions.items.get(index).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicking_the_preview_of_an_example_takes_it() {
        let engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
        let config = engine_state.get_config().clone();
        let mut completer = NuHelpCompleter::new(engine_state.into(), config);
        let item = Suggestion {
            value: "git switch <branch>".into(),
            cursor: Some(19),
            ..Suggestion::default()
        };
        let completions = Completions::new(vec![item.clone()]);
        assert_eq!(completer.preview_clicked(&completions, 0, 2), Some(item));
        assert_eq!(completer.preview_clicked(&completions, 1, 0), None);
    }
}
