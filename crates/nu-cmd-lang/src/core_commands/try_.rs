use nu_engine::command_prelude::*;
use nu_protocol::engine::CommandType;

#[derive(Clone)]
pub struct Try;

impl Command for Try {
    fn name(&self) -> &str {
        "try"
    }

    fn description(&self) -> &str {
        "Try to run a block, if it fails optionally run a catch closure."
    }

    fn signature(&self) -> nu_protocol::Signature {
        Signature::build("try")
            .input_output_types(vec![(Type::Any, Type::Any)])
            .required("try_block", SyntaxShape::Block, "Block to run.")
            .optional(
                "catch",
                SyntaxShape::OneOf(vec![
                    SyntaxShape::Keyword(
                        b"catch".to_vec(),
                        Box::new(SyntaxShape::OneOf(vec![
                            caught(),
                            SyntaxShape::Closure(None),
                        ])),
                    ),
                    SyntaxShape::Keyword(
                        b"finally".to_vec(),
                        Box::new(SyntaxShape::OneOf(vec![
                            SyntaxShape::Closure(None),
                            SyntaxShape::Closure(Some(vec![SyntaxShape::Any])),
                        ])),
                    ),
                ]),
                "Closure to run if try block fails.",
            )
            .optional(
                "finally",
                SyntaxShape::OneOf(vec![
                    SyntaxShape::Keyword(
                        b"catch".to_vec(),
                        Box::new(SyntaxShape::OneOf(vec![
                            caught(),
                            SyntaxShape::Closure(None),
                        ])),
                    ),
                    SyntaxShape::Keyword(
                        b"finally".to_vec(),
                        Box::new(SyntaxShape::OneOf(vec![
                            SyntaxShape::Closure(None),
                            SyntaxShape::Closure(Some(vec![SyntaxShape::Any])),
                        ])),
                    ),
                ]),
                "Closure to run anyway.",
            )
            .category(Category::Core)
    }

    fn extra_description(&self) -> &str {
        "This command is a parser keyword. For details, check:
  https://www.nushell.sh/book/thinking_in_nu.html"
    }

    fn command_type(&self) -> CommandType {
        CommandType::Keyword
    }

    fn run(
        &self,
        _engine_state: &EngineState,
        _stack: &mut Stack,
        _call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        // This is compiled specially by the IR compiler. The code here is never used when
        // running in IR mode.
        eprintln!(
            "Tried to execute 'run' for the 'try' command: this code path should never be reached in IR mode"
        );
        unreachable!();
    }

    fn examples(&self) -> Vec<Example<'_>> {
        vec![
            Example {
                description: "Try to run a division by zero.",
                example: "try { 1 / 0 }",
                result: None,
            },
            Example {
                description: "Try to run a division by zero and return a string instead.",
                example: "try { 1 / 0 } catch { 'divided by zero' }",
                result: Some(Value::test_string("divided by zero")),
            },
            Example {
                description: "Try to run a division by zero and report the message.",
                example: "try { 1 / 0 } catch { |err| $err.msg }",
                result: None,
            },
            Example {
                description: "Try to run a division by zero, report the message, and run finally",
                example: "try { 1 / 0 } catch { |err| print $err.msg } finally { 'clean' }",
                result: None,
            },
        ]
    }
}

/// What a `catch` closure is given: the error as a record, so that `$err.` completes and is typed.
/// An external's failure also has an `exit_code`, which not every error has.
fn caught() -> SyntaxShape {
    let fields = [
        ("msg", SyntaxShape::String),
        ("debug", SyntaxShape::String),
        ("raw", SyntaxShape::Error),
        ("rendered", SyntaxShape::String),
        ("details", SyntaxShape::Record(Vec::new().into())),
    ];
    let fields = fields
        .map(|(name, shape)| (name.to_owned(), shape))
        .to_vec();
    SyntaxShape::Closure(Some(vec![SyntaxShape::Record(fields.into())]))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_examples() -> nu_test_support::Result {
        nu_test_support::test().examples(Try)
    }
}
