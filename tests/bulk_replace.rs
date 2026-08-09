//! `bulk_replace` against real files, including the trap the benchmark found.
//!
//! The point of two stages is that a caller can see what a pattern matched before anything is
//! planned. So the tests care most about the cases where "replace them all" would be wrong: a
//! literal that also appears in a string, and a selection that must exclude it.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::Weavatrix;
use weavatrix_rust_refactor::operations::RefactorSession;

fn repository(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-bulk-{}-{name}", std::process::id()));
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

fn status(value: &Value) -> &str {
    value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn call(root: &std::path::Path, arguments: &Value) -> Value {
    let engine = Weavatrix::open(root).expect("repository opens");
    RefactorSession::new(true)
        .call(engine.state(), "bulk_replace", arguments)
        .expect("declared tool")
}

fn occurrences(value: &Value) -> Vec<(String, String)> {
    value
        .get("occurrences")
        .and_then(Value::as_array)
        .map(|found| {
            found
                .iter()
                .map(|occurrence| {
                    (
                        occurrence
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        occurrence
                            .get("file")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

const CODE: &str = "pub fn resolve() -> &'static str {\n    \"resolve\"\n}\n";

#[test]
fn the_first_stage_lists_occurrences_and_plans_nothing() {
    let root = repository("preview", &[("src/lib.rs", CODE)]);
    let answer = call(
        &root,
        &json!({"pattern": "resolve", "replacement": "locate"}),
    );
    assert_eq!(status(&answer), "PREVIEW");
    assert!(answer.get("plan").is_none(), "stage one must not plan");
    assert_eq!(
        answer.get("total").and_then(Value::as_u64),
        Some(2),
        "the declaration and the string literal both match"
    );
}

#[test]
fn a_selection_plans_only_the_chosen_occurrence() {
    let root = repository("select", &[("src/lib.rs", CODE)]);
    let preview = call(
        &root,
        &json!({"pattern": "resolve", "replacement": "locate"}),
    );
    let ids = occurrences(&preview);
    assert_eq!(ids.len(), 2);
    let planned = call(
        &root,
        &json!({
            "pattern": "resolve",
            "replacement": "locate",
            "occurrence_ids": [ids[0].0.clone()],
        }),
    );
    assert_eq!(status(&planned), "PLANNED");
    assert_eq!(planned.get("total").and_then(Value::as_u64), Some(1));
}

#[test]
fn planning_everything_requires_the_count_to_match() {
    let root = repository("count", &[("src/lib.rs", CODE)]);
    let wrong = call(
        &root,
        &json!({"pattern": "resolve", "replacement": "locate", "expected_count": 99}),
    );
    assert_eq!(status(&wrong), "COUNT_MISMATCH");

    let right = call(
        &root,
        &json!({"pattern": "resolve", "replacement": "locate", "expected_count": 2}),
    );
    assert_eq!(status(&right), "PLANNED");
    assert_eq!(right.get("total").and_then(Value::as_u64), Some(2));
}

#[test]
fn an_id_from_a_stale_preview_is_refused_by_name() {
    let root = repository("stale", &[("src/lib.rs", CODE)]);
    let answer = call(
        &root,
        &json!({
            "pattern": "resolve",
            "replacement": "locate",
            "occurrence_ids": ["src/lib.rs@99:0"],
        }),
    );
    assert_eq!(status(&answer), "UNKNOWN_OCCURRENCES");
    assert_eq!(
        answer
            .get("unknown")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1)
    );
}

#[test]
fn an_empty_selection_is_refused_rather_than_treated_as_all() {
    let root = repository("empty", &[("src/lib.rs", CODE)]);
    let answer = call(
        &root,
        &json!({"pattern": "resolve", "replacement": "locate", "occurrence_ids": []}),
    );
    assert_eq!(status(&answer), "NO_SELECTION");
}

#[test]
fn a_pattern_that_matches_nothing_says_so() {
    let root = repository("none", &[("src/lib.rs", CODE)]);
    let answer = call(
        &root,
        &json!({"pattern": "absent_name", "replacement": "x"}),
    );
    assert_eq!(status(&answer), "NO_MATCHES");
}

#[test]
fn a_bad_regex_is_a_pattern_refusal_not_a_crash() {
    let root = repository("badre", &[("src/lib.rs", CODE)]);
    let answer = call(
        &root,
        &json!({"pattern": "(unclosed", "replacement": "x", "literal": false}),
    );
    assert_eq!(status(&answer), "INVALID_PATTERN");
}

#[test]
fn an_unsupported_flag_is_refused() {
    let root = repository("flag", &[("src/lib.rs", CODE)]);
    let answer = call(
        &root,
        &json!({"pattern": "resolve", "replacement": "x", "literal": false, "flags": "g"}),
    );
    assert_eq!(status(&answer), "INVALID_PATTERN");
}

#[test]
fn a_literal_dollar_in_the_replacement_stays_literal() {
    let root = repository("dollar", &[("src/lib.rs", "let price = 1;\n")]);
    let planned = call(
        &root,
        &json!({"pattern": "price", "replacement": "$total", "expected_count": 1}),
    );
    assert_eq!(status(&planned), "PLANNED");
    let after = planned
        .get("plan")
        .and_then(|plan| plan.get("files"))
        .and_then(Value::as_array)
        .and_then(|files| files.first())
        .and_then(|file| file.get("edits"))
        .and_then(Value::as_array)
        .and_then(|edits| edits.first())
        .and_then(|edit| edit.get("after"))
        .and_then(Value::as_str);
    assert_eq!(
        after,
        Some("$total"),
        "in literal mode a dollar is a character, not a capture reference"
    );
}

fn planned_after(planned: &Value) -> Option<&str> {
    planned
        .get("plan")
        .and_then(|plan| plan.get("files"))
        .and_then(Value::as_array)
        .and_then(|files| files.first())
        .and_then(|file| file.get("edits"))
        .and_then(Value::as_array)
        .and_then(|edits| edits.first())
        .and_then(|edit| edit.get("after"))
        .and_then(Value::as_str)
}

#[test]
fn regex_mode_expands_captures() {
    let root = repository("capture", &[("src/lib.rs", "let one_value = 1;\n")]);
    let planned = call(
        &root,
        &json!({
            "pattern": "(\\w+)_value",
            "replacement": "${1}_amount",
            "literal": false,
            "expected_count": 1,
        }),
    );
    assert_eq!(status(&planned), "PLANNED");
    assert_eq!(planned_after(&planned), Some("one_amount"));
}

#[test]
fn an_unbraced_capture_followed_by_word_characters_reads_as_one_name() {
    // `$1_amount` is the group named `1_amount`, not group 1 followed by text — the standard
    // rule, and a trap worth pinning: the braced form is the only unambiguous one. The plan
    // shows the caller exactly what it would write, which is how they notice.
    let root = repository("ambiguous", &[("src/lib.rs", "let one_value = 1;\n")]);
    let planned = call(
        &root,
        &json!({
            "pattern": "(\\w+)_value",
            "replacement": "$1_amount",
            "literal": false,
            "expected_count": 1,
        }),
    );
    assert_eq!(status(&planned), "PLANNED");
    assert_eq!(
        planned_after(&planned),
        Some(""),
        "an unknown group name expands to nothing, and the preview makes that visible"
    );
}

#[test]
fn a_plan_from_bulk_replace_survives_the_applier_and_writes_the_selection() {
    let root = repository("apply", &[("src/lib.rs", CODE)]);
    let engine = Weavatrix::open(&root).expect("repository opens");
    let session = RefactorSession::new(true);
    let state = engine.state().clone();

    let preview = session
        .call(
            &state,
            "bulk_replace",
            &json!({"pattern": "resolve", "replacement": "locate", "expected_count": 2}),
        )
        .expect("declared tool");
    assert_eq!(status(&preview), "PLANNED");
    let plan = preview.get("plan").expect("plan").clone();

    let checked = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("declared tool");
    assert_eq!(status(&checked), "PREVIEW_OK", "{checked:?}");
    let token = checked
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
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "pub fn locate() -> &'static str {\n    \"locate\"\n}\n"
    );
}
