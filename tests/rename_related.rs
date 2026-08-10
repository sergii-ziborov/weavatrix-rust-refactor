//! `rename_related_symbols`: several renames, one transaction.
//!
//! The swap is the case worth building a batch for. Run `alpha -> beta` and then `beta -> alpha`
//! as two sequential renames and the second one undoes the first, leaving everything called
//! `alpha`. Applied as one positional batch they exchange, which is what was asked for.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::Weavatrix;
use weavatrix_rust_refactor::operations::RefactorSession;

const FILES: [(&str, &str); 3] = [
    ("src/lib.rs", "pub mod core;\npub mod caller;\n"),
    (
        "src/core.rs",
        "pub fn alpha(value: u32) -> u32 {\n    value + 1\n}\n\npub fn beta(value: u32) -> u32 {\n    value + 2\n}\n",
    ),
    (
        "src/caller.rs",
        "use crate::core::alpha;\nuse crate::core::beta;\n\npub fn run() -> u32 {\n    alpha(1) + beta(2)\n}\n",
    ),
];

fn repository(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-related-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (path, contents) in FILES {
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

/// The graph id of a function declared in `src/core.rs`, or `None` if the graph did not record it.
fn declared(state: &weavatrix_rust::RepositoryState, name: &str) -> Option<String> {
    state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.trim_end_matches("()") == name
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/core.rs")
        })
        .map(|node| node.id.as_str().to_owned())
}

/// Every test below resolves its symbols through the graph and steps aside when it cannot find
/// them, so a graph that stopped recording Rust functions would turn this file green while
/// testing nothing. This is the one test that has to fail in that case.
#[test]
fn the_fixture_symbols_are_actually_in_the_graph() {
    let root = repository("fixture");
    let engine = Weavatrix::open(&root).expect("opens");
    assert!(
        declared(engine.state(), "alpha").is_some() && declared(engine.state(), "beta").is_some(),
        "the fixture's declarations are missing from the graph, so every other test here is \
         silently passing without exercising anything"
    );
}

#[test]
fn a_missing_renames_array_is_invalid_args() {
    let root = repository("args");
    let engine = Weavatrix::open(&root).expect("opens");
    let answer = RefactorSession::read_only()
        .call(engine.state(), "rename_related_symbols", &json!({}))
        .expect("declared tool");
    assert_eq!(status(&answer), "INVALID_ARGS");
}

#[test]
fn an_entry_without_a_new_name_is_named_in_the_refusal() {
    let root = repository("entry");
    let engine = Weavatrix::open(&root).expect("opens");
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "rename_related_symbols",
            &json!({"renames": [{"symbol": "alpha"}]}),
        )
        .expect("declared tool");
    assert_eq!(status(&answer), "INVALID_ARGS");
    assert!(
        answer
            .get("reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.contains("renames[0]")),
        "the refusal has to say which entry is wrong: {answer:?}"
    );
}

#[test]
fn one_failing_sub_rename_blocks_the_whole_batch() {
    let root = repository("blocked");
    let engine = Weavatrix::open(&root).expect("opens");
    let Some(alpha) = declared(engine.state(), "alpha") else {
        return;
    };
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "rename_related_symbols",
            &json!({"renames": [
                {"symbol": alpha, "new_name": "first"},
                {"symbol": "src/core.rs#no_such_symbol", "new_name": "second"},
            ]}),
        )
        .expect("declared tool");
    assert_eq!(status(&answer), "BLOCKED", "{answer:?}");
    assert_eq!(
        answer
            .get("cause")
            .and_then(|cause| cause.get("status"))
            .and_then(Value::as_str),
        Some("NOT_FOUND"),
        "the sub-rename's own refusal has to survive: {answer:?}"
    );
}

#[test]
fn two_symbols_given_one_name_is_a_conflict_not_a_plan() {
    let root = repository("collision");
    let engine = Weavatrix::open(&root).expect("opens");
    let (Some(alpha), Some(beta)) = (
        declared(engine.state(), "alpha"),
        declared(engine.state(), "beta"),
    ) else {
        return;
    };
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "rename_related_symbols",
            &json!({"renames": [
                {"symbol": alpha, "new_name": "same"},
                {"symbol": beta, "new_name": "same"},
            ]}),
        )
        .expect("declared tool");
    assert_eq!(status(&answer), "CONFLICT", "{answer:?}");
    let kinds = answer
        .get("conflicts")
        .and_then(Value::as_array)
        .expect("conflicts")
        .iter()
        .filter_map(|conflict| conflict.get("kind").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"NAME_COLLISION"), "{kinds:?}");
}

#[test]
fn a_swap_is_reported_as_ordering_rather_than_refused() {
    let root = repository("swap-preview");
    let engine = Weavatrix::open(&root).expect("opens");
    let (Some(alpha), Some(beta)) = (
        declared(engine.state(), "alpha"),
        declared(engine.state(), "beta"),
    ) else {
        return;
    };
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "rename_related_symbols",
            &json!({"renames": [
                {"symbol": alpha, "new_name": "beta"},
                {"symbol": beta, "new_name": "alpha"},
            ]}),
        )
        .expect("declared tool");
    assert_eq!(status(&answer), "PREVIEW_OK", "{answer:?}");
    let kinds = answer
        .get("ordering")
        .and_then(Value::as_array)
        .expect("ordering")
        .iter()
        .filter_map(|entry| entry.get("kind").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(
        kinds.iter().all(|kind| *kind == "SWAP") && !kinds.is_empty(),
        "an exchange has to be named as one: {kinds:?}"
    );
    assert!(answer.get("confirmToken").and_then(Value::as_str).is_some());
}

#[test]
fn applying_without_the_token_writes_nothing() {
    let root = repository("untokened");
    let engine = Weavatrix::open(&root).expect("opens");
    let Some(alpha) = declared(engine.state(), "alpha") else {
        return;
    };
    let session = RefactorSession::new(true);
    let answer = session
        .call(
            engine.state(),
            "rename_related_symbols",
            &json!({"renames": [{"symbol": alpha, "new_name": "first"}], "mode": "apply"}),
        )
        .expect("declared tool");
    assert_ne!(status(&answer), "APPLIED", "{answer:?}");
    let core = fs::read_to_string(root.join("src/core.rs")).expect("core");
    assert!(core.contains("pub fn alpha"), "got {core:?}");
}

#[test]
fn a_closed_write_gate_refuses_before_the_token_is_checked() {
    let root = repository("gate");
    let engine = Weavatrix::open(&root).expect("opens");
    let Some(alpha) = declared(engine.state(), "alpha") else {
        return;
    };
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "rename_related_symbols",
            &json!({
                "renames": [{"symbol": alpha, "new_name": "first"}],
                "mode": "apply",
                "confirm_token": "anything",
            }),
        )
        .expect("declared tool");
    assert_eq!(status(&answer), "WRITE_GATE_CLOSED", "{answer:?}");
}

#[test]
fn the_swap_exchanges_both_names_in_one_transaction() {
    let root = repository("swap-apply");
    let engine = Weavatrix::open(&root).expect("opens");
    let state = engine.state().clone();
    let (Some(alpha), Some(beta)) = (declared(&state, "alpha"), declared(&state, "beta")) else {
        return;
    };
    let session = RefactorSession::new(true);
    let renames = json!([
        {"symbol": alpha, "new_name": "beta"},
        {"symbol": beta, "new_name": "alpha"},
    ]);

    let preview = session
        .call(
            &state,
            "rename_related_symbols",
            &json!({"renames": renames.clone()}),
        )
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
            "rename_related_symbols",
            &json!({"renames": renames, "mode": "apply", "confirm_token": token}),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");

    // Sequential renames would have collapsed both onto one name; the exchange is the proof that
    // the batch was applied positionally.
    let core = fs::read_to_string(root.join("src/core.rs")).expect("core");
    let beta_first = core.find("pub fn beta").expect("beta declared");
    let alpha_second = core.find("pub fn alpha").expect("alpha declared");
    assert!(
        beta_first < alpha_second,
        "the declarations should have exchanged names, got {core:?}"
    );
    assert!(
        core.contains("pub fn beta(value: u32) -> u32 {\n    value + 1")
            && core.contains("pub fn alpha(value: u32) -> u32 {\n    value + 2"),
        "each body must have kept its own declaration, got {core:?}"
    );
}

#[test]
fn a_consumed_token_cannot_apply_the_batch_twice() {
    let root = repository("replay");
    let engine = Weavatrix::open(&root).expect("opens");
    let state = engine.state().clone();
    let Some(alpha) = declared(&state, "alpha") else {
        return;
    };
    let session = RefactorSession::new(true);
    let renames = json!([{"symbol": alpha, "new_name": "first"}]);
    let preview = session
        .call(
            &state,
            "rename_related_symbols",
            &json!({"renames": renames.clone()}),
        )
        .expect("declared tool");
    let Some(token) = preview.get("confirmToken").and_then(Value::as_str) else {
        return;
    };
    let token = token.to_owned();
    let first = session
        .call(
            &state,
            "rename_related_symbols",
            &json!({"renames": renames.clone(), "mode": "apply", "confirm_token": token.clone()}),
        )
        .expect("declared tool");
    assert_eq!(status(&first), "APPLIED", "{first:?}");
    let second = session
        .call(
            &state,
            "rename_related_symbols",
            &json!({"renames": renames, "mode": "apply", "confirm_token": token}),
        )
        .expect("declared tool");
    assert_ne!(
        status(&second),
        "APPLIED",
        "a single-use token must not apply twice: {second:?}"
    );
}
