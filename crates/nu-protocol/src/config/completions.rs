use super::{config_update_string_enum, prelude::*};
use crate as nu_protocol;
use crate::engine::Closure;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, IntoValue, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionAlgorithm {
    Prefix,
    /// Deprecated: `"substring"` in a config is read as [`CompletionAlgorithm::Fuzzy`], which
    /// finds every substring match and ranks it higher. The variant stays so that old values keep
    /// deserializing.
    Substring,
    #[default]
    Fuzzy,
}

impl FromStr for CompletionAlgorithm {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "prefix" => Ok(Self::Prefix),
            // Deprecated: a substring is a fuzzy match, and ranks first.
            "substring" => Ok(Self::Fuzzy),
            "fuzzy" => Ok(Self::Fuzzy),
            _ => Err("'prefix' or 'fuzzy'"),
        }
    }
}

impl UpdateFromValue for CompletionAlgorithm {
    fn update(&mut self, value: &Value, path: &mut ConfigPath, errors: &mut ConfigErrors) {
        config_update_string_enum(self, value, path, errors)
    }
}

#[derive(Clone, Copy, Debug, Default, IntoValue, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionSort {
    #[default]
    Smart,
    Alphabetical,
}

impl FromStr for CompletionSort {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "smart" => Ok(Self::Smart),
            "alphabetical" => Ok(Self::Alphabetical),
            _ => Err("'smart' or 'alphabetical'"),
        }
    }
}

impl UpdateFromValue for CompletionSort {
    fn update(&mut self, value: &Value, path: &mut ConfigPath, errors: &mut ConfigErrors) {
        config_update_string_enum(self, value, path, errors)
    }
}

/// How the examples of a command's help show in a preview.
#[derive(Clone, Copy, Debug, Default, IntoValue, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExamplesMode {
    /// Behind the preview, unfolded with the key that cycles previews.
    #[default]
    Folded,
    /// Under the preview, unfolded from the start.
    Shown,
    /// Never: a preview holds no examples, and nothing in it can be taken.
    Off,
}

impl FromStr for ExamplesMode {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "folded" => Ok(Self::Folded),
            "shown" => Ok(Self::Shown),
            "off" => Ok(Self::Off),
            _ => Err("'folded', 'shown' or 'off'"),
        }
    }
}

impl UpdateFromValue for ExamplesMode {
    fn update(&mut self, value: &Value, path: &mut ConfigPath, errors: &mut ConfigErrors) {
        config_update_string_enum(self, value, path, errors)
    }
}

#[derive(Clone, Debug, IntoValue, Serialize, Deserialize)]
pub struct ExternalCompleterConfig {
    pub enable: bool,
    pub max_results: i64,
    /// A closure completing external-command arguments. To drive a picker like `fzf`, have it
    /// call a command carrying the `@interactive` attribute; interactivity is seen through the
    /// closure to that command, so there is no separate switch to set.
    pub completer: Option<Closure>,
}

impl Default for ExternalCompleterConfig {
    fn default() -> Self {
        Self {
            enable: true,
            max_results: 100,
            completer: None,
        }
    }
}

impl UpdateFromValue for ExternalCompleterConfig {
    fn update<'a>(
        &mut self,
        value: &'a Value,
        path: &mut ConfigPath<'a>,
        errors: &mut ConfigErrors,
    ) {
        let Value::Record { val: record, .. } = value else {
            errors.type_mismatch(path, Type::record(), value);
            return;
        };

        for (col, val) in record.iter() {
            let path = &mut path.push(col);
            match col.as_str() {
                "completer" => match val {
                    Value::Nothing { .. } => self.completer = None,
                    Value::Closure { val, .. } => self.completer = Some(val.as_ref().clone()),
                    _ => errors.type_mismatch(path, Type::custom("closure or nothing"), val),
                },
                "max_results" => self.max_results.update(val, path, errors),
                "enable" => self.enable.update(val, path, errors),
                _ => errors.unknown_option(path, val),
            }
        }
    }
}

#[derive(Clone, Debug, IntoValue, Serialize, Deserialize)]
pub struct CompletionConfig {
    pub sort: CompletionSort,
    pub case_sensitive: bool,
    pub quick: bool,
    pub partial: bool,
    /// Deprecated: completions now stay open while the line is edited.
    pub persistent_menus: bool,
    /// Show completions as the line is typed, before any Tab.
    pub passive: bool,
    pub algorithm: CompletionAlgorithm,
    pub external: ExternalCompleterConfig,
    pub use_ls_colors: bool,
    /// Completion results the cache keeps before evicting the least recently used entry; `0` disables it.
    pub cache_size: i64,
    /// What previews each kind of suggestion in place of the shell's own preview, by the name of
    /// the kind (`file`, `directory`, `command`, ...): a closure given the suggestion.
    pub previewers: HashMap<String, Closure>,
    /// Put in what the line can only go on with once a space is typed, with the cursor where the
    /// writing goes on: the closure of `each `, the `= ` of `let x `.
    pub opening: bool,
    /// How the examples of a command's help show in its preview.
    pub examples: ExamplesMode,
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            sort: CompletionSort::default(),
            case_sensitive: false,
            quick: true,
            partial: true,
            persistent_menus: false,
            passive: true,
            algorithm: CompletionAlgorithm::default(),
            external: ExternalCompleterConfig::default(),
            use_ls_colors: true,
            cache_size: 100,
            previewers: HashMap::new(),
            opening: true,
            examples: ExamplesMode::default(),
        }
    }
}

impl UpdateFromValue for CompletionConfig {
    fn update<'a>(
        &mut self,
        value: &'a Value,
        path: &mut ConfigPath<'a>,
        errors: &mut ConfigErrors,
    ) {
        let Value::Record { val: record, .. } = value else {
            errors.type_mismatch(path, Type::record(), value);
            return;
        };

        for (col, val) in record.iter() {
            let path = &mut path.push(col);
            match col.as_str() {
                "sort" => self.sort.update(val, path, errors),
                "quick" => self.quick.update(val, path, errors),
                "partial" => self.partial.update(val, path, errors),
                "opening" => self.opening.update(val, path, errors),
                "examples" => self.examples.update(val, path, errors),
                "persistent_menus" => self.persistent_menus.update(val, path, errors),
                "passive" => self.passive.update(val, path, errors),
                "algorithm" => self.algorithm.update(val, path, errors),
                "case_sensitive" => self.case_sensitive.update(val, path, errors),
                "external" => self.external.update(val, path, errors),
                "use_ls_colors" => self.use_ls_colors.update(val, path, errors),
                "cache_size" => self.cache_size.update(val, path, errors),
                "previewers" => {
                    let Value::Record { val: kinds, .. } = val else {
                        errors.type_mismatch(path, Type::record(), val);
                        continue;
                    };
                    self.previewers.clear();
                    for (kind, previewer) in kinds.iter() {
                        let path = &mut path.push(kind);
                        match previewer {
                            Value::Closure { val, .. } => {
                                self.previewers.insert(kind.clone(), val.as_ref().clone());
                            }
                            Value::Nothing { .. } => {}
                            _ => errors.type_mismatch(path, Type::custom("closure"), previewer),
                        }
                    }
                }
                _ => errors.unknown_option(path, val),
            }
        }
    }
}
