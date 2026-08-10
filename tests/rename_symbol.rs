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
