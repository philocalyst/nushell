use nu_engine::{command_prelude::*, scope::ScopeData};

#[derive(Clone)]
pub struct ScopeCommands;

impl Command for ScopeCommands {
    fn name(&self) -> &str {
        "scope commands"
    }

    fn signature(&self) -> Signature {
        Signature::build("scope commands")
            .input_output_types(vec![(
                Type::Nothing,
                Type::Table(
                    [
                        ("name", Type::String),
                        ("category", Type::String),
                        ("signatures", Type::record()),
                        ("description", Type::String),
                        ("examples", Type::list(Type::Any)),
                        ("attributes", Type::list(Type::Any)),
                        ("type", Type::String),
                        ("is_sub", Type::Bool),
                        ("is_const", Type::Bool),
                        ("creates_scope", Type::Bool),
                        ("extra_description", Type::String),
                        ("search_terms", Type::String),
                        ("complete", Type::Any),
                        ("deprecation_info", Type::list(Type::Any)),
                        ("decl_id", Type::Int),
                    ]
                    .into(),
                ),
            )])
            .allow_variants_without_examples(true)
            .category(Category::Core)
    }

    fn description(&self) -> &str {
        "Output info on the commands in the current scope."
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let head = call.head;
        let mut scope_data = ScopeData::new(engine_state, stack);
        scope_data.populate_decls();
        Ok(Value::list(scope_data.collect_commands(head), head).into_pipeline_data())
    }

    fn examples(&self) -> Vec<Example<'_>> {
        vec![Example {
            description: "Show the commands in the current scope.",
            example: "scope commands",
            result: None,
        }]
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_examples() -> nu_test_support::Result {
        nu_test_support::test().examples(ScopeCommands)
    }
}
