pub(crate) mod cases;
mod delegation;
mod harness;
mod properties;
mod regressions;
pub mod support;

use std::{fs::read_dir, path::PathBuf, sync::Arc};

use nu_cli::NuCompleter;
use nu_path::expand_tilde;
use nu_protocol::engine::Stack;
use support::new_engine;

/// A cancelled picker answers empty; the next Tab on the same line must run it again
/// rather than reuse that answer. Neither answer is kept by the completer: reedline holds
/// the one it was given for as long as the line stays the same.
#[test]
fn interactive_completer_reruns_after_an_empty_answer() {
    let (_, _, mut engine, mut stack) = new_engine();
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("asked");
    let command = format!(
        "@interactive\n\
         def comp [] {{ if ('{m}' | path exists) {{ [alpha] }} else {{ touch '{m}'; [] }} }}\n\
         def my-command [arg: string@comp] {{}}",
        m = marker.display()
    );
    assert!(support::merge_input(command.as_bytes(), &mut engine, &mut stack).is_ok());

    let mut completer = NuCompleter::new(Arc::new(engine), Arc::new(stack));
    let line = "my-command ";
    let first = completer.complete_blocking(line, line.len());
    assert!(first.is_empty(), "got {first:?}");

    let second = completer.complete_blocking(line, line.len());
    assert_eq!(
        second.iter().map(|s| s.value.as_str()).collect::<Vec<_>>(),
        ["alpha"]
    );

    // Without the marker a rerun answers empty again: the `alpha` answer was not kept.
    std::fs::remove_file(&marker).expect("remove marker");
    let third = completer.complete_blocking(line, line.len());
    assert!(third.is_empty(), "got {third:?}");
}

/// fzf Esc (exit 130, empty stdout) through an `@interactive` completer must answer with
/// empty suggestions — the same cancel path as `input list` Esc / returning null.
#[cfg(unix)]
#[test]
fn fzf_esc_exit_130_interactive_completer_offers_nothing() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir for fzf stub");
    let stub = dir.path().join("fzf");
    fs::write(
        &stub,
        "#!/bin/sh\n# simulate fzf Esc: no selection, exit 130\nexit 130\n",
    )
    .expect("write fzf stub");
    let mut perms = fs::metadata(&stub).expect("stub meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&stub, perms).expect("chmod fzf stub");

    let (_, _, mut engine, mut stack) = new_engine();
    // new_engine's PATH is a colon string; prepend the stub dir so `^fzf` hits exit 130.
    let path_prefix = dir.path().display().to_string();
    let command = format!(
        r#"
        $env.PATH = $"{path_prefix}:($env.PATH)"
        @interactive
        def comp [token] {{ ^fzf }}
        def my-command [arg: string@comp] {{}}
        "#
    );
    assert!(support::merge_input(command.as_bytes(), &mut engine, &mut stack).is_ok());
    let mut completer = NuCompleter::new(Arc::new(engine), Arc::new(stack));

    let line = "my-command ";
    let suggestions = completer.complete_blocking(line, line.len());
    assert!(
        suggestions.is_empty(),
        "fzf Esc (exit 130) must yield empty suggestions, got {suggestions:?}"
    );
}

/// `use` at the root or the home directory offers what `read_dir` lists there: the directories
/// (and the links to them, which the completer follows) and the `.nu` files.
#[test]
fn dotnu_completions_of_the_root_and_home() {
    let (_, _, engine, stack) = new_engine();
    let mut completer = NuCompleter::new(Arc::new(engine), Arc::new(stack));
    let places = [
        ("use /", PathBuf::from("/")),
        ("use ~", expand_tilde("~")),
        #[cfg(windows)]
        ("use \\", PathBuf::from("\\")),
    ];
    for (line, dir) in places {
        let mut listed: Vec<String> = (read_dir(dir).expect("the directory is readable"))
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().into_string().ok()?;
                (entry.path().is_dir() || name.ends_with(".nu")).then_some(name)
            })
            .collect();
        // The names, without the quotes, the `~` and the separators a suggestion adds.
        let mut offered: Vec<String> = (completer.complete_blocking(line, line.len()).iter())
            .map(|s| s.value.trim_matches('`').trim_start_matches('~'))
            .map(|name| name.trim_matches('/').trim_matches('\\').to_owned())
            .collect();
        listed.sort();
        offered.sort();
        assert_eq!(listed, offered, "for {line:?}");
    }
}

/// A variable `unlet` in a parent or a grandparent stack is not offered in the stacks below it.
#[test]
fn unlet_variable_ancestor_stack_not_in_completions() {
    let (_, _, mut engine, mut stack) = new_engine();
    assert!(support::merge_input(b"let myvar = 123", &mut engine, &mut stack).is_ok());
    assert!(support::merge_input(b"unlet $myvar", &mut engine, &mut stack).is_ok());
    let parent = Arc::new(stack);
    let child = Stack::with_parent(Arc::clone(&parent));
    let grandchild = Stack::with_parent(Arc::new(Stack::with_parent(parent)));
    for stack in [child, grandchild] {
        let mut completer = NuCompleter::new(Arc::new(engine.clone()), Arc::new(stack));
        let suggestions = completer.complete_blocking("$my", 3);
        assert!(
            !suggestions.iter().any(|s| s.value == "$myvar"),
            "got {suggestions:?}"
        );
    }
}
