use nu_engine::command_prelude::*;

#[derive(Clone)]
pub struct AttrNonDestructive;

impl Command for AttrNonDestructive {
    fn name(&self) -> &str {
        "attr non-destructive"
    }

    fn signature(&self) -> Signature {
        Signature::build("attr non-destructive")
            .input_output_type(Type::Nothing, Type::Bool)
            .category(Category::Core)
    }

    fn description(&self) -> &str {
        "Attribute marking a command as safe to run while the line is still being typed."
    }

    fn extra_description(&self) -> &str {
        "A non-destructive command has no lasting effect when it runs: it writes no file, \
changes no environment and sends nothing anywhere. Completion runs only such commands on its \
own, to offer the values a pipeline would give, such as the names after `where name ==`. \
Without the attribute a custom command is run only when its body is made of commands that \
are known to only read; with it, the body is trusted, so the command may call anything \
(an external `git`, a closure) as long as the effect is nothing the user would mind \
happening on every keystroke.

The attribute is a promise from the author, so keep it for commands that deserve it. It \
does not make a command run when it would not: an `@interactive` completer still owns the \
terminal and is never run in the background, and a command that changes the environment \
(`def --env`) is refused whatever it carries."
    }

    fn run(
        &self,
        _engine_state: &EngineState,
        _stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        Ok(Value::bool(true, call.head).into_pipeline_data())
    }

    fn run_const(
        &self,
        _working_set: &StateWorkingSet,
        _stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        Ok(Value::bool(true, call.head).into_pipeline_data())
    }

    fn is_const(&self) -> bool {
        true
    }

    fn examples(&self) -> Vec<Example<'_>> {
        vec![Example {
            description: "Let completion run a command that only reads, to offer its values.",
            example: "\
                @non-destructive\n\
                def branches [] { ^git branch --format '%(refname:short)' | lines | wrap name }\n\n\
                branches | where name == \
            ",
            result: None,
        }]
    }
}
