//! `organize_imports` on real files.
//!
//! The round trip matters more than the statuses here. Every deletion carries the exact bytes it
//! claims to remove, and the applier re-checks them, so an apply that succeeds and leaves valid
//! source is the proof that the ranges were right — a plan that merely looks plausible would be
//! rejected at preview.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::Weavatrix;
use weavatrix_rust_refactor::operations::RefactorSession;

fn repository(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-imports-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (path, contents) in files {
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

fn organize(root: &std::path::Path, file: &str) -> Value {
    let engine = Weavatrix::open(root).expect("repository opens");
    RefactorSession::read_only()
        .call(engine.state(), "organize_imports", &json!({"file": file}))
        .expect("declared tool")
}

/// Plans, previews and applies, returning the file as it ends up on disk.
fn apply(root: &std::path::Path, file: &str) -> String {
    let engine = Weavatrix::open(root).expect("opens");
    let state = engine.state().clone();
    let session = RefactorSession::new(true);
    let planned = session
        .call(&state, "organize_imports", &json!({"file": file}))
        .expect("declared tool");
    assert_eq!(status(&planned), "PLANNED", "{planned:?}");
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("declared tool");
    assert_eq!(
        status(&preview),
        "PREVIEW_OK",
        "the removed bytes did not match the file: {preview:?}"
    );
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
    fs::read_to_string(root.join(file)).expect("file")
}

#[test]
fn a_missing_file_argument_is_invalid_args() {
    let root = repository("args", &[("src/a.js", "export const a = 1;\n")]);
    let engine = Weavatrix::open(&root).expect("opens");
    let answer = RefactorSession::read_only()
        .call(engine.state(), "organize_imports", &json!({}))
        .expect("declared tool");
    assert_eq!(status(&answer), "INVALID_ARGS");
}

#[test]
fn a_file_outside_the_repository_is_source_unavailable() {
    let root = repository("missing", &[("src/a.js", "export const a = 1;\n")]);
    assert_eq!(
        status(&organize(&root, "src/nope.js")),
        "SOURCE_UNAVAILABLE"
    );
}

#[test]
fn a_file_without_imports_has_nothing_to_organize() {
    let root = repository("none", &[("src/a.js", "export const a = 1;\n")]);
    assert_eq!(status(&organize(&root, "src/a.js")), "NO_UNUSED_IMPORTS");
}

#[test]
fn bindings_that_are_used_are_left_alone() {
    let root = repository(
        "used",
        &[(
            "src/a.js",
            "import { one, two } from './lib.js';\n\nexport const sum = one(1) + two(2);\n",
        )],
    );
    let answer = organize(&root, "src/a.js");
    assert_eq!(status(&answer), "NO_UNUSED_IMPORTS", "{answer:?}");
}

#[test]
fn a_name_that_appears_only_in_a_comment_is_still_unused() {
    let root = repository(
        "comment",
        &[(
            "src/a.js",
            "import { one, two } from './lib.js';\n\n// two is mentioned here only\nexport const x = one(1);\n",
        )],
    );
    let answer = organize(&root, "src/a.js");
    assert_eq!(status(&answer), "PLANNED", "{answer:?}");
    let removed = answer
        .get("removed")
        .and_then(Value::as_array)
        .expect("removed");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].as_str(), Some("two"));
}

#[test]
fn a_name_that_appears_only_inside_a_string_is_still_unused() {
    let root = repository(
        "string",
        &[(
            "src/a.js",
            "import { one, two } from './lib.js';\n\nexport const label = 'two';\nexport const x = one(1);\n",
        )],
    );
    assert_eq!(status(&organize(&root, "src/a.js")), "PLANNED");
}

#[test]
fn a_default_import_is_reported_and_never_removed() {
    let root = repository(
        "default",
        &[(
            "src/a.jsx",
            "import React from 'react';\n\nexport const view = <div />;\n",
        )],
    );
    let answer = organize(&root, "src/a.jsx");
    let uncertain = answer
        .get("uncertain")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("binding").and_then(Value::as_str))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        uncertain.contains(&"React".to_owned()),
        "a default import has to be reported rather than judged: {answer:?}"
    );
    assert!(
        answer
            .get("removed")
            .and_then(Value::as_array)
            .is_none_or(|removed| !removed.iter().any(|name| name.as_str() == Some("React"))),
        "{answer:?}"
    );
}

#[test]
fn one_unused_binding_leaves_the_rest_of_the_import_intact() {
    let root = repository(
        "one",
        &[(
            "src/a.js",
            "import { one, two } from './lib.js';\n\nexport const x = one(1);\n",
        )],
    );
    let after = apply(&root, "src/a.js");
    assert_eq!(
        after, "import { one } from './lib.js';\n\nexport const x = one(1);\n",
        "got {after:?}"
    );
}

#[test]
fn an_unused_first_binding_takes_its_own_comma() {
    let root = repository(
        "first",
        &[(
            "src/a.js",
            "import { one, two } from './lib.js';\n\nexport const x = two(1);\n",
        )],
    );
    let after = apply(&root, "src/a.js");
    assert_eq!(
        after, "import { two } from './lib.js';\n\nexport const x = two(1);\n",
        "got {after:?}"
    );
}

#[test]
fn an_import_whose_every_binding_is_unused_is_removed_whole() {
    let root = repository(
        "whole",
        &[(
            "src/a.js",
            "import { one } from './lib.js';\nimport { keep } from './other.js';\n\nexport const x = keep(1);\n",
        )],
    );
    let after = apply(&root, "src/a.js");
    assert_eq!(
        after, "import { keep } from './other.js';\n\nexport const x = keep(1);\n",
        "the whole statement and its line should be gone, got {after:?}"
    );
}

#[test]
fn a_statement_without_a_semicolon_still_takes_its_module_specifier() {
    let root = repository(
        "nosemi",
        &[(
            "src/a.js",
            "import { one } from './lib.js'\nimport { keep } from './other.js'\n\nexport const x = keep(1)\n",
        )],
    );
    let after = apply(&root, "src/a.js");
    assert!(
        !after.contains("./lib.js"),
        "the specifier must go with the statement, got {after:?}"
    );
    assert!(
        after.starts_with("import { keep } from './other.js'"),
        "got {after:?}"
    );
}

#[test]
fn the_braces_go_but_the_default_beside_them_stays() {
    let root = repository(
        "beside",
        &[(
            "src/a.js",
            "import base, { unused } from './lib.js';\n\nexport const x = base(1);\n",
        )],
    );
    let after = apply(&root, "src/a.js");
    assert_eq!(
        after, "import base from './lib.js';\n\nexport const x = base(1);\n",
        "got {after:?}"
    );
}

#[test]
fn an_alias_is_judged_by_the_local_name() {
    let root = repository(
        "alias",
        &[(
            "src/a.js",
            "import { original as local, other } from './lib.js';\n\nexport const x = local(1);\n",
        )],
    );
    let after = apply(&root, "src/a.js");
    assert_eq!(
        after, "import { original as local } from './lib.js';\n\nexport const x = local(1);\n",
        "got {after:?}"
    );
}

#[test]
fn a_multi_line_import_keeps_the_bindings_it_still_needs() {
    let root = repository(
        "multiline",
        &[(
            "src/a.js",
            "import {\n  one,\n  two,\n  three,\n} from './lib.js';\n\nexport const x = one(1) + three(3);\n",
        )],
    );
    let after = apply(&root, "src/a.js");
    assert!(
        after.contains("one") && after.contains("three") && !after.contains("two"),
        "got {after:?}"
    );
}

#[test]
fn a_dynamic_import_expression_is_not_treated_as_a_statement() {
    let root = repository(
        "dynamic",
        &[(
            "src/a.js",
            "export async function load() {\n  return import('./lib.js');\n}\n",
        )],
    );
    assert_eq!(status(&organize(&root, "src/a.js")), "NO_UNUSED_IMPORTS");
}

#[test]
fn a_rust_file_is_reported_unproven_rather_than_edited() {
    let root = repository(
        "rust",
        &[(
            "src/main.rs",
            "use std::io::Write;\n\nfn main() {\n    let mut out = std::io::stdout();\n    out.write_all(b\"x\").unwrap();\n}\n",
        )],
    );
    let answer = organize(&root, "src/main.rs");
    assert_eq!(
        status(&answer),
        "UNPROVEN",
        "removing a trait import that is used through its methods would break the build: \
         {answer:?}"
    );
    assert!(answer.get("plan").is_none(), "{answer:?}");
}
