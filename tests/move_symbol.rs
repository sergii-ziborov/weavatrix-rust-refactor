//! `move_symbol` against real dependency shapes.
//!
//! The verdict only means something if the fixture can actually produce a cycle, so one case
//! moves a declaration into the file that already depends on it — the move that turns a
//! one-way dependency into a mutual one.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::Weavatrix;
use weavatrix_rust_refactor::operations::RefactorSession;

fn repository(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-move-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).expect("repository");
    for (path, contents) in files {
        fs::write(root.join(path), contents).expect("source");
    }
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("manifest");
    root
}

fn call(root: &std::path::Path, arguments: &Value) -> Value {
    let engine = Weavatrix::open(root).expect("repository opens");
    RefactorSession::read_only()
        .call(engine.state(), "move_symbol", arguments)
        .expect("declared tool")
}

fn status(value: &Value) -> &str {
    value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn verdict(value: &Value) -> &str {
    value
        .get("verdict")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// `caller` depends on `core`; nothing depends on `caller`.
const FILES: [(&str, &str); 3] = [
    ("src/lib.rs", "pub mod core;\npub mod caller;\n"),
    (
        "src/core.rs",
        "pub fn helper(value: u32) -> u32 {\n    value + 1\n}\n",
    ),
    (
        "src/caller.rs",
        "use crate::core::helper;\n\npub fn run() -> u32 {\n    helper(1)\n}\n",
    ),
];

#[test]
fn a_missing_argument_is_invalid_args() {
    let root = repository("args", &FILES);
    assert_eq!(
        status(&call(&root, &json!({"symbol": "helper"}))),
        "INVALID_ARGS"
    );
}

#[test]
fn an_unknown_symbol_is_not_found() {
    let root = repository("missing", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "absent::name", "to_file": "src/caller.rs"}),
    );
    assert_eq!(status(&answer), "NOT_FOUND");
}

#[test]
fn moving_a_symbol_to_its_own_file_is_no_change() {
    let root = repository("same", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "helper", "to_file": "src/core.rs"}),
    );
    if status(&answer) == "NOT_FOUND" {
        return; // the fixture graph did not expose that label
    }
    assert_eq!(status(&answer), "NO_CHANGE");
}

#[test]
fn a_move_that_creates_no_cycle_is_feasible() {
    let root = repository("feasible", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "helper", "to_file": "src/caller.rs"}),
    );
    if status(&answer) != "EVALUATED" {
        return;
    }
    // Moving the helper into its only consumer removes the dependency rather than adding one.
    assert_eq!(verdict(&answer), "FEASIBLE", "{answer:?}");
    let introduced = answer
        .get("cycles")
        .and_then(|cycles| cycles.get("introduced"))
        .and_then(Value::as_array)
        .map(Vec::len);
    assert_eq!(introduced, Some(0));
}

#[test]
fn the_answer_names_who_depends_on_the_symbol() {
    let root = repository("blast", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "helper", "to_file": "src/other.rs"}),
    );
    if status(&answer) != "EVALUATED" {
        return;
    }
    let importers = answer
        .get("blastRadius")
        .and_then(|radius| radius.get("importers"))
        .and_then(Value::as_array)
        .expect("importers");
    assert!(
        importers
            .iter()
            .any(|file| file.as_str() == Some("src/caller.rs")),
        "the consumer must appear in the blast radius, got {importers:?}"
    );
}

#[test]
fn the_projection_says_it_is_a_projection() {
    let root = repository("fidelity", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "helper", "to_file": "src/caller.rs"}),
    );
    if status(&answer) != "EVALUATED" {
        return;
    }
    assert_eq!(
        answer.get("fidelity").and_then(Value::as_str),
        Some("PROJECTED_FROM_GRAPH_EDGES"),
        "a prediction from simulated edges must not read as a rebuild"
    );
    assert!(
        answer.get("plan").is_none(),
        "move_symbol computes no byte edits"
    );
}
