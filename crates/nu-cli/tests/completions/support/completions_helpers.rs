use nu_engine::eval_block;
use nu_parser::parse;
use nu_path::AbsolutePathBuf;
use nu_protocol::{
    DynamicCompletionCallRef, DynamicSuggestion, PipelineData, ShellError, Signature, Span,
    SyntaxShape, Value,
    debugger::WithoutDebug,
    engine::{ArgType, Command, EngineState, Stack, StateWorkingSet},
};
use nu_test_support::prelude::*;

fn create_default_context() -> EngineState {
    let state = nu_cmd_lang::create_default_context();
    let state = nu_command::add_shell_command_context(state);
    nu_cli::add_cli_context(state)
}

/// `engine_state` completing by prefix, as the fixture-based tests of this suite were written
/// for. The default is `fuzzy`; the tests that want it use `new_engine_helper`, whose engines
/// keep the default, or ask for it in their config.
pub fn match_by_prefix(mut engine_state: EngineState) -> EngineState {
    let mut config = engine_state.get_config().as_ref().clone();
    config.completions.algorithm = nu_protocol::CompletionAlgorithm::Prefix;
    engine_state.set_config(config);
    engine_state
}

// A fake cmd for testing.
#[derive(Clone)]
struct FakeCmd;

impl Command for FakeCmd {
    fn name(&self) -> &str {
        "fake-cmd"
    }
    fn description(&self) -> &str {
        "a fake cmd completion for testing"
    }
    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .optional("second", SyntaxShape::String, "optional second")
            .required("first", SyntaxShape::String, "Required integer value.")
            .named(
                "flag",
                SyntaxShape::Int,
                "Example flag which support auto completion.",
                Some('f'),
            )
            .named(
                "plugin-config",
                SyntaxShape::Int,
                "Example flag which support auto completion from plugin config.",
                None,
            )
    }

    #[expect(deprecated, reason = "example usage")]
    fn get_dynamic_completion(
        &self,
        engine_state: &EngineState,
        _stack: &mut Stack,
        _call: DynamicCompletionCallRef,
        arg_type: &ArgType,
        _experimental: nu_protocol::engine::ExperimentalMarker,
    ) -> Result<Option<Vec<DynamicSuggestion>>, ShellError> {
        Ok(match arg_type {
            ArgType::Positional(index) => {
                // be careful: Don't include any spaces for values.
                if *index == 0 {
                    Some(
                        (0..1)
                            .map(|s| DynamicSuggestion {
                                value: format!("arg0:{s}"),
                                ..Default::default()
                            })
                            .collect(),
                    )
                } else if *index == 1 {
                    Some(
                        (0..2)
                            .map(|s| DynamicSuggestion {
                                value: format!("arg1:{s}"),
                                ..Default::default()
                            })
                            .collect(),
                    )
                } else {
                    None
                }
            }
            ArgType::Flag(flag_name) => match flag_name.as_ref() {
                "flag" => Some(
                    (0..3)
                        .map(|s| DynamicSuggestion {
                            value: format!("flag:{s}"),
                            ..Default::default()
                        })
                        .collect(),
                ),
                "plugin-config" => engine_state
                    .get_plugin_config("fake-cmd")
                    .and_then(|config| {
                        config.get_data_by_key("completion").map(|completion| {
                            vec![DynamicSuggestion {
                                value: completion.into_string().unwrap_or_else(|_| {
                                    "fake-cmd plugin config incorrect".to_string()
                                }),
                                ..Default::default()
                            }]
                        })
                    }),
                _ => None,
            },
        })
    }
    fn run(
        &self,
        _engine_state: &EngineState,
        _stack: &mut Stack,
        _call: &nu_protocol::engine::Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        Ok(PipelineData::Empty)
    }
}

pub fn new_engine_helper(pwd: AbsolutePathBuf) -> (AbsolutePathBuf, String, EngineState, Stack) {
    let pwd_str = pwd
        .clone()
        .into_os_string()
        .into_string()
        .unwrap_or_default();

    // Create a new engine with default context
    let mut engine_state = create_default_context();

    // Add $nu
    engine_state.generate_nu_constant();

    // New stack
    let mut stack = Stack::new();

    // Add pwd as env var
    stack.add_env_var(
        "PWD".to_string(),
        Value::string(pwd_str.clone(), nu_protocol::Span::new(0, pwd_str.len())),
    );
    stack.add_env_var(
        "TEST".to_string(),
        Value::string(
            "NUSHELL".to_string(),
            nu_protocol::Span::new(0, pwd_str.len()),
        ),
    );
    #[cfg(windows)]
    stack.add_env_var(
        "Path".to_string(),
        Value::string(
            "c:\\some\\path;c:\\some\\other\\path".to_string(),
            nu_protocol::Span::new(0, pwd_str.len()),
        ),
    );
    #[cfg(not(windows))]
    stack.add_env_var(
        "PATH".to_string(),
        Value::string(
            "/some/path:/some/other/path".to_string(),
            nu_protocol::Span::new(0, pwd_str.len()),
        ),
    );

    // Merge environment into the permanent state
    let merge_result = engine_state.merge_env(&mut stack);
    assert!(merge_result.is_ok());

    let mut working_set = StateWorkingSet::new(&engine_state);
    working_set.add_decl(Box::new(FakeCmd));
    let delta = working_set.render();
    let merge_result = engine_state.merge_delta(delta);
    assert!(merge_result.is_ok());
    (pwd, pwd_str, engine_state, stack)
}

/// creates a new engine with the current path in the completions fixtures folder
pub fn new_engine() -> (AbsolutePathBuf, String, EngineState, Stack) {
    let (dir, dir_str, engine_state, stack) = new_engine_helper(
        FIXTURES
            .join("completions")
            .try_into()
            .expect("fixtures is absolute"),
    );
    (dir, dir_str, match_by_prefix(engine_state), stack)
}

/// Adds pseudo PATH env for external completion tests
pub fn new_external_engine() -> EngineState {
    let mut engine = create_default_context();
    let dir = FIXTURES.join("external_completions").join("path");
    let dir_str = dir.to_string_lossy().to_string();
    let span = nu_protocol::Span::new(0, dir_str.len());
    engine.add_env_var(
        "PATH".to_string(),
        Value::list(vec![Value::string(dir_str, span)], span),
    );
    match_by_prefix(engine)
}

/// An engine whose `PATH` holds exactly the given programs, with a working directory of
/// its own holding `notes.txt` and a `docs` directory.
///
/// The temporary directory lives as long as the returned guard.
pub fn new_program_engine(programs: &[&str]) -> (tempfile::TempDir, EngineState, Stack) {
    let root = tempfile::tempdir().expect("temporary fixture directory");
    let bin = root.path().join("bin");
    let work = root.path().join("work");
    std::fs::create_dir_all(&bin).expect("bin directory");
    std::fs::create_dir_all(work.join("docs")).expect("work directory");
    std::fs::write(work.join("notes.txt"), "").expect("a file to complete");

    for program in programs {
        let path = bin.join(format!("{program}{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&path, "").expect("a program");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("an executable program");
        }
    }

    let (_, _, mut engine_state, mut stack) = new_engine_helper(
        work.canonicalize()
            .expect("work directory exists")
            .try_into()
            .expect("work directory is absolute"),
    );
    let bin = bin.to_string_lossy().into_owned();
    let span = Span::unknown();
    let path = Value::list(vec![Value::string(bin, span)], span);
    engine_state.add_env_var("PATH".to_string(), path.clone());
    // Programs the completers run are found through the stack's.
    stack.add_env_var("PATH".to_string(), path);
    (root, engine_state, stack)
}

/// merge_input executes the given input into the engine
/// and merges the state
pub fn merge_input(
    input: &[u8],
    engine_state: &mut EngineState,
    stack: &mut Stack,
) -> Result<(), ShellError> {
    let (block, delta) = {
        let mut working_set = StateWorkingSet::new(engine_state);

        let block = parse(&mut working_set, None, input, false);

        assert!(working_set.parse_errors.is_empty());

        (block, working_set.render())
    };

    engine_state.merge_delta(delta)?;

    assert!(
        eval_block::<WithoutDebug>(
            engine_state,
            stack,
            &block,
            PipelineData::value(Value::nothing(Span::test_data()), None),
        )
        .is_ok()
    );

    // Update engine_state with deleted variables (mirrors util.rs behavior)
    engine_state.merge_deletions(stack);

    // Merge environment into the permanent state
    engine_state.merge_env(stack)
}
