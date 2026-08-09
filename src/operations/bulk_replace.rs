//! Occurrence-selective replacement over the indexed files.
//!
//! Two stages on purpose. The first returns every occurrence with a stable id and changes
//! nothing; the second takes the ids the caller chose, or an exact count of everything, and only
//! then produces a plan. A one-shot replace-all is the operation that quietly edits a string
//! literal in a file nobody looked at — the benchmark caught exactly that, where three of six
//! textual hits belonged to a different symbol.

use crate::coordinates::utf16_offset;
use crate::evidence::read_source;
use crate::plan::{PlanBuilder, sha256_of};
use blazingly_json::{Value, json};
use regex::{Regex, RegexBuilder};
use std::collections::BTreeSet;
use weavatrix_rust::{NodeKind, RepositoryState};

/// Bounds, so a careless pattern cannot turn one call into a repository-sized answer.
const MAX_FILES: usize = 5_000;
const MAX_PER_FILE: usize = 500;
const MAX_TOTAL: usize = 5_000;
const MAX_PATTERN_BYTES: usize = 1_000;

struct Occurrence {
    id: String,
    file: String,
    line: u32,
    start_char: u32,
    end_char: u32,
    before: String,
    after: String,
    excerpt: String,
}

/// Compiles the caller's pattern, refusing anything the scanner could not run safely.
fn matcher(pattern: &str, literal: bool, flags: Option<&str>) -> Result<Regex, Value> {
    if pattern.is_empty() {
        return Err(invalid_pattern("pattern must not be empty"));
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(invalid_pattern(format!(
            "pattern exceeds {MAX_PATTERN_BYTES} bytes"
        )));
    }
    let flags = flags.unwrap_or_default();
    // `g` is deliberately absent: every match is always collected, so accepting the flag would
    // imply a choice the caller does not have.
    if flags.chars().any(|flag| !"ims".contains(flag)) {
        return Err(invalid_pattern("flags may only contain i, m or s"));
    }
    let source = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    RegexBuilder::new(&source)
        .case_insensitive(flags.contains('i'))
        .multi_line(flags.contains('m'))
        .dot_matches_new_line(flags.contains('s'))
        .size_limit(1 << 20)
        .build()
        .map_err(|error| invalid_pattern(error.to_string()))
}

fn invalid_pattern(reason: impl Into<String>) -> Value {
    json!({"status": "INVALID_PATTERN", "reason": reason.into()})
}

/// Every indexed file, in deterministic order so occurrence ids are stable between calls.
///
/// Taken from the file nodes rather than from symbol spans: a file with no declarations is still
/// an indexed file, and skipping it would silently narrow "replace across the repository" to
/// "replace where a symbol happens to live".
fn indexed_files(state: &RepositoryState, prefix: Option<&str>) -> Vec<String> {
    let mut files = state
        .graph()
        .nodes()
        .iter()
        .filter(|node| node.kind == NodeKind::File)
        .map(|node| node.label.clone())
        .filter(|path| !path.is_empty())
        .filter(|path| prefix.is_none_or(|prefix| path.starts_with(prefix)))
        .collect::<Vec<_>>();
    files.sort_unstable();
    files.dedup();
    files.truncate(MAX_FILES);
    files
}

/// Collects occurrences, or the refusal that stops the scan.
fn scan(
    state: &RepositoryState,
    regex: &Regex,
    replacement: &str,
    prefix: Option<&str>,
) -> Result<(Vec<Occurrence>, usize, bool), Value> {
    let mut found = Vec::new();
    let mut scanned = 0_usize;
    let mut capped = false;
    let mut zero_width = 0_usize;
    for file in indexed_files(state, prefix) {
        let Some(source) = read_source(state.root(), &file) else {
            continue;
        };
        scanned += 1;
        let mut in_file = 0_usize;
        for (number, text) in source.split('\n').enumerate() {
            let line = u32::try_from(number + 1).unwrap_or(u32::MAX);
            let text = text.strip_suffix('\r').unwrap_or(text);
            for capture in regex.captures_iter(text) {
                let Some(whole) = capture.get(0) else {
                    continue;
                };
                if whole.is_empty() {
                    zero_width += 1;
                    continue;
                }
                if in_file >= MAX_PER_FILE || found.len() >= MAX_TOTAL {
                    capped = true;
                    break;
                }
                // Columns in the plan are UTF-16; the regex reports byte offsets.
                let (Ok(start_char), Ok(end_char)) = (
                    utf16_offset(&source, line, u32::try_from(whole.start() + 1).unwrap_or(1)),
                    utf16_offset(&source, line, u32::try_from(whole.end() + 1).unwrap_or(1)),
                ) else {
                    continue;
                };
                let mut after = String::new();
                capture.expand(replacement, &mut after);
                found.push(Occurrence {
                    id: format!("{file}@{line}:{start_char}"),
                    file: file.clone(),
                    line,
                    start_char,
                    end_char,
                    before: whole.as_str().to_owned(),
                    after,
                    excerpt: excerpt(text, whole.start(), whole.end()),
                });
                in_file += 1;
            }
            if capped {
                break;
            }
        }
    }
    if found.is_empty() && zero_width > 0 {
        return Err(json!({
            "status": "ZERO_WIDTH_UNSUPPORTED",
            "reason": format!(
                "the pattern matched {zero_width} zero-width position(s); bulk_replace only \
                 replaces non-empty matches. Use edit_symbol for an insertion."
            ),
        }));
    }
    Ok((found, scanned, capped))
}

/// A short window around a match, for a caller choosing between occurrences.
fn excerpt(line: &str, start: usize, end: usize) -> String {
    let from = line[..start]
        .char_indices()
        .rev()
        .nth(29)
        .map_or(0, |(index, _)| index);
    let to = line[end..]
        .char_indices()
        .nth(30)
        .map_or(line.len(), |(index, _)| end + index);
    line.get(from..to).unwrap_or(line).trim().to_owned()
}

pub(super) fn bulk_replace(state: &RepositoryState, arguments: &Value) -> Value {
    let Some(pattern) = arguments.get("pattern").and_then(Value::as_str) else {
        return super::invalid_args("bulk_replace", &["pattern"]);
    };
    let Some(replacement) = arguments.get("replacement").and_then(Value::as_str) else {
        return super::invalid_args("bulk_replace", &["replacement"]);
    };
    let literal = arguments
        .get("literal")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let regex = match matcher(
        pattern,
        literal,
        arguments.get("flags").and_then(Value::as_str),
    ) {
        Ok(regex) => regex,
        Err(refusal) => return refusal,
    };
    // In literal mode the replacement is text, not a template: `$1` in a name is a name.
    let expansion = if literal {
        replacement.replace('$', "$$")
    } else {
        replacement.to_owned()
    };
    let prefix = arguments.get("path_prefix").and_then(Value::as_str);

    let (occurrences, scanned, capped) = match scan(state, &regex, &expansion, prefix) {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    if occurrences.is_empty() {
        return json!({
            "status": "NO_MATCHES",
            "reason": "no occurrence of the pattern exists in the indexed files",
            "scannedFiles": scanned,
        });
    }

    let selection = arguments.get("occurrence_ids").and_then(Value::as_array);
    let expected = arguments.get("expected_count").and_then(Value::as_u64);
    if selection.is_none() && expected.is_none() {
        return preview(&occurrences, scanned, capped);
    }
    plan(&occurrences, selection, expected, state, scanned)
}

fn preview(occurrences: &[Occurrence], scanned: usize, capped: bool) -> Value {
    json!({
        "status": "PREVIEW",
        "total": occurrences.len(),
        "scannedFiles": scanned,
        "capped": capped,
        "occurrences": occurrences.iter().map(|occurrence| json!({
            "id": occurrence.id,
            "file": occurrence.file,
            "line": occurrence.line,
            "before": occurrence.before,
            "after": occurrence.after,
            "excerpt": occurrence.excerpt,
        })).collect::<Vec<_>>(),
        "next": "call again with occurrence_ids=[...] to plan a selection, or \
                 expected_count=<total> to plan every one of them",
    })
}

fn plan(
    occurrences: &[Occurrence],
    selection: Option<&Vec<Value>>,
    expected: Option<u64>,
    state: &RepositoryState,
    scanned: usize,
) -> Value {
    let chosen = if let Some(selection) = selection {
        let wanted = selection
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();
        if wanted.is_empty() {
            return json!({
                "status": "NO_SELECTION",
                "reason": "occurrence_ids was empty; select at least one occurrence, or pass \
                           expected_count to plan them all",
            });
        }
        let known = occurrences
            .iter()
            .map(|occurrence| occurrence.id.clone())
            .collect::<BTreeSet<_>>();
        let unknown = wanted.difference(&known).cloned().collect::<Vec<_>>();
        if !unknown.is_empty() {
            return json!({
                "status": "UNKNOWN_OCCURRENCES",
                "reason": "some occurrence_ids do not match the current scan; the files changed. \
                           Preview again and reselect.",
                "unknown": unknown,
            });
        }
        occurrences
            .iter()
            .filter(|occurrence| wanted.contains(&occurrence.id))
            .collect::<Vec<_>>()
    } else {
        let expected = expected.unwrap_or_default();
        if expected != occurrences.len() as u64 {
            return json!({
                "status": "COUNT_MISMATCH",
                "reason": format!(
                    "expected_count is {expected} but the scan found {}; the files changed since \
                     the preview. Nothing was planned.",
                    occurrences.len()
                ),
                "total": occurrences.len(),
            });
        }
        occurrences.iter().collect::<Vec<_>>()
    };

    let mut builder = PlanBuilder::new("bulk_replace");
    let mut current = String::new();
    for occurrence in &chosen {
        if occurrence.file != current {
            let Some(source) = read_source(state.root(), &occurrence.file) else {
                return json!({
                    "status": "SOURCE_UNAVAILABLE",
                    "reason": format!("{}: unreadable while planning", occurrence.file),
                });
            };
            builder = builder.file(&occurrence.file, &sha256_of(&source));
            current.clone_from(&occurrence.file);
        }
        builder = builder.edit(
            occurrence.line,
            occurrence.start_char,
            occurrence.line,
            occurrence.end_char,
            occurrence.before.clone(),
            occurrence.after.clone(),
            "LEXICAL_EXACT",
        );
    }
    json!({
        "status": "PLANNED",
        "completeness": "COMPLETE",
        "total": chosen.len(),
        "scannedFiles": scanned,
        "plan": builder.build(),
        "warnings": ["INDEXED_FILES_ONLY"],
        "next": "apply with apply_edit_plan (preview -> confirm)",
    })
}
