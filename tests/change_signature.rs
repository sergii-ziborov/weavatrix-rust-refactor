//! `change_signature` on a real tree.
//!
//! Removing a parameter is the dangerous half: the declaration and every call have to change
//! together, and a call left holding an extra argument is a bug the compiler may not catch in
//! JavaScript. So the round trip here reads both files back, not just the declaration.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::{RepositoryState, Weavatrix};
use weavatrix_rust_refactor::operations::RefactorSession;

const FILES: [(&str, &str); 3] = [
    (
        "src/core.js",
        "export function compute(alpha, beta) {\n  return alpha + beta;\n}\n",
    ),
    (
        "src/caller.js",
        "import { compute } from './core.js';\n\nexport function run() {\n  return compute(1, 2);\n}\n",
    ),
    (
        "src/spread.js",
        "import { compute } from './core.js';\n\nexport function relay(args) {\n  return compute(...args);\n}\n",
    ),
];

fn repository(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-signature-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (path, contents) in FILES {
        let full = root.join(path);
        fs::create_dir_all(full.parent().expect("parent")).expect("directories");
        fs::write(full, contents).expect("source");
    }
    fs::write(
        root.join("package.json"),
        "{\"name\":\"fixture\",\"version\":\"0.0.0\",\"type\":\"module\"}\n",
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

fn declared(state: &RepositoryState, name: &str) -> Option<String> {
    state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.trim_end_matches("()") == name
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/core.js")
        })
        .map(|node| node.id.as_str().to_owned())
}

fn call(root: &std::path::Path, arguments: &Value) -> Value {
    let engine = Weavatrix::open(root).expect("repository opens");
    RefactorSession::read_only()
        .call(engine.state(), "change_signature", arguments)
        .expect("declared tool")
}

/// The fixture's declaration has to be in the graph, or every test below skips silently.
#[test]
fn the_fixture_symbol_is_actually_in_the_graph() {
    let root = repository("fixture");
    let engine = Weavatrix::open(&root).expect("opens");
    assert!(
        declared(engine.state(), "compute").is_some(),
        "compute is missing from the graph, so the tests here would pass without exercising it"
    );
}

#[test]
fn a_missing_operation_is_invalid_args() {
    let root = repository("args");
    assert_eq!(
        status(&call(&root, &json!({"symbol": "compute"}))),
        "INVALID_ARGS"
    );
}

#[test]
fn an_unknown_operation_kind_is_invalid_args() {
    let root = repository("kind");
    let answer = call(
        &root,
        &json!({"symbol": "compute", "operation": {"kind": "reorder"}}),
    );
    assert_eq!(status(&answer), "INVALID_ARGS");
}

#[test]
fn an_index_past_the_last_parameter_is_no_change() {
    let root = repository("range");
    let engine = Weavatrix::open(&root).expect("opens");
    let Some(id) = declared(engine.state(), "compute") else {
        return;
    };
    let answer = call(
        &root,
        &json!({"symbol": id, "operation": {"kind": "remove_parameter", "index": 7}}),
    );
    assert_eq!(status(&answer), "NO_CHANGE", "{answer:?}");
}

#[test]
fn adding_a_parameter_with_a_default_touches_only_the_declaration() {
    let root = repository("add-default");
    let engine = Weavatrix::open(&root).expect("opens");
    let state = engine.state().clone();
    let Some(id) = declared(&state, "compute") else {
        return;
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "change_signature",
            &json!({
                "symbol": id,
                "operation": {"kind": "add_parameter", "name": "gamma", "default": "0"},
            }),
        )
        .expect("declared tool");
    assert_eq!(status(&planned), "PLANNED", "{planned:?}");
    assert_eq!(
        planned
            .get("uncertain")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0),
        "a defaulted parameter leaves every existing call correct: {planned:?}"
    );
    let after = apply(&session, &state, &root, &planned, "src/core.js");
    assert!(
        after.starts_with("export function compute(alpha, beta, gamma = 0) {"),
        "got {after:?}"
    );
}

#[test]
fn adding_a_parameter_without_a_default_reports_every_call_site() {
    let root = repository("add-bare");
    let engine = Weavatrix::open(&root).expect("opens");
    let Some(id) = declared(engine.state(), "compute") else {
        return;
    };
    let answer = call(
        &root,
        &json!({"symbol": id, "operation": {"kind": "add_parameter", "name": "gamma"}}),
    );
    assert_eq!(status(&answer), "PLANNED", "{answer:?}");
    let kinds = answer
        .get("uncertain")
        .and_then(Value::as_array)
        .expect("uncertain")
        .iter()
        .filter_map(|entry| entry.get("kind").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&"VALUE_REQUIRED"),
        "the value to pass is a decision, and has to be handed back: {kinds:?}"
    );
}

#[test]
fn removing_a_parameter_edits_the_declaration_and_the_call() {
    let root = repository("remove");
    let engine = Weavatrix::open(&root).expect("opens");
    let state = engine.state().clone();
    let Some(id) = declared(&state, "compute") else {
        return;
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "change_signature",
            &json!({"symbol": id, "operation": {"kind": "remove_parameter", "index": 1}}),
        )
        .expect("declared tool");
    assert_eq!(status(&planned), "PLANNED", "{planned:?}");
    assert_eq!(
        planned.get("completeness").and_then(Value::as_str),
        Some("PARTIAL"),
        "call sites come from graph edges, so this can never be COMPLETE"
    );

    let core = apply(&session, &state, &root, &planned, "src/core.js");
    assert!(
        core.starts_with("export function compute(alpha) {"),
        "got {core:?}"
    );
    let caller = fs::read_to_string(root.join("src/caller.js")).expect("caller");
    assert!(
        caller.contains("compute(1)"),
        "the call has to lose its argument with the parameter, got {caller:?}"
    );
}

#[test]
fn a_call_that_spreads_is_reported_rather_than_edited() {
    let root = repository("spread");
    let engine = Weavatrix::open(&root).expect("opens");
    let Some(id) = declared(engine.state(), "compute") else {
        return;
    };
    let answer = call(
        &root,
        &json!({"symbol": id, "operation": {"kind": "remove_parameter", "index": 1}}),
    );
    if status(&answer) != "PLANNED" {
        return;
    }
    let spread = answer
        .get("uncertain")
        .and_then(Value::as_array)
        .expect("uncertain")
        .iter()
        .any(|entry| entry.get("kind").and_then(Value::as_str) == Some("SPREAD_ARGUMENT"));
    let edited_spread = answer
        .get("callSites")
        .and_then(Value::as_array)
        .is_some_and(|sites| {
            sites
                .iter()
                .any(|site| site.get("file").and_then(Value::as_str) == Some("src/spread.js"))
        });
    assert!(
        spread && !edited_spread,
        "which argument sits at an index is unknowable behind a spread: {answer:?}"
    );
}

/// Previews and applies a plan, returning the named file as it ends up on disk.
fn apply(
    session: &RefactorSession,
    state: &RepositoryState,
    root: &std::path::Path,
    planned: &Value,
    file: &str,
) -> String {
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("declared tool");
    assert_eq!(
        status(&preview),
        "PREVIEW_OK",
        "the edited bytes did not match the files: {preview:?}"
    );
    let token = preview
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();
    let applied = session
        .call(
            state,
            "apply_edit_plan",
            &json!({"plan": plan, "mode": "apply", "confirm_token": token}),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");
    fs::read_to_string(root.join(file)).expect("file")
}
