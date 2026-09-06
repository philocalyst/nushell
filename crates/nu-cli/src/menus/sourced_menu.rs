use reedline::{Completer, Editor, Menu, MenuEvent, Painter, Suggestion};
use std::sync::{Arc, Mutex};

/// Shared command line for menu source.
#[derive(Clone, Default)]
pub struct MenuLine(Arc<Mutex<Option<String>>>);

impl MenuLine {
    /// Record current editor line.
    pub(crate) fn record(&self, line: &str) {
        if let Ok(mut recorded) = self.0.lock() {
            *recorded = Some(line.to_string());
        }
    }

    /// Last recorded line.
    pub(crate) fn read(&self) -> Option<String> {
        self.0.lock().ok()?.clone()
    }
}

/// Menu wrapper carrying line to source.
pub struct SourcedMenu<M> {
    menu: M,
    line: MenuLine,
}

impl<M> SourcedMenu<M> {
    pub fn new(menu: M, line: MenuLine) -> Self {
        Self { menu, line }
    }
}

impl<M: Menu> Menu for SourcedMenu<M> {
    fn name(&self) -> &str {
        self.menu.name()
    }

    fn indicator(&self) -> &str {
        self.menu.indicator()
    }

    fn is_active(&self) -> bool {
        self.menu.is_active()
    }

    fn set_active(&mut self, active: bool) {
        self.menu.set_active(active);
    }

    fn clear_input(&mut self) {
        self.menu.clear_input();
    }

    fn on_activate(&mut self) {
        self.menu.on_activate();
    }

    fn on_deactivate(&mut self) {
        self.menu.on_deactivate();
    }

    fn menu_event(&mut self, event: MenuEvent) {
        self.menu.menu_event(event);
    }

    fn can_quick_complete(&self) -> bool {
        self.menu.can_quick_complete()
    }

    fn can_partially_complete(
        &mut self,
        values_updated: bool,
        editor: &mut Editor,
        completer: &mut dyn Completer,
    ) -> bool {
        self.line.record(editor.get_buffer());
        self.menu
            .can_partially_complete(values_updated, editor, completer)
    }

    fn update_values(&mut self, editor: &mut Editor, completer: &mut dyn Completer) {
        self.line.record(editor.get_buffer());
        self.menu.update_values(editor, completer);
    }

    fn reset_position(&mut self) {
        self.menu.reset_position();
    }

    fn reload(&mut self, updated: bool, editor: &mut Editor, completer: &mut dyn Completer) {
        self.line.record(editor.get_buffer());
        self.menu.reload(updated, editor, completer);
    }

    fn update_working_details(
        &mut self,
        editor: &mut Editor,
        completer: &mut dyn Completer,
        painter: &Painter,
    ) {
        self.line.record(editor.get_buffer());
        self.menu.update_working_details(editor, completer, painter);
    }

    fn replace_in_buffer(&self, editor: &mut Editor) {
        self.menu.replace_in_buffer(editor);
    }

    fn menu_required_lines(&self, terminal_columns: u16) -> u16 {
        self.menu.menu_required_lines(terminal_columns)
    }

    fn menu_string(&self, available_lines: u16, use_ansi_coloring: bool) -> String {
        self.menu.menu_string(available_lines, use_ansi_coloring)
    }

    fn min_rows(&self) -> u16 {
        self.menu.min_rows()
    }

    fn get_values(&self) -> &[Suggestion] {
        self.menu.get_values()
    }

    fn results_are_provisional(&self) -> bool {
        self.menu.results_are_provisional()
    }

    fn is_awaiting_first_answer(&self) -> bool {
        self.menu.is_awaiting_first_answer()
    }

    fn set_cursor_pos(&mut self, pos: (u16, u16)) {
        self.menu.set_cursor_pos(pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::{ColumnarMenu, CompletionResult, InputMode, MenuBuilder, UndoBehavior};

    struct Recorder(Arc<Mutex<Vec<(String, usize)>>>);

    impl Completer for Recorder {
        fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
            if let Ok(mut seen) = self.0.lock() {
                seen.push((line.to_string(), pos));
            }
            CompletionResult::fresh(vec![Suggestion {
                value: "alpha".into(),
                ..Suggestion::default()
            }])
        }
    }

    #[test]
    fn a_diff_menu_leaves_the_line_it_is_on() {
        let mut editor = Editor::default();
        editor.edit_buffer(
            |buffer| {
                buffer.set_buffer("ls ".into());
                buffer.set_insertion_point(3);
            },
            UndoBehavior::CreateUndoPoint,
        );

        let line = MenuLine::default();
        let mut menu = SourcedMenu::new(
            ColumnarMenu::default().with_input_mode(InputMode::Diff),
            line.clone(),
        );

        let seen = Arc::new(Mutex::new(Vec::new()));
        menu.update_values(&mut editor, &mut Recorder(Arc::clone(&seen)));

        assert_eq!(line.read().as_deref(), Some("ls "));
        assert_eq!(
            seen.lock().expect("what the menu handed over").as_slice(),
            [(String::new(), 3)]
        );
    }
}
