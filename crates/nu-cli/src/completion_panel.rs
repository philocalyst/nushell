//! The completion panel: reedline's [`DefaultPanel`], told what nushell knows of each suggestion.
//! A value takes the colour the line will give it once inserted, the fields a custom completer
//! returned beyond the ones the engine reads are dim cells beside it (a column's type coloured as
//! the status line colours types), a correction reads as what was typed struck out and then what
//! was meant, and paths in one directory, or subcommands of one command, name it once in the top
//! edge. On the hint line a command says what it takes before what it is for.

use crate::completions::{Detail, Details, ENV_VAR_SOURCE, TAKES};
use crate::status_line::{StatusStyles, type_parts};
use nu_ansi_term::{Color, Style};
use nu_color_config::{color_record_to_nustyle, get_shape_color, lookup_ansi_color_style};
use nu_protocol::{Config, SuggestionKind, Type, Value, engine::CommandType};
use reedline::{Completions, DefaultPanel, PanelStyle, Suggestion};
use std::{collections::HashMap, path::is_separator, sync::Arc};

/// The completion panel, styled by the `style` record of the configured completion menu, if
/// there is one, with kinds coloured from `config`'s `color_config`, and framed in `dim`, the
/// style of the status line.
pub(crate) fn nu_panel(config: &Arc<Config>, dim: Style) -> DefaultPanel {
    let decoration = Decoration::new(config, dim);
    DefaultPanel::new(decoration.style.clone())
        .with_decoration(move |completions| decoration.decorate(completions))
}

/// What the panel is told of each answer from its [`Details`].
struct Decoration {
    /// Formats the values of the extra fields, and colours the values.
    config: Arc<Config>,
    style: PanelStyle,
    /// The names of types, as the status line colours them.
    types: Style,
}

impl Decoration {
    fn new(config: &Arc<Config>, dim: Style) -> Self {
        // The selection is a bar in the colour of the status line, which no prompt or highlight
        // of the line shares.
        let bar = dim.foreground.unwrap_or(Color::Fixed(67));
        let mut style = PanelStyle {
            text: Style::new(),
            selected: Style::new().fg(Color::White).on(bar).bold(),
            description: dim,
            matched: Style::new().underline(),
        };
        let configured = config.menus.iter().find(|menu| {
            matches!(
                menu.name.as_str(),
                Ok("completion_menu" | "ide_completion_menu")
            )
        });
        if let Some(Value::Record { val, .. }) = configured.map(|menu| &menu.style) {
            let read = |key: &str| val.get(key).map(style_of);
            style.text = read("text").unwrap_or(style.text);
            style.selected = read("selected_text").unwrap_or(style.selected);
            style.matched = read("match_text").unwrap_or(style.matched);
        }
        Self {
            config: Arc::clone(config),
            types: (config.color_config.get("status_line_type").map(style_of))
                .unwrap_or_else(StatusStyles::default_types),
            style,
        }
    }

    /// `completions` with what their details say, and the title of what they share.
    fn decorate(&self, completions: &Completions) -> Completions {
        let Some(details) = completions.data::<Details>() else {
            return completions.clone();
        };
        Completions {
            items: self.items(&completions.items, details),
            title: title(&completions.items, details),
            ..completions.clone()
        }
    }

    /// `items` with what their `details` say: each in the colour of its kind, with its type and
    /// fields as cells, a correction as one, and the description the hint line shows.
    fn items(&self, items: &[Suggestion], details: &Details) -> Arc<[Suggestion]> {
        let detail = |index: usize| details.0.get(index);
        let dim = self.style.description;
        // A type every item has tells none of them apart.
        let types: Vec<_> = (items.iter().enumerate())
            .map(|(index, item)| cell_path_type(item, detail(index)))
            .collect();
        let typed = !(types.len() > 1 && types.windows(2).all(|pair| pair[0] == pair[1]))
            && types.iter().any(Option::is_some);
        let columns = details.columns();
        let field = |detail: &Detail, name: &str| {
            let value = detail.fields.as_ref()?.get(name)?;
            Some(value.to_expanded_string(", ", &self.config))
        };
        // A type reads as one: its names coloured, what joins them quiet.
        let coloured = |text: &str| -> String {
            (type_parts(text, self.types, dim).iter())
                .map(|(style, part)| style.paint(part.as_str()).to_string())
                .collect()
        };
        let mut shapes = HashMap::new();
        (items.iter().enumerate())
            .map(|(index, item)| {
                let detail = detail(index);
                let shape = detail.and_then(shape);
                let style = item.style.or_else(|| {
                    let shape = shape?;
                    Some(
                        *(shapes.entry(shape))
                            .or_insert_with(|| get_shape_color(shape, &self.config)),
                    )
                });
                let ty = types[index].as_deref();
                let mut cells: Vec<String> = typed
                    .then(|| ty.map(coloured).unwrap_or_default())
                    .into_iter()
                    .collect();
                cells.extend(columns.iter().map(|&name| {
                    let text = detail.and_then(|detail| field(detail, name));
                    let text = text.unwrap_or_default();
                    if name == TAKES { coloured(&text) } else { text }
                }));
                // A correction reads as one: what was typed struck out, then what was meant.
                let display_override = match detail.and_then(|detail| detail.fixes.as_deref()) {
                    Some(typed) => Some(format!(
                        "{}{}{}",
                        dim.strikethrough().paint(typed),
                        dim.paint(" → "),
                        style.unwrap_or(self.style.text).paint(item.display_value()),
                    )),
                    None => item.display_override.clone(),
                };
                Suggestion {
                    style,
                    extra: cells.iter().any(|cell| !cell.is_empty()).then_some(cells),
                    display_override,
                    description: self.hint_of(item, detail, ty),
                    ..item.clone()
                }
            })
            .collect()
    }

    /// What the hint line says while `item` is selected: a command's usage, then the first line
    /// of its description, less the type of a column, which is beside it.
    fn hint_of(
        &self,
        item: &Suggestion,
        detail: Option<&Detail>,
        ty: Option<&str>,
    ) -> Option<String> {
        let line = (item.description.as_deref())
            .and_then(|text| text.lines().next())
            .map_or("", str::trim);
        let line = ty
            .and_then(|ty| line.strip_prefix(ty))
            .and_then(|rest| rest.strip_prefix(" · ").or(rest.is_empty().then_some("")))
            .unwrap_or(line);
        let Some(usage) = detail.and_then(|detail| detail.usage.as_deref()) else {
            return (!line.is_empty()).then(|| line.to_owned());
        };
        // Styled, the line is drawn as it is: the usage quiet, the description in italics.
        let dim = self.style.description;
        let line = (!line.is_empty()).then(|| dim.italic().paint(format!("  {line}")));
        Some(format!(
            "{}{}",
            dim.paint(usage),
            line.map(|line| line.to_string()).unwrap_or_default()
        ))
    }
}

/// A style written as a color name or a `{fg, bg, attr}` record.
fn style_of(value: &Value) -> Style {
    match value {
        Value::String { val, .. } => lookup_ansi_color_style(val),
        Value::Record { .. } => color_record_to_nustyle(value),
        _ => Style::new(),
    }
}

/// The shape the line highlights a suggestion of `detail`'s kind as once it is inserted:
/// commands, flags, variables, and values by their type. Paths come coloured by `LS_COLORS`
/// already, in the style of the suggestion.
fn shape(detail: &Detail) -> Option<&'static str> {
    Some(match detail.kind.as_ref()? {
        SuggestionKind::Command(CommandType::External, _) => "shape_external",
        SuggestionKind::Command(CommandType::Keyword, _) => "shape_keyword",
        SuggestionKind::Command(..) => "shape_internalcall",
        SuggestionKind::Flag => "shape_flag",
        SuggestionKind::Variable => "shape_variable",
        SuggestionKind::Operator => "shape_operator",
        SuggestionKind::Value(_) if detail.source.as_deref() == Some(ENV_VAR_SOURCE) => {
            "shape_variable"
        }
        SuggestionKind::Value(ty) => match ty {
            Type::Int | Type::Number => "shape_int",
            Type::Float => "shape_float",
            Type::Bool => "shape_bool",
            Type::String | Type::Any => "shape_string",
            Type::Range => "shape_range",
            Type::Date => "shape_datetime",
            Type::Nothing => "shape_nothing",
            Type::Binary => "shape_binary",
            Type::Glob => "shape_globpattern",
            // The line has no shape of their own for these, so their colour as values.
            Type::Filesize => "filesize",
            Type::Duration => "duration",
            _ => return None,
        },
        _ => return None,
    })
}

/// The type a column of a cell path holds, which the description of its suggestion starts with
/// (`filesize · 160 B`, the sample being the hint line's).
fn cell_path_type(item: &Suggestion, detail: Option<&Detail>) -> Option<String> {
    if !matches!(detail?.kind, Some(SuggestionKind::CellPath)) {
        return None;
    }
    let line = item.description.as_deref()?.lines().next()?;
    let ty = line.split(" · ").next()?.trim();
    (ty.chars().count() <= 16).then(|| ty.to_owned())
}

/// What every item is in, as the items show it and with what separates it from the rest: the
/// directory of paths, unquoted (`crates/nu-cli/src/`), or the command of two or more
/// subcommands when the rest are single words (`cargo `).
fn title(items: &[Suggestion], details: &Details) -> Option<String> {
    let all = |test: fn(&SuggestionKind) -> bool| {
        (0..items.len()).all(|index| {
            details
                .0
                .get(index)
                .and_then(|d| d.kind.as_ref())
                .is_some_and(test)
        })
    };
    let values = || items.iter().map(Suggestion::display_value);
    if all(|kind| matches!(kind, SuggestionKind::File | SuggestionKind::Directory)) {
        let quoted = values().any(|value| value.starts_with(['\'', '"', '`']));
        return (!quoted)
            .then(|| shared(values(), is_separator, false))
            .flatten();
    }
    if all(|kind| matches!(kind, SuggestionKind::Command(..))) {
        return shared(values(), |c| c == ' ', true);
    }
    None
}

/// The start `values` share up to a `separator`, with it: every value has to have one before
/// its end, or with `singles` the values without one are passed over and two or more need one.
fn shared<'a>(
    values: impl Iterator<Item = &'a str>,
    separator: fn(char) -> bool,
    singles: bool,
) -> Option<String> {
    let (mut shared, mut parents): (Option<&str>, usize) = (None, 0);
    for value in values {
        let Some(at) = value.trim_end_matches(separator).rfind(separator) else {
            if singles {
                continue;
            }
            return None;
        };
        let parent = &value[..=at];
        let so_far = shared.map_or(parent, |so_far| {
            let same = (so_far.char_indices().zip(parent.chars()))
                .take_while(|((_, a), b)| a == b)
                .last()
                .map_or(0, |((at, c), _)| at + c.len_utf8());
            &so_far[..so_far[..same].rfind(separator).map_or(0, |at| at + 1)]
        });
        if so_far.is_empty() {
            return None;
        }
        (shared, parents) = (Some(so_far), parents + 1);
    }
    shared
        .filter(|_| !singles || parents >= 2)
        .map(str::to_owned)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use nu_protocol::{Record, Span as NuSpan};
    use reedline::{Panel, Span, View};

    /// Completions of `values`, each with its `detail`, as the completer answers them.
    fn answer(values: &[(&str, &str)], details: Vec<Detail>) -> Completions {
        let items: Vec<Suggestion> = (values.iter())
            .map(|(value, description)| Suggestion {
                value: (*value).to_owned(),
                description: Some((*description).to_owned()),
                span: Span::new(0, 0),
                ..Suggestion::default()
            })
            .collect();
        Completions::new(items).with_data(Details(details))
    }

    fn of(kind: SuggestionKind) -> Detail {
        Detail {
            kind: Some(kind),
            ..Detail::default()
        }
    }

    /// A view of `completions` on a screen 60 columns wide with room for 8 rows, and no colours.
    pub(crate) fn view(completions: &Completions, selected: Option<usize>) -> View<'_> {
        View {
            completions,
            selected,
            anchor: 0,
            width: 60,
            rows: 8,
            list_rows: 8,
            still_rows: u16::MAX,
            preview: None,
            unfolded: false,
            list_offset: 0,
            list_scrolloff: 0,
            preview_offset: 0,
            pick: None,
            use_ansi_coloring: false,
        }
    }

    fn panel() -> DefaultPanel {
        nu_panel(&Arc::new(Config::default()), Style::new())
    }

    /// What the panel is told of `completions`.
    fn decorated(completions: &Completions) -> Completions {
        Decoration::new(&Arc::new(Config::default()), Style::new()).decorate(completions)
    }

    #[test]
    fn paths_in_one_directory_and_subcommands_of_one_command_name_it_once() {
        let paths = |values: &[&str]| {
            let kinds = (values.iter()).map(|value| match value.ends_with('/') {
                true => of(SuggestionKind::Directory),
                false => of(SuggestionKind::File),
            });
            let pairs: Vec<_> = values.iter().map(|value| (*value, "")).collect();
            answer(&pairs, kinds.collect())
        };
        let shared = paths(&[
            "crates/nu-cli/src/completions/",
            "crates/nu-cli/src/repl.rs",
        ]);
        let rows = panel().render(&view(&shared, None));
        assert_eq!(rows[0], "╭─ crates/nu-cli/src/ ─╮", "{rows:#?}");
        assert_eq!(rows[1], "│ completions/         │");
        let apart = paths(&["src/lib.rs", "tests/main.rs"]);
        assert_eq!(panel().render(&view(&apart, None))[1], "│ src/lib.rs    │");
        let quoted = paths(&["'a b/c'", "'a b/d'"]);
        assert_eq!(decorated(&quoted).title, None);

        let command = SuggestionKind::Command(CommandType::External, None);
        let pairs = [("config", ""), ("cargo check", ""), ("cargo clean", "")];
        let subcommands = answer(&pairs, (0..3).map(|_| of(command.clone())).collect());
        assert_eq!(decorated(&subcommands).title.as_deref(), Some("cargo "));
    }

    #[test]
    fn fields_and_the_types_of_columns_are_cells_and_kinds_are_colours() {
        let fields = |pairs: &[(&str, &str)]| Detail {
            fields: Some(
                (pairs.iter())
                    .map(|(name, text)| {
                        (
                            (*name).to_owned(),
                            Value::string(*text, NuSpan::test_data()),
                        )
                    })
                    .collect::<Record>(),
            ),
            ..Detail::default()
        };
        // The columns are the union of the fields, in first-seen order.
        let details = vec![
            fields(&[("updated", "2hr"), ("remote", "origin")]),
            fields(&[("pushed", "yes"), ("remote", "fork")]),
            Detail::default(),
        ];
        let items = answer(&[("main", ""), ("topic", ""), ("old", "")], details);
        let rows = panel().render(&view(&items, None));
        assert_eq!(rows[1], "│ main   2hr  origin      │");
        assert_eq!(rows[2], "│ topic       fork    yes │");
        assert_eq!(rows[3], "│ old                     │");

        // A column's type follows its name, as in a signature; a sample after it is the hint's.
        let columns = answer(
            &[("name", "string"), ("size", "filesize · 160 B")],
            vec![of(SuggestionKind::CellPath), of(SuggestionKind::CellPath)],
        );
        let mut panel = panel();
        let rows = panel.render(&view(&columns, None));
        assert_eq!(rows[1], "│ name  string   │");
        assert_eq!(rows[2], "│ size  filesize │");
        assert_eq!(panel.hint(&view(&columns, Some(0))), None, "only the type");
        assert_eq!(
            panel.hint(&view(&columns, Some(1))).as_deref(),
            Some("  160 B")
        );

        // A command takes the colour the config gives it, a path keeps its own.
        let colors = [("shape_internalcall", "red"), ("shape_int", "blue")];
        let config = Config {
            color_config: (colors.iter())
                .map(|(key, color)| ((*key).to_owned(), Value::test_string(*color)))
                .collect(),
            ..Config::default()
        };
        let kinds = vec![
            of(SuggestionKind::Command(CommandType::Builtin, None)),
            of(SuggestionKind::Directory),
            of(SuggestionKind::Value(Type::Int)),
        ];
        let mut items = answer(&[("ls", ""), ("src", ""), ("309", "")], kinds);
        let directory = Style::new().fg(Color::Yellow).bold();
        let mut suggestions = items.items.to_vec();
        suggestions[1].style = Some(directory);
        items.items = suggestions.into();
        let shown = Decoration::new(&Arc::new(config), Style::new()).decorate(&items);
        let styles: Vec<_> = shown.items.iter().map(|item| item.style).collect();
        assert_eq!(
            styles,
            [
                Some(Color::Red.normal()),
                Some(directory),
                Some(Color::Blue.normal())
            ]
        );
    }

    #[test]
    fn a_correction_reads_as_what_was_typed_and_what_was_meant() {
        let fix = Detail {
            fixes: Some("crgo".to_owned()),
            ..of(SuggestionKind::Command(CommandType::External, None))
        };
        let completions = answer(&[("cargo", "")], vec![fix]);
        let rows = panel().render(&view(&completions, None));
        assert_eq!(rows[1], "│ crgo → cargo │", "{rows:#?}");
    }

    #[test]
    fn the_hint_says_what_a_command_takes_before_what_it_is_for() {
        let detail = Detail {
            usage: Some("first {flags} (rows)".into()),
            ..Detail::default()
        };
        let command = answer(&[("first", "Return the first rows.")], vec![detail]);
        // Styled, and plain without colours.
        let hint = panel().hint(&view(&command, Some(0)));
        assert_eq!(
            hint.as_deref(),
            Some("  first {flags} (rows)  Return the first rows.")
        );
    }
}
