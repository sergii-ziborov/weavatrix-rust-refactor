//! Renaming a symbol from graph evidence.
//!
//! Every edit here is anchored to a reference the graph proved: the declaration, and each site
//! that calls or references it. Within those lines the identifier is matched on word boundaries,
//! so `resolveTarget` never matches inside `resolveTargetPath`.
//!
//! What this backend cannot do is prove the *absence* of other references, so it never claims
//! `COMPLETE`. Occurrences of the same name that the graph does not vouch for are reported as
//! uncertain rather than renamed — that is the difference between renaming a symbol and
//! find-replacing a string, and the benchmark's shadow trap is exactly the case where they
//! diverge.

use crate::coordinates::utf16_offset;
use crate::evidence::{declaring_file, read_source};
use crate::plan::{PlanBuilder, sha256_of};
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use weavatrix_rust::{EdgeKind, RepositoryState};

/// Relations that mean "this site uses that symbol", as opposed to containing it.
fn is_use(kind: &EdgeKind) -> bool {
    matches!(kind, EdgeKind::Calls | EdgeKind::References)
}

/// A valid identifier in the languages this backend serves.
fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
}

/// Word-boundary occurrences of `name` on a 1-based line, as (start, end) byte columns.
fn occurrences_on_line(source: &str, line: u32, name: &str) -> Vec<(u32, u32)> {
    let Some(text) = source
        .split('\n')
        .nth(usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX))
    else {
        return Vec::new();
    };
    let text = text.strip_suffix('\r').unwrap_or(text);
    let boundary = |character: Option<char>| {
        character.is_none_or(|character| !character.is_alphanumeric() && character != '_')
    };
    let mut hits = Vec::new();
    let mut from = 0_usize;
    while let Some(found) = text[from..].find(name) {
        let start = from + found;
        let end = start + name.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        if boundary(before)
            && boundary(after)
            && let (Ok(start), Ok(end)) = (u32::try_from(start + 1), u32::try_from(end + 1))
        {
            hits.push((start, end));
        }
        from = start + 1;
    }
    hits
}

/// Files and lines the graph proves reference this symbol, plus the declaration's own line.
fn reference_lines(
    state: &RepositoryState,
    id: &str,
    declaring: (&str, u32),
) -> BTreeMap<String, BTreeSet<u32>> {
    let mut by_file: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    by_file
        .entry(declaring.0.to_owned())
        .or_default()
        .insert(declaring.1);
    for edge in state.graph().edges() {
        if !is_use(&edge.kind) || edge.target.as_str() != id {
            continue;
        }
        let Some(source_node) = state
            .graph()
            .nodes()
            .iter()
            .find(|node| node.id.as_str() == edge.source.as_str())
        else {
            continue;
        };
        let Some(span) = source_node.span.as_ref() else {
            continue;
        };
        // A use edge points at the referencing symbol, whose span is its own declaration line.
        // The reference itself may sit anywhere inside that symbol, so its whole range is
        // searched rather than only its first line.
        for line in span.start.line..=span.end.line.max(span.start.line) {
            by_file.entry(span.file.clone()).or_default().insert(line);
        }
    }
    by_file
}

/// Plans one file: renames on graph-proven lines, reports every other same-named occurrence.
///
/// The two halves belong together because they partition the same file. An occurrence is either
/// on a line the graph attributes to this symbol — planned — or it is not, and then it is
/// reported. Nothing is silently dropped, which is what makes the uncertain list trustworthy.
fn plan_file(
    mut builder: PlanBuilder,
    path: &str,
    source: &str,
    proven: &BTreeSet<u32>,
    names: (&str, &str),
    uncertain: &mut Vec<Value>,
) -> (PlanBuilder, usize) {
    let (old_name, new_name) = names;
    let mut edits = 0_usize;
    let mut opened = false;
    for line in proven {
        for (start_column, end_column) in occurrences_on_line(source, *line, old_name) {
            let (Ok(start_char), Ok(end_char)) = (
                utf16_offset(source, *line, start_column),
                utf16_offset(source, *line, end_column),
            ) else {
                continue;
            };
            if !opened {
                builder = builder.file(path, &sha256_of(source));
                opened = true;
            }
            builder = builder.edit(
                *line,
                start_char,
                *line,
                end_char,
                old_name.to_owned(),
                new_name.to_owned(),
                "RESOLVED",
            );
            edits += 1;
        }
    }
    // Occurrences outside a graph-proven line are exactly what a find-replace would rename and
    // this must not: they may belong to a different symbol that happens to share the name.
    for (number, text) in source.split('\n').enumerate() {
        let line = u32::try_from(number + 1).unwrap_or(u32::MAX);
        if proven.contains(&line) || occurrences_on_line(source, line, old_name).is_empty() {
            continue;
        }
        uncertain.push(json!({
            "file": path,
            "line": line,
            "kind": "UNPROVEN_OCCURRENCE",
            "excerpt": text.trim(),
        }));
    }
    (builder, edits)
}

pub(super) fn rename_symbol(state: &RepositoryState, arguments: &Value) -> Value {
    let symbol = arguments.get("symbol").and_then(Value::as_str);
    let new_name = arguments.get("new_name").and_then(Value::as_str);
    let (Some(symbol), Some(new_name)) = (symbol, new_name) else {
        return super::invalid_args("rename_symbol", &["symbol", "new_name"]);
    };
    if !is_identifier(new_name) {
        return json!({
            "status": "INVALID_NEW_NAME",
            "reason": format!("{new_name:?} is not a valid identifier"),
        });
    }
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return super::not_found(symbol);
    };
    let Some(node) = state.graph().node_at(index) else {
        return super::not_found(symbol);
    };
    let old_name = node.label.trim_end_matches("()").to_owned();
    if !is_identifier(&old_name) {
        return json!({
            "status": "NOT_SUPPORTED",
            "reason": format!(
                "the graph records this symbol as {:?}, which is not a plain identifier; rename \
                 needs one to anchor its edits",
                node.label
            ),
        });
    }
    if old_name == new_name {
        return json!({"status": "NO_CHANGE", "reason": "the symbol already has that name"});
    }
    let Some(file) = declaring_file(node) else {
        return super::not_found(symbol);
    };
    let Some(span) = node.span.as_ref() else {
        return super::not_found(symbol);
    };

    let id = node.id.as_str().to_owned();
    let lines = reference_lines(state, &id, (&file, span.start.line));
    let mut builder = PlanBuilder::new("rename_symbol");
    let mut edits = 0_usize;
    let mut uncertain = Vec::new();

    for (path, line_numbers) in &lines {
        let Some(source) = read_source(state.root(), path) else {
            uncertain.push(json!({
                "file": path,
                "reason": "the file is unreadable, so its references were not planned",
            }));
            continue;
        };
        let planned = plan_file(
            builder,
            path,
            &source,
            line_numbers,
            (&old_name, new_name),
            &mut uncertain,
        );
        builder = planned.0;
        edits += planned.1;
    }

    if edits == 0 {
        return json!({
            "status": "NO_EDITS",
            "reason": format!(
                "{old_name} was not found on any line the graph attributes to this symbol; \
                 rebuild the graph if the file changed"
            ),
        });
    }
    json!({
        "status": "PLANNED",
        // Never COMPLETE: this backend proves the sites it renames, not the absence of others.
        "completeness": "PARTIAL",
        "backend": "graph+lexical",
        "oldName": old_name,
        "newName": new_name,
        "renamedEdits": edits,
        "plan": builder.build(),
        "uncertainReferences": uncertain,
        "warnings": ["GRAPH_PROVEN_SITES_ONLY"],
        "next": "apply with apply_edit_plan (preview -> confirm). Every uncertainReference is a \
                 same-named occurrence this backend refused to guess at; review them yourself.",
    })
}
