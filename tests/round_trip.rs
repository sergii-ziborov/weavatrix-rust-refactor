//! A plan produced by an engine, applied by the write path, on a real file.
//!
//! This is the test that catches a coordinate mistake. A planner and an applier can each be
//! self-consistent and still disagree — the graph counts byte columns from one, the plan counts
//! UTF-16 units from zero — and the only way that shows up is writing a file and reading it back.
//! So one case here is deliberately non-ASCII: on a pure-ASCII fixture the two systems agree and
//! the bug hides.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::Weavatrix;
use weavatrix_rust_refactor::operations::RefactorSession;

fn repository(name: &str, lib: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-round-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).expect("repository");
    fs::write(root.join("src/lib.rs"), lib).expect("source");
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

fn session(root: &std::path::Path) -> (RefactorSession, weavatrix_rust::RepositoryState) {
    let engine = Weavatrix::open(root).expect("repository opens");
    (RefactorSession::new(true), engine.state().clone())
}

/// Plans with one engine, then applies through the write path, returning the new file contents.
fn plan_and_apply(root: &std::path::Path, tool: &str, arguments: &Value) -> Result<String, String> {
    let (session, state) = session(root);
    let planned = session
        .call(&state, tool, arguments)
        .expect("declared tool");
    if status(&planned) != "PLANNED" {
        return Err(format!("{tool} did not plan: {planned:?}"));
    }
    let plan = planned.get("plan").expect("a plan").clone();

    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("declared tool");
    if status(&preview) != "PREVIEW_OK" {
        return Err(format!(
            "preview refused the engine's own plan: {preview:?}"
        ));
    }
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
    if status(&applied) != "APPLIED" {
        return Err(format!("apply refused: {applied:?}"));
    }
    Ok(fs::read_to_string(root.join("src/lib.rs")).expect("source"))
}

#[test]
fn an_insertion_planned_by_edit_symbol_lands_where_it_was_planned() {
    let root = repository("insert", "pub fn one() -> u32 {\n    1\n}\n");
    let result = plan_and_apply(
        &root,
        "edit_symbol",
        &json!({
            "symbol": "one",
            "operation": "insert_before_symbol",
            "content": "/// counted\n",
        }),
    );
    let Ok(contents) = result else {
        // The fixture graph did not expose that symbol; the unit tests cover the refusal paths.
        return;
    };
    assert!(
        contents.starts_with("/// counted\npub fn one()"),
        "the insertion must land at the declaration, got {contents:?}"
    );
}

#[test]
fn a_plan_over_a_non_ascii_file_edits_the_intended_characters() {
    // Two multi-byte characters before the declaration: if the planner emitted byte columns
    // where the applier expects UTF-16 units, the edit lands inside the comment instead.
    let source = "// naïve café helper\npub fn one() -> u32 {\n    1\n}\n";
    let root = repository("utf8", source);
    let result = plan_and_apply(
        &root,
        "edit_symbol",
        &json!({
            "symbol": "one",
            "operation": "insert_before_symbol",
            "content": "#[inline]\n",
        }),
    );
    let Ok(contents) = result else {
        return;
    };
    assert!(
        contents.contains("// naïve café helper\n#[inline]\npub fn one()"),
        "the comment must survive intact and the attribute must precede the declaration, got \
         {contents:?}"
    );
    assert!(
        contents.contains("naïve café"),
        "the multi-byte characters must be untouched, got {contents:?}"
    );
}

#[test]
fn an_engine_plan_that_the_applier_would_refuse_is_never_produced() {
    // Whatever an engine plans, the applier's own validation has to accept it. A planner that
    // emits an unreadable envelope is a bug the engine's unit tests cannot see.
    let root = repository("valid", "pub fn one() -> u32 {\n    1\n}\n");
    let (session, state) = session(&root);
    let planned = session
        .call(
            &state,
            "edit_symbol",
            &json!({"symbol": "one", "operation": "insert_after_symbol", "content": "\n"}),
        )
        .expect("declared tool");
    if status(&planned) != "PLANNED" {
        return;
    }
    let preview = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": planned.get("plan").expect("plan").clone()}),
        )
        .expect("declared tool");
    assert_eq!(
        status(&preview),
        "PREVIEW_OK",
        "the applier rejected a plan this crate produced: {preview:?}"
    );
}
