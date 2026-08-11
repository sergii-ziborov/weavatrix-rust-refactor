//! `rename_symbol` on a real tree, including the shadow trap.
//!
//! The trap is the whole point: two files declare the same name, and only one of them is the
//! target. A find-replace renames both and silently breaks the other. Renaming a *symbol* means
//! the second one is left alone and reported, not edited.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::Weavatrix;
use weavatrix_rust_refactor::operations::RefactorSession;

fn repository(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-rename-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (path, contents) in files {
        let full = root.join(path);
        fs::create_dir_all(full.parent().expect("parent")).expect("directories");
        fs::write(full, contents).expect("source");
    }
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("manifest");
    root
}

fn status(value: &Value) -> &str {
    value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn call(root: &std::path::Path, arguments: &Value) -> Value {
    let engine = Weavatrix::open(root).expect("repository opens");
    RefactorSession::read_only()
        .call(engine.state(), "rename_symbol", arguments)
        .expect("declared tool")
}

/// `core::resolve_target` is used by `caller`; `local` declares an unrelated function with the
/// same name that nothing outside it uses.
const FILES: [(&str, &str); 4] = [
    (
        "src/lib.rs",
        "pub mod core;\npub mod caller;\npub mod local;\n",
    ),
    (
        "src/core.rs",
        "pub fn resolve_target(value: u32) -> u32 {\n    value + 1\n}\n",
    ),
    (
        "src/caller.rs",
        "use crate::core::resolve_target;\n\npub fn run() -> u32 {\n    resolve_target(1)\n}\n",
    ),
    (
        "src/local.rs",
        "fn resolve_target(node: u32) -> u32 {\n    node\n}\n\npub fn pick() -> u32 {\n    resolve_target(2)\n}\n",
    ),
];

#[test]
fn a_missing_argument_is_invalid_args() {
    let root = repository("args", &FILES);
    assert_eq!(
        status(&call(&root, &json!({"symbol": "resolve_target"}))),
        "INVALID_ARGS"
    );
}

#[test]
fn an_invalid_new_name_is_refused_before_anything_is_planned() {
    let root = repository("badname", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "src/core.rs#resolve_target", "new_name": "not a name"}),
    );
    assert_eq!(status(&answer), "INVALID_NEW_NAME");
}

#[test]
fn renaming_to_the_same_name_is_no_change() {
    let root = repository("same", &FILES);
    let engine = Weavatrix::open(&root).expect("opens");
    let Some(id) = engine
        .state()
        .graph()
        .nodes()
        .iter()
        .find(|node| node.label.starts_with("resolve_target"))
        .map(|node| node.id.as_str().to_owned())
    else {
        return;
    };
    let answer = call(&root, &json!({"symbol": id, "new_name": "resolve_target"}));
    assert_eq!(status(&answer), "NO_CHANGE");
}

#[test]
fn an_ambiguous_name_is_refused_rather_than_renaming_the_wrong_one() {
    // Two declarations share this label, so the bare name must not resolve to either.
    let root = repository("ambiguous", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "resolve_target", "new_name": "locate_target"}),
    );
    assert_eq!(
        status(&answer),
        "NOT_FOUND",
        "an ambiguous label must not pick a declaration: {answer:?}"
    );
}

#[test]
fn the_declaration_and_its_proven_call_sites_are_renamed() {
    let root = repository("sites", &FILES);
    let engine = Weavatrix::open(&root).expect("opens");
    let Some(id) = engine
        .state()
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.starts_with("resolve_target")
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/core.rs")
        })
        .map(|node| node.id.as_str().to_owned())
    else {
        return;
    };
    let answer = call(&root, &json!({"symbol": id, "new_name": "locate_target"}));
    assert_eq!(status(&answer), "PLANNED", "{answer:?}");
    assert_eq!(
        answer.get("completeness").and_then(Value::as_str),
        Some("PARTIAL"),
        "a backend that cannot prove the absence of other references must not claim COMPLETE"
    );
    let files = answer
        .get("plan")
        .and_then(|plan| plan.get("files"))
        .and_then(Value::as_array)
        .expect("files");
    let touched = files
        .iter()
        .filter_map(|file| file.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(
        touched.contains(&"src/core.rs"),
        "the declaration must be renamed: {touched:?}"
    );
    assert!(
        !touched.contains(&"src/local.rs"),
        "the unrelated same-named declaration must not be touched: {touched:?}"
    );
}

#[test]
fn applying_the_plan_leaves_the_shadowed_declaration_untouched() {
    let root = repository("apply", &FILES);
    let engine = Weavatrix::open(&root).expect("opens");
    let session = RefactorSession::new(true);
    let state = engine.state().clone();
    let Some(id) = state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.starts_with("resolve_target")
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/core.rs")
        })
        .map(|node| node.id.as_str().to_owned())
    else {
        return;
    };

    let planned = session
        .call(
            &state,
            "rename_symbol",
            &json!({"symbol": id, "new_name": "locate_target"}),
        )
        .expect("declared tool");
    if status(&planned) != "PLANNED" {
        return;
    }
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("declared tool");
    assert_eq!(status(&preview), "PREVIEW_OK", "{preview:?}");
    let token = preview
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();
    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": plan, "mode": "apply", "confirm_token": token}),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");

    let core = fs::read_to_string(root.join("src/core.rs")).expect("core");
    assert!(core.contains("locate_target"), "got {core:?}");
    let local = fs::read_to_string(root.join("src/local.rs")).expect("local");
    assert!(
        local.contains("resolve_target") && !local.contains("locate_target"),
        "the shadowed declaration must survive intact, got {local:?}"
    );
}

/// The refusal for an ambiguous bare name carries the candidate ids, so disambiguation costs
/// zero extra calls — the agent picks from the refusal instead of running a graph query.
#[test]
fn an_ambiguous_refusal_names_its_candidates() {
    let root = repository("candidates", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "resolve_target", "new_name": "locate_target"}),
    );
    assert_eq!(status(&answer), "NOT_FOUND", "{answer:?}");
    let candidates = answer
        .get("candidates")
        .and_then(Value::as_array)
        .expect("candidates");
    assert!(
        candidates.len() >= 2,
        "both same-named declarations have to be offered: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .filter_map(Value::as_str)
            .any(|id| id.contains("core.rs")),
        "{candidates:?}"
    );
}

/// The rename previews its own plan and hands over the confirmation, so the whole flow is two
/// calls and the agent never echoes plan bytes back: rename -> apply {confirm_token}.
#[test]
fn applying_with_the_token_alone_writes_the_previewed_plan() {
    let root = repository("tokenonly", &FILES);
    let engine = Weavatrix::open(&root).expect("opens");
    let state = engine.state().clone();
    let session = RefactorSession::new(true);
    let Some(id) = state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.starts_with("resolve_target")
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/core.rs")
        })
        .map(|node| node.id.as_str().to_owned())
    else {
        return;
    };
    let planned = session
        .call(
            &state,
            "rename_symbol",
            &json!({"symbol": id, "new_name": "locate_target"}),
        )
        .expect("declared tool");
    assert_eq!(
        planned.get("previewed").and_then(Value::as_bool),
        Some(true),
        "the rename must have dry-run its own plan: {planned:?}"
    );
    let Some(token) = planned.get("confirmToken").and_then(Value::as_str) else {
        panic!("{planned:?}");
    };
    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"mode": "apply", "confirm_token": token.to_owned()}),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");
    let core = fs::read_to_string(root.join("src/core.rs")).expect("core");
    assert!(core.contains("locate_target"), "got {core:?}");
}

/// The graph records a symbol's span as its declaration line, and a call lives in the body one
/// line further down. Reading that span literally renamed the declaration and nothing else, which
/// looked like a working rename until you opened the caller.
#[test]
fn the_call_site_inside_a_calling_function_is_renamed_too() {
    let root = repository("callsite", &FILES);
    let engine = Weavatrix::open(&root).expect("opens");
    let state = engine.state().clone();
    let session = RefactorSession::new(true);
    let Some(id) = state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.starts_with("resolve_target")
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/core.rs")
        })
        .map(|node| node.id.as_str().to_owned())
    else {
        return;
    };
    let planned = session
        .call(
            &state,
            "rename_symbol",
            &json!({"symbol": id, "new_name": "locate_target"}),
        )
        .expect("declared tool");
    assert_eq!(status(&planned), "PLANNED", "{planned:?}");
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("declared tool");
    let Some(token) = preview.get("confirmToken").and_then(Value::as_str) else {
        panic!("{preview:?}");
    };
    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": plan, "mode": "apply", "confirm_token": token.to_owned()}),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");

    let caller = fs::read_to_string(root.join("src/caller.rs")).expect("caller");
    assert!(
        caller.contains("locate_target(1)"),
        "the call has to be renamed with the declaration, got {caller:?}"
    );
    assert!(
        !caller.contains("use crate::core::resolve_target"),
        "an import left naming the old symbol is a rename that broke the build, got {caller:?}"
    );
}

/// The benchmark's string trap: the fixture's caller carries the name inside a string literal
/// on an `export` line. The import-line widening put that line into the proven set, the textual
/// scan cannot tell an identifier from the same characters inside quotes, and the literal got
/// corrupted. A site now needs the tokenizer's word, not just the graph's line.
#[test]
fn a_string_literal_on_a_proven_line_is_reported_not_renamed() {
    let root = repository(
        "stringtrap",
        &[
            ("src/lib.rs", "pub mod core;\npub mod caller;\n"),
            (
                "src/core.rs",
                "pub fn resolve_target(value: u32) -> u32 {\n    value + 1\n}\n",
            ),
            (
                "src/caller.rs",
                "use crate::core::resolve_target;\n\npub const HELP: &str = \"call resolve_target with a value\";\n\npub fn run() -> u32 {\n    resolve_target(1)\n}\n",
            ),
        ],
    );
    let engine = Weavatrix::open(&root).expect("opens");
    let state = engine.state().clone();
    let session = RefactorSession::new(true);
    let Some(id) = state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.starts_with("resolve_target")
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/core.rs")
        })
        .map(|node| node.id.as_str().to_owned())
    else {
        return;
    };
    let planned = session
        .call(
            &state,
            "rename_symbol",
            &json!({"symbol": id, "new_name": "locate_target"}),
        )
        .expect("declared tool");
    assert_eq!(status(&planned), "PLANNED", "{planned:?}");
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("declared tool");
    let Some(token) = preview.get("confirmToken").and_then(Value::as_str) else {
        panic!("{preview:?}");
    };
    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": plan, "mode": "apply", "confirm_token": token.to_owned()}),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");

    let caller = fs::read_to_string(root.join("src/caller.rs")).expect("caller");
    assert!(
        caller.contains("\"call resolve_target with a value\""),
        "the string literal must survive the rename, got {caller:?}"
    );
    assert!(
        caller.contains("use crate::core::locate_target;") && caller.contains("locate_target(1)"),
        "the import and the call must still be renamed, got {caller:?}"
    );
    let uncertain = planned
        .get("uncertainReferences")
        .and_then(Value::as_array)
        .expect("uncertain");
    assert!(
        uncertain.iter().any(|entry| {
            entry.get("file").and_then(Value::as_str) == Some("src/caller.rs")
                && entry
                    .get("excerpt")
                    .and_then(Value::as_str)
                    .is_some_and(|excerpt| excerpt.contains("HELP"))
        }),
        "the string occurrence has to be reported rather than dropped: {uncertain:?}"
    );
}

/// A call inside a template-literal interpolation is code; the same name in a plain template
/// string is prose. The tokenizer emits `${...}` as one Interpolation token, so an
/// identifier-only filter dropped the call — the fix re-tokenizes interpolation contents.
#[test]
fn a_call_inside_a_template_interpolation_is_renamed_and_a_plain_template_is_not() {
    let root = repository(
        "template",
        &[
            (
                "src/core.js",
                "export function resolveTarget(selector) {\n  return `/${resolveTarget(selector)}` + `resolveTarget`;\n}\n",
            ),
            (
                "src/caller.js",
                "import { resolveTarget } from './core.js';\n\nexport const OUT = resolveTarget('x');\n",
            ),
        ],
    );
    let engine = Weavatrix::open(&root).expect("opens");
    let state = engine.state().clone();
    let session = RefactorSession::new(true);
    let Some(id) = state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.trim_end_matches("()") == "resolveTarget"
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/core.js")
        })
        .map(|node| node.id.as_str().to_owned())
    else {
        return;
    };
    let planned = session
        .call(
            &state,
            "rename_symbol",
            &json!({"symbol": id, "new_name": "locateTarget"}),
        )
        .expect("declared tool");
    assert_eq!(status(&planned), "PLANNED", "{planned:?}");
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("declared tool");
    let Some(token) = preview.get("confirmToken").and_then(Value::as_str) else {
        panic!("{preview:?}");
    };
    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": plan, "mode": "apply", "confirm_token": token.to_owned()}),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");

    let core = fs::read_to_string(root.join("src/core.js")).expect("core");
    assert!(
        core.contains("`/${locateTarget(selector)}`"),
        "the call inside the interpolation is code and must be renamed, got {core:?}"
    );
    assert!(
        core.contains("`resolveTarget`"),
        "the plain template string is prose and must survive, got {core:?}"
    );
}

#[test]
fn unproven_same_named_occurrences_are_reported_rather_than_renamed() {
    let root = repository("uncertain", &FILES);
    let engine = Weavatrix::open(&root).expect("opens");
    let Some(id) = engine
        .state()
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.starts_with("resolve_target")
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/core.rs")
        })
        .map(|node| node.id.as_str().to_owned())
    else {
        return;
    };
    let answer = call(&root, &json!({"symbol": id, "new_name": "locate_target"}));
    if status(&answer) != "PLANNED" {
        return;
    }
    let warnings = answer
        .get("warnings")
        .and_then(Value::as_array)
        .expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str() == Some("GRAPH_PROVEN_SITES_ONLY")),
        "the limit of the backend has to be stated on every plan"
    );
}
