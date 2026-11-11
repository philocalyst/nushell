use crate::{LanguageServer, span_to_range, uri_to_path};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionList,
    CompletionParams, CompletionResponse, CompletionTextEdit, Documentation, InsertTextFormat,
    MarkupContent, MarkupKind, Range, TextEdit,
};
use nu_cli::{CompletionEngine, SemanticSuggestion, SuggestionKind, preview_path};
use nu_protocol::{
    DeclId, PositionalArg, Span, SyntaxShape, Type,
    engine::{CommandType, EngineState, Stack},
};
use nu_utils::strip_ansi_unlikely;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A description longer than this many characters (or spanning several lines) is not shown as
/// the item's `detail`, which editors render on a single line. It goes to the documentation.
const MAX_DETAIL_CHARS: usize = 80;

/// Width, in columns, of the file and directory previews shown as documentation.
const PREVIEW_WIDTH: u16 = 80;

/// What `completionItem/resolve` needs to document an item, carried in [`CompletionItem::data`].
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ResolveData {
    /// A command known to the language server's base engine state.
    Decl(usize),
    /// The absolute path of a file or directory.
    Path(String),
}

/// Whether nu-cli stopped collecting `PATH` executables early for `results`.
///
/// `$env.config.completions.external.max_results` caps only the external command names found
/// on `PATH` (not the output of an external completer closure), so the list is cut short
/// exactly when it holds that many of them. A negative cap wraps around to unlimited in nu-cli.
fn externals_capped(max_results: i64, results: &[SemanticSuggestion]) -> bool {
    usize::try_from(max_results).is_ok_and(|max| {
        let externals = results
            .iter()
            .filter(|r| {
                matches!(
                    r.kind,
                    Some(SuggestionKind::Command(CommandType::External, _))
                )
            })
            .count();
        max > 0 && externals >= max
    })
}

/// The `sortText` of the item ranked `rank` of `total`: zero-padded, so that sorting it as text
/// gives back the order nushell ranked the results in.
fn sort_text(rank: usize, total: usize) -> String {
    let width = total.to_string().len().max(4);
    format!("{rank:0width$}")
}

/// `text` without ANSI styling, trimmed.
fn plain(text: &str) -> String {
    strip_ansi_unlikely(text).trim().to_owned()
}

/// `text` as a Markdown code block, fenced so that backticks in `text` cannot end it early.
fn fenced(text: &str) -> String {
    let longest_run = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat((longest_run + 1).max(3));
    format!("{fence}\n{text}\n{fence}")
}

impl LanguageServer {
    pub(crate) fn complete(&mut self, params: &CompletionParams) -> Option<CompletionResponse> {
        let path_uri = &params.text_document_position.text_document.uri;
        let docs = self.docs.lock().ok()?;
        let file = docs.get_document(path_uri)?;
        let file_text = file.get_content(None).to_owned();
        let location = file_text.floor_char_boundary(
            (file.offset_at(params.text_document_position.position) as usize).min(file_text.len()),
        );
        drop(docs);
        let file_path = uri_to_path(path_uri);
        let filename = file_path.to_str()?;
        // One parse of the whole file, probed at the cursor, says what may be written there.
        let engine_state = self.new_engine_state(Some(path_uri));
        let results = CompletionEngine::new(&engine_state, &Stack::new())
            .fetch_completions_within_file(filename, location, &file_text);
        if results.is_empty() {
            return None;
        }

        let is_incomplete = externals_capped(
            engine_state.get_config().completions.external.max_results,
            &results,
        );
        let docs = self.docs.lock().ok()?;
        let file = docs.get_document(path_uri)?;
        let total = results.len();
        let base_decls = self.initial_engine_state.num_decls();
        let cwd = engine_state
            .cwd(None)
            .ok()
            .map(|cwd| cwd.into_std_path_buf());
        let items: Vec<CompletionItem> = results
            .into_iter()
            .enumerate()
            .map(|(rank, r)| {
                let reedline_span = r.suggestion.span;
                let mut item = Self::completion_item_from_suggestion(
                    &engine_state,
                    base_decls,
                    cwd.as_deref(),
                    r,
                    span_to_range(&Span::new(reedline_span.start, reedline_span.end), file, 0),
                );
                // Editors sort by `sortText`, not by arrival, and would discard nushell's ranking.
                item.sort_text = Some(sort_text(rank, total));
                item
            })
            .collect();
        // An array is a list that is known to be complete.
        Some(if is_incomplete {
            CompletionResponse::List(CompletionList {
                is_incomplete,
                items,
            })
        } else {
            CompletionResponse::Array(items)
        })
    }

    /// Answers `completionItem/resolve`: documents `item` from what [`ResolveData`] in its `data`
    /// points at, which was too costly to compute for every item of the completion response.
    pub(crate) fn resolve_completion_item(
        &mut self,
        item: &CompletionItem,
    ) -> Option<CompletionItem> {
        let mut item = item.clone();
        let data = item
            .data
            .clone()
            .and_then(|data| serde_json::from_value::<ResolveData>(data).ok());
        let documentation = match data {
            // Ids past the base engine state were defined by the file; nothing stable to look up.
            Some(ResolveData::Decl(id)) if id < self.initial_engine_state.num_decls() => {
                let decl = self.initial_engine_state.get_decl(DeclId::new(id));
                // The description is the item's detail, unless it was too long for one.
                Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: Self::get_decl_description(decl, item.detail.is_some()),
                }))
            }
            Some(ResolveData::Path(path)) => {
                let rows =
                    preview_path(&self.initial_engine_state, Path::new(&path), PREVIEW_WIDTH);
                (!rows.is_empty()).then(|| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: fenced(&rows.join("\n")),
                    })
                })
            }
            _ => None,
        };
        item.documentation = documentation.or(item.documentation);
        Some(item)
    }

    /// The completion item for `suggestion`, whose replacement is `range`.
    ///
    /// Documentation is not computed here, only what `completionItem/resolve` will need to do
    /// it; `base_decls` is the number of commands in the base engine state, whose ids stay valid
    /// between requests, and `cwd` the directory relative paths are completed in.
    fn completion_item_from_suggestion(
        engine_state: &EngineState,
        base_decls: usize,
        cwd: Option<&Path>,
        suggestion: SemanticSuggestion,
        range: Range,
    ) -> CompletionItem {
        let kind = suggestion.kind;
        let chose_spacing = suggestion.chose_spacing;
        let suggestion = suggestion.suggestion;
        let detail = suggestion.description.as_deref().map(plain).filter(|d| {
            !d.is_empty() && !d.contains('\n') && d.chars().count() <= MAX_DETAIL_CHARS
        });
        // A description too long for the detail, and the extra lines, are plain documentation.
        let mut doc_parts: Vec<String> = Vec::new();
        if detail.is_none() {
            doc_parts.extend(suggestion.description.as_deref().map(plain));
        }
        doc_parts.extend(suggestion.extra.iter().flatten().map(|line| plain(line)));
        doc_parts.retain(|part| !part.is_empty());
        let mut documentation = (!doc_parts.is_empty()).then(|| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::PlainText,
                value: doc_parts.join("\n"),
            })
        });
        let mut data = None;
        let mut snippet_text = suggestion.value.clone();
        let mut insert_text_format = None;
        let mut idx = 0;
        match &kind {
            // use snippet as `insert_text_format` for command argument completion
            Some(SuggestionKind::Command(_, Some(decl_id))) => {
                // NOTE: for new commands defined in current context,
                // which are not present in the engine state, skip the snippet.
                if engine_state.num_decls() > decl_id.get() {
                    let cmd = engine_state.get_decl(*decl_id);
                    if decl_id.get() < base_decls {
                        data = serde_json::to_value(ResolveData::Decl(decl_id.get())).ok();
                    } else {
                        // Can't be looked up by id later, but there are few such commands.
                        documentation = Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: Self::get_decl_description(cmd, detail.is_some()),
                        }));
                    }
                    insert_text_format = Some(InsertTextFormat::SNIPPET);
                    let signature = cmd.signature();
                    // add curly brackets around block arguments
                    // and keywords, e.g. `=` in `alias foo = bar`
                    let mut arg_wrapper =
                        |arg: &PositionalArg, text: String, optional: bool| -> String {
                            idx += 1;
                            match &arg.shape {
                                SyntaxShape::Block | SyntaxShape::MatchBlock => {
                                    format!("{{ ${{{idx}:{text}}} }}")
                                }
                                SyntaxShape::Keyword(kwd, _) => {
                                    // NOTE: If optional, the keyword should also be in a placeholder so that it can be removed easily.
                                    // Here we choose to use nested placeholders. Note that some editors don't fully support this format,
                                    // but usually they will simply ignore the inner ones, so it should be fine.
                                    if optional {
                                        idx += 1;
                                        format!(
                                            "${{{}:{} ${{{}:{}}}}}",
                                            idx - 1,
                                            String::from_utf8_lossy(kwd),
                                            idx,
                                            text
                                        )
                                    } else {
                                        format!(
                                            "{} ${{{}:{}}}",
                                            String::from_utf8_lossy(kwd),
                                            idx,
                                            text
                                        )
                                    }
                                }
                                _ => format!("${{{idx}:{text}}}"),
                            }
                        };

                    for required in signature.required_positional {
                        snippet_text.push(' ');
                        snippet_text.push_str(
                            arg_wrapper(&required, required.name.clone(), false).as_str(),
                        );
                    }
                    for optional in signature.optional_positional {
                        snippet_text.push(' ');
                        snippet_text.push_str(
                            arg_wrapper(&optional, format!("{}?", optional.name), true).as_str(),
                        );
                    }
                    if let Some(rest) = signature.rest_positional {
                        idx += 1;
                        snippet_text.push_str(format!(" ${{{}:...{}}}", idx, rest.name).as_str());
                    }
                }
            }
            // The unescaped path is the display; the value is quoted when it needs to be.
            Some(SuggestionKind::File | SuggestionKind::Directory) => {
                if let Some(cwd) = cwd {
                    let path = nu_path::expand_path_with(suggestion.display_value(), cwd, true);
                    data = serde_json::to_value(ResolveData::Path(
                        path.to_string_lossy().into_owned(),
                    ))
                    .ok();
                }
            }
            _ => {}
        }
        // No extra space for a command with args expanded in the snippet. The space the shell
        // adds after any finished argument is the line editor's; an editor gets one after a
        // command, a flag, an operator or a module, or where a completer asks for it.
        let spaced_here = chose_spacing
            || matches!(
                kind,
                Some(
                    SuggestionKind::Command(..)
                        | SuggestionKind::Flag
                        | SuggestionKind::Operator
                        | SuggestionKind::Module
                )
            );
        if idx == 0 && suggestion.append_whitespace && spaced_here {
            snippet_text.push(' ');
        }

        let text_edit = Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: snippet_text,
        }));

        CompletionItem {
            label: plain(suggestion.display_value()),
            label_details: kind.as_ref().map(|kind| CompletionItemLabelDetails {
                detail: None,
                description: Some(Self::kind_description(kind)),
            }),
            detail,
            documentation,
            kind: kind.as_ref().map(Self::lsp_completion_item_kind),
            // What the editor matches the typed text against. The replaced range covers the
            // token as written, which is the value, not the label or the snippet.
            filter_text: Some(suggestion.value),
            text_edit,
            insert_text_format,
            data,
            ..Default::default()
        }
    }

    /// The short name of what a suggestion is, shown next to its label.
    fn kind_description(kind: &SuggestionKind) -> String {
        match kind {
            SuggestionKind::Value(t) => t.to_string(),
            SuggestionKind::Command(cmd, _) => cmd.to_string(),
            SuggestionKind::CellPath => "cell path".to_string(),
            SuggestionKind::Directory => "directory".to_string(),
            SuggestionKind::File => "file".to_string(),
            SuggestionKind::Flag => "flag".to_string(),
            SuggestionKind::Module => "module".to_string(),
            SuggestionKind::Operator => "operator".to_string(),
            SuggestionKind::Variable => "variable".to_string(),
        }
    }

    fn lsp_completion_item_kind(suggestion_kind: &SuggestionKind) -> CompletionItemKind {
        match suggestion_kind {
            SuggestionKind::Value(t) => match t {
                Type::Bool | Type::Nothing => CompletionItemKind::CONSTANT,
                Type::Filesize | Type::Duration => CompletionItemKind::UNIT,
                Type::Record(_) | Type::Table(_) => CompletionItemKind::STRUCT,
                Type::CellPath => CompletionItemKind::PROPERTY,
                _ => CompletionItemKind::VALUE,
            },
            SuggestionKind::CellPath => CompletionItemKind::PROPERTY,
            SuggestionKind::Command(c, _) => match c {
                CommandType::Keyword => CompletionItemKind::KEYWORD,
                CommandType::Builtin | CommandType::Plugin => CompletionItemKind::FUNCTION,
                CommandType::Alias => CompletionItemKind::REFERENCE,
                CommandType::External => CompletionItemKind::INTERFACE,
                CommandType::Custom => CompletionItemKind::METHOD,
            },
            SuggestionKind::Directory => CompletionItemKind::FOLDER,
            SuggestionKind::File => CompletionItemKind::FILE,
            SuggestionKind::Flag => CompletionItemKind::FIELD,
            SuggestionKind::Module => CompletionItemKind::MODULE,
            SuggestionKind::Operator => CompletionItemKind::OPERATOR,
            SuggestionKind::Variable => CompletionItemKind::VARIABLE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ResolveData, externals_capped, sort_text};
    use crate::tests::{initialize_language_server, open, open_unchecked, result_from_message};
    use crate::{LanguageServer, path_to_uri};
    use assert_json_diff::assert_json_include;
    use lsp_server::{Connection, Message};
    use lsp_types::{
        CompletionItemKind, CompletionParams, PartialResultParams, Position,
        TextDocumentIdentifier, TextDocumentPositionParams, Uri, WorkDoneProgressParams,
        request::{Completion, Request, ResolveCompletionItem},
    };
    use nu_cli::{SemanticSuggestion, SuggestionKind};
    use nu_protocol::{Type, engine::CommandType};
    use nu_test_support::{fs::Stub, prelude::*};
    use rstest::rstest;

    fn send_complete_request(
        client_connection: &Connection,
        uri: Uri,
        line: u32,
        character: u32,
    ) -> Message {
        client_connection
            .sender
            .send(Message::Request(lsp_server::Request {
                id: 2.into(),
                method: Completion::METHOD.to_string(),
                params: serde_json::to_value(CompletionParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri },
                        position: Position { line, character },
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: None,
                })
                .unwrap(),
            }))
            .unwrap();

        client_connection
            .receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
    }

    /// A completion item as an editor reads it: its label and kind, and the text it puts over
    /// `columns` of `line`.
    fn item(
        label: &str,
        description: &str,
        new_text: &str,
        line: u32,
        columns: (u32, u32),
        kind: u32,
    ) -> serde_json::Value {
        let at = |character| serde_json::json!({ "line": line, "character": character });
        serde_json::json!({
            "label": label,
            "labelDetails": { "description": description },
            "textEdit": { "newText": new_text, "range": { "start": at(columns.0), "end": at(columns.1) } },
            "kind": kind
        })
    }

    #[rstest]
    #[case::variable("var.nu", (2, 9), None,
        serde_json::json!([item("$greeting", "variable", "$greeting", 2, (5, 9), 6)])
    )]
    #[case::local_variable("var.nu", (5, 10), None,
        serde_json::json!([item("$bar", "variable", "$bar", 5, (7, 10), 6)])
    )]
    #[case::keyword("keyword.nu", (0, 2), None,
        serde_json::json!([item("overlay", "keyword", "overlay ", 0, (0, 2), 14)])
    )]
    #[case::cell_path_first("cell_path.nu", (1, 5), None, serde_json::json!([
        {
            "label": "\"1\"",
            "detail": "string",
            "textEdit": {
                "newText": "\"1\"",
                "range": { "start": { "line": 1, "character": 5 }, "end": { "line": 1, "character": 5 } }
            },
            "kind": 10
        }
    ]))]
    #[case::cell_path_second("cell_path.nu", (1, 10), None, serde_json::json!([
        {
            "label": "baz",
            "detail": "int",
            "textEdit": {
                "newText": "baz",
                "range": { "start": { "line": 1, "character": 10 }, "end": { "line": 1, "character": 10 } }
            },
            "kind": 10
        }
    ]))]
    #[case::command_with_line("utf_pipeline.nu", (0, 13), None, serde_json::json!([
        {
            "label": "str trim",
            "filterText": "str trim",
            "sortText": "0000",
            "labelDetails": { "description": "built-in" },
            "detail": "Trim whitespace or specific character.",
            "textEdit": {
                "range": { "start": { "line": 0, "character": 8 }, "end": { "line": 0, "character": 13 }, },
                "newText": "str trim ${1:...rest}"
            },
            "insertTextFormat": 2,
            "kind": 3
        }
    ]))]
    #[case::external_completer(
        "external.nu", (0, 11),
        Some("$env.config.completions.external.completer = {|input| ['--background']}"),
        serde_json::json!([{
            "label": "--background",
            "labelDetails": { "description": "string" },
            "textEdit": {
                "newText": "--background",
                "range": { "start": { "line": 0, "character": 5 }, "end": { "line": 0, "character": 11 } }
            },
        }])
    )]
    #[case::fallback_beginning("fallback.nu", (0, 0), None, serde_json::json!([
        {
            "label": "alias",
            "labelDetails": { "description": "keyword" },
            "detail": "Alias a command (with optional flags) to a new name.",
            "textEdit": {
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 }, },
                "newText": "alias ${1:name} = ${2:initial_value}"
            },
            "insertTextFormat": 2,
            "kind": 14
        }
    ]))]
    #[case::fallback_whitespace("fallback.nu", (3, 2), None, serde_json::json!([
        {
            "label": "alias",
            "labelDetails": { "description": "keyword" },
            "detail": "Alias a command (with optional flags) to a new name.",
            "textEdit": {
                "range": { "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 2 }, },
                "newText": "alias ${1:name} = ${2:initial_value}"
            },
            "insertTextFormat": 2,
            "kind": 14
        }
    ]))]
    // Equality is offered first, then inequality.
    #[case::operator_not_equal("fallback.nu", (7, 10), None,
        serde_json::json!([item("==", "operator", "== ", 7, (10, 10), 24)])
    )]
    #[case::operator_not_has("fallback.nu", (7, 15), None,
        serde_json::json!([item("not-has", "operator", "not-has ", 7, (10, 15), 24)])
    )]
    #[case::use_module("use.nu", (4, 17), None,
        serde_json::json!([item("std-rfc", "module", "std-rfc", 4, (11, 17), 9)])
    )]
    #[case::use_null_device("use.nu", (6, 14), None,
        serde_json::json!([item("null_device", "variable", "null_device", 6, (8, 14), 6)])
    )]
    #[case::use_foo("use.nu", (7, 13), None,
        serde_json::json!([item("foo", "variable", "foo", 7, (11, 14), 6)])
    )]
    #[case::use_submodule("use.nu", (20, 4), None,
        serde_json::json!([
            item("dirs", "custom", "dirs ", 20, (0, 4), 2),
            item("dirs next", "custom", "dirs next ", 20, (0, 4), 2),
        ])
    )]
    #[case::command_prefix_ignores_untyped_argument("command.nu", (0, 6), None, serde_json::json!([
        { "label": "config", "detail": "Edit nushell configuration files.", "kind": 3 },
        {
            "label": "config nu",
            "detail": "Edit nu configurations.",
            "textEdit": { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 6 } },
                "newText": "config nu "
            }
        }
    ]))]
    #[case::command_multiword_prefix("command.nu", (0, 8), None, serde_json::json!([
        { "label": "config n foo bar", "detail": "detail", "kind": 2 },
        {
            "label": "config nu",
            "detail": "Edit nu configurations.",
            "textEdit": { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 8 } },
                "newText": "config nu "
            }
        }
    ]))]
    #[case::command_fallback("command.nu", (13, 9), None, serde_json::json!([
        { "label": "config n foo bar", "detail": "detail", "kind": 2 }
    ]))]
    #[case::short_flag_name("command.nu", (15, 17), None, serde_json::json!([
        {
            "label": "-e",
            "detail": "Byte encode endian, available options: native(default), little, big.",
            "textEdit": {
                "newText": "-e ",
                "range": { "start": { "character": 15, "line": 15 }, "end": { "character": 17, "line": 15 } }
            },
            "kind": 5 }
    ]))]
    #[case::short_flag_value("command.nu", (15, 20), None, serde_json::json!([
        { "label": "big", "kind": 12 }
    ]))]
    #[case::long_flag_name("command.nu", (15, 30), None, serde_json::json!([
        {
            "label": "--endian",
            "detail": "Byte encode endian, available options: native(default), little, big.",
            "textEdit": {
                "newText": "--endian ",
                "range": { "start": { "character": 22, "line": 15 }, "end": { "character": 30, "line": 15 } }
            },
            "kind": 5 }
    ]))]
    // The shortest of the files that start with `c`.
    #[case::fallback_file_path("fallback.nu", (5, 4), None,
        serde_json::json!([item("custom.nu", "file", "custom.nu", 5, (3, 4), 17)])
    )]
    // What `sudo` runs is a program, never a Nushell command, so an empty head
    // after it lists programs; with none on the test `PATH` nothing is offered, as a file
    // is only a plausible head when it is written as a path (`./x`).
    #[case::external_fallback(
        "external.nu", (0, 5),
        Some("$env.config.completions.external.completer = {|input| ['--background']}"),
        serde_json::json!(null)
    )]
    // The cursor is inside `bar`, so the last token is the typed prefix `b`, not `baz`.
    #[case::command_wide_custom(
        "command.nu", (23, 5), None,
        serde_json::json!([item("b", "string", "b", 23, (4, 7), 12)])
    )]
    #[case::command_wide_external(
        "command.nu", (28, 8),
        Some("$env.config.completions.external.completer = {|input| ['text']}"),
        serde_json::json!([item("text", "string", "text", 28, (8, 8), 12)])
    )]
    #[case::attributable_command_with_snippet(
        "command.nu", (21, 0), None,
        serde_json::json!([item("def", "keyword", "def ${1:def_name} ${2:params} ${3:block}", 21, (0, 0), 14)])
    )]
    #[case::custom_completion_with_position_and_span(
        "custom.nu", (13, 15), None,
        serde_json::json!([item("foo", "string", "foo", 13, (14, 15), 12)])
    )]
    #[case::custom_completion_with_position_and_span_on_fallback(
        "custom.nu", (13, 17), None,
        serde_json::json!([item("foo", "string", "foo", 13, (16, 17), 12)])
    )]
    #[case::custom_completion_with_position_and_span_on_flag_value(
        "custom.nu", (13, 30), None,
        serde_json::json!([item("foo", "string", "foo", 13, (29, 30), 12)])
    )]
    fn completion_single_request(
        #[case] filename: &str,
        #[case] cursor_position: (u32, u32),
        #[case] config: Option<&str>,
        #[case] expected: serde_json::Value,
    ) {
        let (client_connection, _recv) = initialize_language_server(config, None);

        let mut script = FIXTURES.clone();
        script.push("lsp/completion");
        script.push(filename);
        let script = path_to_uri(&script);

        open_unchecked(&client_connection, script.clone());
        let (line, character) = cursor_position;
        let resp = send_complete_request(&client_connection, script, line, character);

        assert_json_include!(
            actual: result_from_message(resp),
            expected: expected
        );
    }

    /// Sends `completionItem/resolve` for `item` and returns the resolved item.
    fn resolve(client_connection: &Connection, item: &serde_json::Value) -> serde_json::Value {
        client_connection
            .sender
            .send(Message::Request(lsp_server::Request {
                id: 3.into(),
                method: ResolveCompletionItem::METHOD.to_string(),
                params: item.clone(),
            }))
            .unwrap();
        result_from_message(
            client_connection
                .receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
        )
    }

    /// Completes at `cursor` in an unsaved document with `text`, located in the fixtures
    /// directory, so that the text is all the server knows about it.
    fn complete_text(config: Option<&str>, text: &str, cursor: (u32, u32)) -> serde_json::Value {
        let dir = FIXTURES.join("lsp/completion");
        let (client_connection, _recv) = initialize_language_server(config, None);
        complete_text_in(&client_connection, &dir, text, cursor)
    }

    /// Like [`complete_text`], for a document in `dir` on a server that is already running.
    fn complete_text_in(
        client_connection: &Connection,
        dir: &std::path::Path,
        text: &str,
        (line, character): (u32, u32),
    ) -> serde_json::Value {
        let uri = path_to_uri(dir.join("unsaved.nu"));
        open(client_connection, uri.clone(), Some(text.to_owned())).unwrap();
        result_from_message(send_complete_request(
            client_connection,
            uri,
            line,
            character,
        ))
    }

    /// The `label`s of the items of a completion response, in order.
    fn labels(response: &serde_json::Value) -> Vec<&str> {
        response
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["label"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn sort_text_is_the_rank_nushell_returned() {
        let response = complete_text(None, "overlay", (0, 2));
        let items = response.as_array().unwrap();
        assert!(items.len() > 2, "{response}");
        for (rank, item) in items.iter().enumerate() {
            assert_eq!(item["sortText"], format!("{rank:04}"), "{item}");
        }
    }

    #[test]
    fn sort_text_is_zero_padded_and_widens_with_the_list() {
        assert_eq!(sort_text(0, 3), "0000");
        assert_eq!(sort_text(42, 100), "0042");
        assert_eq!(sort_text(9_999, 10_000), "09999");
        assert_eq!(sort_text(10_000, 10_001), "10000");
        let mut texts: Vec<String> = (0..12_000).map(|rank| sort_text(rank, 12_000)).collect();
        let sorted = texts.clone();
        texts.sort();
        assert_eq!(texts, sorted);
    }

    #[test]
    fn completes_in_the_file_after_punctuation() {
        // Nothing before `(` completes as a prefix, yet the whole file knows `ls` follows,
        // so it is the token replaced.
        let response = complete_text(None, "(ls)", (0, 1));
        assert_json_include!(
            actual: response,
            expected: serde_json::json!([{
                "label": "alias",
                "textEdit": { "range": {
                    "start": { "line": 0, "character": 1 },
                    "end": { "line": 0, "character": 3 }
                } },
            }])
        );
    }

    #[rstest]
    #[case::def_parameter(
        "def foo [bar: int] {\n  echo ($ba)\n}", (1, 11), "$bar", (8, 11)
    )]
    #[case::closure_parameter("[1 2] | each {|it| ($i) }", (0, 22), "$it", (20, 22))]
    fn local_variables_come_from_the_text_before_the_cursor(
        #[case] text: &str,
        #[case] cursor: (u32, u32),
        #[case] label: &str,
        #[case] columns: (u32, u32),
    ) {
        let response = complete_text(None, text, cursor);
        assert!(labels(&response).contains(&label), "{response}");
        let item = response
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["label"] == label)
            .unwrap();
        assert_eq!(
            item["textEdit"]["range"]["start"]["character"], columns.0,
            "{item}"
        );
        assert_eq!(
            item["textEdit"]["range"]["end"]["character"], columns.1,
            "{item}"
        );
    }

    #[test]
    fn after_whitespace_the_following_token_is_not_replaced() {
        // Read within the whole file, the cursor would take `foo` for the token to complete.
        // `ls` takes a path, so the gap is offered the files around it.
        let response = complete_text(None, "ls  foo", (0, 3));
        let first = &response.as_array().unwrap()[0];
        assert_eq!(
            first["textEdit"]["range"]["start"]["character"], 3,
            "{first}"
        );
        assert_eq!(first["textEdit"]["range"]["end"]["character"], 3, "{first}");
    }

    #[test]
    fn a_flag_being_written_before_an_argument_is_completed() {
        let response = complete_text(None, "str trim -- foo", (0, 11));
        assert!(labels(&response).contains(&"--char"), "{response}");
    }

    #[rstest]
    #[case::string(SuggestionKind::Value(Type::String), CompletionItemKind::VALUE)]
    #[case::int(SuggestionKind::Value(Type::Int), CompletionItemKind::VALUE)]
    #[case::bool(SuggestionKind::Value(Type::Bool), CompletionItemKind::CONSTANT)]
    #[case::filesize(SuggestionKind::Value(Type::Filesize), CompletionItemKind::UNIT)]
    #[case::duration(SuggestionKind::Value(Type::Duration), CompletionItemKind::UNIT)]
    #[case::record(
        SuggestionKind::Value(Type::Record(Default::default())),
        CompletionItemKind::STRUCT
    )]
    #[case::cell_path_value(SuggestionKind::Value(Type::CellPath), CompletionItemKind::PROPERTY)]
    #[case::cell_path(SuggestionKind::CellPath, CompletionItemKind::PROPERTY)]
    #[case::builtin(
        SuggestionKind::Command(CommandType::Builtin, None),
        CompletionItemKind::FUNCTION
    )]
    #[case::plugin(
        SuggestionKind::Command(CommandType::Plugin, None),
        CompletionItemKind::FUNCTION
    )]
    #[case::custom(
        SuggestionKind::Command(CommandType::Custom, None),
        CompletionItemKind::METHOD
    )]
    #[case::keyword(
        SuggestionKind::Command(CommandType::Keyword, None),
        CompletionItemKind::KEYWORD
    )]
    #[case::external(
        SuggestionKind::Command(CommandType::External, None),
        CompletionItemKind::INTERFACE
    )]
    #[case::alias(
        SuggestionKind::Command(CommandType::Alias, None),
        CompletionItemKind::REFERENCE
    )]
    #[case::flag(SuggestionKind::Flag, CompletionItemKind::FIELD)]
    #[case::variable(SuggestionKind::Variable, CompletionItemKind::VARIABLE)]
    #[case::module(SuggestionKind::Module, CompletionItemKind::MODULE)]
    #[case::operator(SuggestionKind::Operator, CompletionItemKind::OPERATOR)]
    #[case::file(SuggestionKind::File, CompletionItemKind::FILE)]
    #[case::directory(SuggestionKind::Directory, CompletionItemKind::FOLDER)]
    fn suggestion_kind_maps_to_item_kind(
        #[case] kind: SuggestionKind,
        #[case] expected: CompletionItemKind,
    ) {
        assert_eq!(LanguageServer::lsp_completion_item_kind(&kind), expected);
    }

    #[test]
    fn label_details_name_the_kind_and_the_type() {
        let describe = LanguageServer::kind_description;
        assert_eq!(describe(&SuggestionKind::Value(Type::Int)), "int");
        assert_eq!(
            describe(&SuggestionKind::Command(CommandType::Keyword, None)),
            "keyword"
        );
        assert_eq!(describe(&SuggestionKind::File), "file");
        assert_eq!(describe(&SuggestionKind::Directory), "directory");
        assert_eq!(describe(&SuggestionKind::CellPath), "cell path");
    }

    /// The label is the display of a file whose value had to be quoted, and the value stays the
    /// text that is inserted and matched.
    #[test]
    fn label_is_the_display_and_the_value_is_inserted() {
        Playground::setup("lsp_completion_display", |dirs, sandbox| {
            sandbox.with_files(&[Stub::FileWithContent("my file.txt", "line one\nline two\n")]);
            let (client_connection, _recv) = initialize_language_server(None, None);
            let response =
                complete_text_in(&client_connection, dirs.test().as_ref(), "open my", (0, 7));
            let item = &response.as_array().unwrap()[0];
            assert_eq!(item["label"], "my file.txt", "{item}");
            let inserted = item["textEdit"]["newText"].as_str().unwrap();
            assert_ne!(inserted, "my file.txt", "the value is quoted: {item}");
            assert_eq!(
                item["filterText"].as_str().unwrap(),
                inserted.trim_end(),
                "{item}"
            );
            assert_eq!(
                item["kind"],
                serde_json::to_value(CompletionItemKind::FILE).unwrap(),
                "{item}"
            );
        });
    }

    #[test]
    fn resolving_a_file_or_directory_previews_it() {
        Playground::setup("lsp_completion_preview", |dirs, sandbox| {
            sandbox.mkdir("a dir").with_files(&[
                Stub::FileWithContent("a dir/inner.txt", "inside"),
                Stub::FileWithContent("a file.txt", "line one\nline two\n"),
            ]);
            let (client_connection, _recv) = initialize_language_server(None, None);
            let dir = dirs.test().as_ref();
            let response = complete_text_in(&client_connection, dir, "open a", (0, 6));
            let items = response.as_array().unwrap();
            let find = |label: &str| items.iter().find(|item| item["label"] == label).unwrap();
            let (file, folder) = (find("a file.txt"), find("a dir/"));
            // Nothing is read until the client asks.
            assert!(file["documentation"].is_null() && folder["documentation"].is_null());
            assert!(
                file["data"]["path"]
                    .as_str()
                    .unwrap()
                    .ends_with("a file.txt")
            );

            let documentation = resolve(&client_connection, file);
            assert_eq!(documentation["documentation"]["kind"], "markdown");
            let value = documentation["documentation"]["value"].as_str().unwrap();
            assert!(value.contains("line one\nline two"), "{value}");
            assert!(!value.contains('\u{1b}'), "no styling: {value:?}");

            let documentation = resolve(&client_connection, folder);
            let value = documentation["documentation"]["value"].as_str().unwrap();
            assert!(value.contains("inner.txt"), "{value}");
            assert!(!value.contains('\u{1b}'), "no styling: {value:?}");
        });
    }

    #[test]
    fn resolving_a_command_documents_it_lazily() {
        let (client_connection, _recv) = initialize_language_server(None, None);
        let dir = FIXTURES.join("lsp/completion");
        let response = complete_text_in(&client_connection, &dir, "str trim", (0, 8));
        let item = response
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["label"] == "str trim")
            .unwrap();
        // The response only carries what is needed to resolve.
        assert!(item["documentation"].is_null(), "{item}");
        assert!(item["data"]["decl"].is_u64(), "{item}");
        let detail = item["detail"].as_str().unwrap();

        let resolved = resolve(&client_connection, item);
        assert_eq!(resolved["documentation"]["kind"], "markdown");
        let value = resolved["documentation"]["value"].as_str().unwrap();
        assert!(value.contains("### Usage"), "{value}");
        assert!(value.contains("### Flags"), "{value}");
        // The description is already the detail.
        assert!(!value.contains(detail), "{value}");
        // Everything but the documentation is as it was.
        let mut without_docs = resolved.clone();
        without_docs["documentation"] = serde_json::Value::Null;
        let mut original = item.clone();
        original["documentation"] = serde_json::Value::Null;
        assert_eq!(without_docs, original);
    }

    #[test]
    fn resolving_an_item_without_data_returns_it_unchanged() {
        let (client_connection, _recv) = initialize_language_server(None, None);
        let item = serde_json::json!({ "label": "mine", "detail": "unchanged" });
        assert_eq!(resolve(&client_connection, &item), item);
        // Data that points at nothing known is not an error either.
        let item = serde_json::json!({ "label": "mine", "data": { "decl": 99_999_999 } });
        assert_eq!(resolve(&client_connection, &item), item);
    }

    #[test]
    fn long_descriptions_become_documentation() {
        let long = "x".repeat(200);
        let config = format!(
            "$env.config.completions.external.completer = {{|input| [{{value: 'flag', description: '{long}'}}, {{value: 'other', description: 'short'}}]}}"
        );
        let response = complete_text(Some(&config), "sudo --b", (0, 8));
        assert_json_include!(
            actual: response,
            expected: serde_json::json!([
                { "label": "flag", "documentation": { "kind": "plaintext", "value": long } },
                { "label": "other", "detail": "short" },
            ])
        );
        assert!(response[0]["detail"].is_null(), "{response}");
    }

    fn external_suggestion(kind: Option<SuggestionKind>) -> SemanticSuggestion {
        SemanticSuggestion {
            kind,
            ..Default::default()
        }
    }

    #[test]
    fn externals_are_capped_when_the_list_holds_max_results_of_them() {
        let external =
            || external_suggestion(Some(SuggestionKind::Command(CommandType::External, None)));
        let builtin =
            || external_suggestion(Some(SuggestionKind::Command(CommandType::Builtin, None)));
        let results = vec![external(), builtin(), external(), builtin()];
        assert!(externals_capped(2, &results));
        assert!(externals_capped(1, &results));
        assert!(!externals_capped(3, &results), "room was left");
        // Only external command names count: a closure's values are never capped.
        let values = vec![external_suggestion(Some(SuggestionKind::Value(
            Type::String,
        )))];
        assert!(!externals_capped(1, &values));
        assert!(
            !externals_capped(0, &results),
            "no external is wanted at all"
        );
        assert!(
            !externals_capped(-1, &results),
            "negative is unlimited in nu-cli"
        );
    }

    /// With `max_results` executables on `PATH`, nushell stops looking, so the list is
    /// marked incomplete; with room to spare it is complete.
    #[cfg(unix)]
    #[test]
    fn capped_external_commands_make_the_list_incomplete() {
        use std::os::unix::fs::PermissionsExt;
        Playground::setup("lsp_completion_capped", |dirs, sandbox| {
            sandbox.with_files(&[
                Stub::EmptyFile("zz_one"),
                Stub::EmptyFile("zz_two"),
                Stub::EmptyFile("zz_three"),
            ]);
            for name in ["zz_one", "zz_two", "zz_three"] {
                let path = dirs.test().join(name);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let dir: &std::path::Path = dirs.test().as_ref();
            for (max_results, incomplete) in [(2, true), (3, true), (4, false)] {
                let config = format!(
                    "$env.PATH = ['{}']; $env.config.completions.external.max_results = {max_results}",
                    dir.display()
                );
                let response = complete_text(Some(&config), "zz_", (0, 3));
                if incomplete {
                    assert_eq!(response["isIncomplete"], true, "{response}");
                    assert_eq!(response["items"].as_array().unwrap().len(), max_results);
                } else {
                    let items = response.as_array().unwrap();
                    assert_eq!(items.len(), 3, "{response}");
                }
            }
        });
    }

    #[test]
    fn resolve_data_is_a_decl_id_or_a_path() {
        assert_eq!(
            serde_json::to_value(ResolveData::Decl(7)).unwrap(),
            serde_json::json!({ "decl": 7 })
        );
        assert_eq!(
            serde_json::to_value(ResolveData::Path("/tmp".into())).unwrap(),
            serde_json::json!({ "path": "/tmp" })
        );
    }
}
