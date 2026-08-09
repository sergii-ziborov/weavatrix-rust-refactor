//! `move_file` on a real tree, applied end to end.
//!
//! Both directions have to be right: the moved file's own specifiers, written from its old
//! directory, and its importers' specifiers, which pointed at the old location. A test that
//! only checks one of them passes on a change that leaves the repository unbuildable.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::Weavatrix;
use weavatrix_rust_refactor::operations::RefactorSession;

fn repository(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-movefile-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (path, contents) in files {
        let full = root.join(path);
        fs::create_dir_all(full.parent().expect("parent")).expect("directories");
        fs::write(full, contents).expect("source");
    }
    fs::write(
        root.join("package.json"),
        "{\n  \"name\": \"fixture\",\n  \"version\": \"1.0.0\",\n  \"type\": \"module\"\n}\n",
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
        .call(engine.state(), "move_file", arguments)
        .expect("declared tool")
}

/// `src/main.js` imports `./core.js`; `src/other.js` also imports `./core.js`.
const FILES: [(&str, &str); 3] = [
    ("src/core.js", "export const value = 1\n"),
    (
        "src/main.js",
        "import {value} from './core.js'\n\nexport const doubled = value * 2\n",
    ),
    (
        "src/other.js",
        "import {value} from './core.js'\n\nexport const tripled = value * 3\n",
    ),
];

#[test]
fn a_missing_argument_is_invalid_args() {
    let root = repository("args", &FILES);
    assert_eq!(
        status(&call(&root, &json!({"from": "src/core.js"}))),
        "INVALID_ARGS"
    );
}

#[test]
fn moving_a_file_to_its_own_path_is_no_change() {
    let root = repository("same", &FILES);
    let answer = call(&root, &json!({"from": "src/core.js", "to": "src/core.js"}));
    assert_eq!(status(&answer), "NO_CHANGE");
}

#[test]
fn a_missing_file_is_source_unavailable() {
    let root = repository("gone", &FILES);
    let answer = call(
        &root,
        &json!({"from": "src/absent.js", "to": "src/other/absent.js"}),
    );
    assert_eq!(status(&answer), "SOURCE_UNAVAILABLE");
}

#[test]
fn every_importer_of_the_moved_file_is_named_and_rewritten() {
    let root = repository("importers", &FILES);
    let answer = call(
        &root,
        &json!({"from": "src/core.js", "to": "src/deep/core.js"}),
    );
    assert_eq!(status(&answer), "PLANNED", "{answer:?}");
    let importers = answer
        .get("importers")
        .and_then(Value::as_array)
        .expect("importers");
    assert_eq!(
        importers.len(),
        2,
        "both consumers must appear: {importers:?}"
    );
    assert_eq!(
        answer.get("specifierEdits").and_then(Value::as_u64),
        Some(2)
    );
}

#[test]
fn the_rename_itself_is_declared_as_a_separate_step() {
    let root = repository("rename", &FILES);
    let answer = call(
        &root,
        &json!({"from": "src/core.js", "to": "src/deep/core.js"}),
    );
    let warnings = answer
        .get("warnings")
        .and_then(Value::as_array)
        .expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str() == Some("FILE_RENAME_NOT_INCLUDED")),
        "a plan of text edits must not read as if it moved the file"
    );
}

#[test]
fn applying_the_plan_leaves_every_importer_pointing_at_the_new_path() {
    let root = repository("apply", &FILES);
    let engine = Weavatrix::open(&root).expect("repository opens");
    let session = RefactorSession::new(true);
    let state = engine.state().clone();

    let planned = session
        .call(
            &state,
            "move_file",
            &json!({"from": "src/core.js", "to": "src/deep/core.js"}),
        )
        .expect("declared tool");
    assert_eq!(status(&planned), "PLANNED");
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

    for importer in ["src/main.js", "src/other.js"] {
        let contents = fs::read_to_string(root.join(importer)).expect("importer");
        assert!(
            contents.contains("'./deep/core.js'"),
            "{importer} must point at the new path, got {contents:?}"
        );
    }
}

#[test]
fn the_moved_file_own_specifiers_are_rewritten_for_the_new_directory() {
    // main.js imports ./core.js from src/; moved to src/deep/ it must climb to ../core.js.
    let root = repository(
        "own",
        &[
            ("src/core.js", "export const value = 1\n"),
            (
                "src/main.js",
                "import {value} from './core.js'\n\nexport const doubled = value * 2\n",
            ),
        ],
    );
    let engine = Weavatrix::open(&root).expect("repository opens");
    let session = RefactorSession::new(true);
    let state = engine.state().clone();

    let planned = session
        .call(
            &state,
            "move_file",
            &json!({"from": "src/main.js", "to": "src/deep/main.js"}),
        )
        .expect("declared tool");
    assert_eq!(status(&planned), "PLANNED", "{planned:?}");
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
    session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": plan, "mode": "apply", "confirm_token": token}),
        )
        .expect("declared tool");

    let contents = fs::read_to_string(root.join("src/main.js")).expect("moved file");
    assert!(
        contents.contains("'../core.js'"),
        "the moved file must climb out of its new directory, got {contents:?}"
    );
}

#[test]
fn module_names_are_left_alone() {
    let root = repository(
        "modules",
        &[
            ("src/core.js", "export const value = 1\n"),
            (
                "src/main.js",
                "import react from 'react'\nimport {value} from './core.js'\n",
            ),
        ],
    );
    let answer = call(
        &root,
        &json!({"from": "src/core.js", "to": "src/deep/core.js"}),
    );
    assert_eq!(status(&answer), "PLANNED");
    let edits = answer
        .get("plan")
        .and_then(|plan| plan.get("files"))
        .and_then(Value::as_array)
        .and_then(|files| files.first())
        .and_then(|file| file.get("edits"))
        .and_then(Value::as_array)
        .expect("edits");
    assert_eq!(edits.len(), 1, "only the path specifier changes: {edits:?}");
    assert_eq!(
        edits[0].get("before").and_then(Value::as_str),
        Some("./core.js")
    );
}
