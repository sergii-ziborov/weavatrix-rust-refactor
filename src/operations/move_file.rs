//! Relocating a file: every relative import that would break, rewritten.
//!
//! Two directions have to be handled and they are easy to confuse. The moved file's own
//! specifiers were written from its old directory and now resolve from the new one; its
//! importers' specifiers pointed at the old location and now have to point at the new one.
//! Missing either leaves a repository that looks refactored and does not build.
//!
//! Only relative specifiers are touched. A module name is not a path, and rewriting one would
//! turn a working import into a broken identifier.

use crate::coordinates::utf16_offset;
use crate::evidence::read_source;
use crate::plan::{PlanBuilder, sha256_of};
use crate::specifier::{between, is_relative, resolve, rewrite};
use blazingly_json::{Value, json};
use weavatrix_rust::{NodeKind, RepositoryState};

/// One specifier that has to change, already located in its file.
struct Rewrite {
    file: String,
    line: u32,
    start_char: u32,
    end_char: u32,
    before: String,
    after: String,
}

/// Locates a specifier inside the import statement the parser reported.
///
/// The parser's span covers the whole statement, so the specifier is found within it rather than
/// assumed to be at its end — `import x from './a'` and `export * from './a'` put it in
/// different places, and quoting style varies.
fn locate_specifier(source: &str, span: (usize, usize), specifier: &str) -> Option<(usize, usize)> {
    let statement = source.get(span.0..span.1)?;
    let offset = statement.find(specifier)?;
    Some((span.0 + offset, span.0 + offset + specifier.len()))
}

/// Turns a byte range into the plan's line and UTF-16 columns.
fn positioned(source: &str, range: (usize, usize)) -> Option<(u32, u32, u32)> {
    let before = source.get(..range.0)?;
    let line = u32::try_from(before.matches('\n').count() + 1).ok()?;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let start_column = u32::try_from(range.0 - line_start + 1).ok()?;
    let end_column = u32::try_from(range.1 - line_start + 1).ok()?;
    let start_char = utf16_offset(source, line, start_column).ok()?;
    let end_char = utf16_offset(source, line, end_column).ok()?;
    Some((line, start_char, end_char))
}

/// Every indexed file, so importers can be found without a resolved import graph.
fn indexed_files(state: &RepositoryState) -> Vec<String> {
    let mut files = state
        .graph()
        .nodes()
        .iter()
        .filter(|node| node.kind == NodeKind::File)
        .map(|node| node.label.clone())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    files.sort_unstable();
    files.dedup();
    files
}

/// Rewrites the moved file's own specifiers for its new directory.
fn own_imports(source: &str, from: &str, to: &str) -> (Vec<Rewrite>, Vec<Value>) {
    let mut rewrites = Vec::new();
    let mut uncertain = Vec::new();
    let Some(facts) = weavatrix_parse::extract_path(from, source) else {
        return (rewrites, uncertain);
    };
    for import in &facts.imports {
        if !is_relative(&import.specifier) {
            continue;
        }
        let Some(resolved) = resolve(from, &import.specifier) else {
            uncertain.push(json!({
                "file": from,
                "specifier": import.specifier,
                "reason": "the specifier resolves outside the repository",
            }));
            continue;
        };
        let updated = rewrite(to, &import.specifier, &resolved);
        if updated == import.specifier {
            continue;
        }
        let Some(range) = locate_specifier(
            source,
            (import.span.start, import.span.end),
            &import.specifier,
        ) else {
            uncertain.push(json!({
                "file": from,
                "specifier": import.specifier,
                "reason": "the specifier could not be located inside its own import statement",
            }));
            continue;
        };
        let Some((line, start_char, end_char)) = positioned(source, range) else {
            continue;
        };
        rewrites.push(Rewrite {
            file: from.to_owned(),
            line,
            start_char,
            end_char,
            before: import.specifier.clone(),
            after: updated,
        });
    }
    (rewrites, uncertain)
}

/// Rewrites every importer's specifier that pointed at the moved file.
fn importer_imports(
    state: &RepositoryState,
    from: &str,
    to: &str,
) -> (Vec<Rewrite>, Vec<Value>, Vec<String>) {
    let mut rewrites = Vec::new();
    let mut uncertain = Vec::new();
    let mut importers = Vec::new();
    for file in indexed_files(state) {
        if file == from {
            continue;
        }
        let Some(source) = read_source(state.root(), &file) else {
            continue;
        };
        let Some(facts) = weavatrix_parse::extract_path(&file, &source) else {
            continue;
        };
        for import in &facts.imports {
            if !is_relative(&import.specifier) {
                continue;
            }
            let Some(resolved) = resolve(&file, &import.specifier) else {
                continue;
            };
            // The specifier may omit the extension, so compare on the stem too.
            let points_at_moved = resolved == from
                || from
                    .rsplit_once('.')
                    .is_some_and(|(stem, _)| resolved == stem);
            if !points_at_moved {
                continue;
            }
            importers.push(file.clone());
            let updated = between(&file, to);
            let updated = if import.specifier.contains('.')
                && import
                    .specifier
                    .rsplit('/')
                    .next()
                    .is_some_and(|last| last.contains('.'))
            {
                updated
            } else {
                updated
                    .rsplit_once('.')
                    .map_or(updated.clone(), |(stem, _)| stem.to_owned())
            };
            let Some(range) = locate_specifier(
                &source,
                (import.span.start, import.span.end),
                &import.specifier,
            ) else {
                uncertain.push(json!({
                    "file": file,
                    "specifier": import.specifier,
                    "reason": "the specifier could not be located inside its import statement",
                }));
                continue;
            };
            let Some((line, start_char, end_char)) = positioned(&source, range) else {
                continue;
            };
            rewrites.push(Rewrite {
                file: file.clone(),
                line,
                start_char,
                end_char,
                before: import.specifier.clone(),
                after: updated,
            });
        }
    }
    importers.sort_unstable();
    importers.dedup();
    (rewrites, uncertain, importers)
}

pub(super) fn move_file(state: &RepositoryState, arguments: &Value) -> Value {
    let from = arguments.get("from").and_then(Value::as_str);
    let to = arguments.get("to").and_then(Value::as_str);
    let (Some(from), Some(to)) = (from, to) else {
        return super::invalid_args("move_file", &["from", "to"]);
    };
    if from == to {
        return json!({
            "status": "NO_CHANGE",
            "reason": format!("{from} is already at that path"),
        });
    }
    let Some(source) = read_source(state.root(), from) else {
        return json!({
            "status": "SOURCE_UNAVAILABLE",
            "reason": format!("{from}: the file is missing, too large, or not valid UTF-8"),
        });
    };

    let (own, mut uncertain) = own_imports(&source, from, to);
    let (importer, importer_uncertain, importers) = importer_imports(state, from, to);
    uncertain.extend(importer_uncertain);

    let mut builder = PlanBuilder::new("move_file");
    let mut current = String::new();
    for change in own.iter().chain(importer.iter()) {
        if change.file != current {
            let Some(text) = read_source(state.root(), &change.file) else {
                continue;
            };
            builder = builder.file(&change.file, &sha256_of(&text));
            current.clone_from(&change.file);
        }
        builder = builder.edit(
            change.line,
            change.start_char,
            change.line,
            change.end_char,
            change.before.clone(),
            change.after.clone(),
            "EXTRACTED",
        );
    }

    let total = own.len() + importer.len();
    json!({
        "status": "PLANNED",
        "completeness": if uncertain.is_empty() { "COMPLETE" } else { "PARTIAL" },
        "move": {"from": from, "to": to},
        "specifierEdits": total,
        "importers": importers,
        "plan": builder.build(),
        "uncertainReferences": uncertain,
        "warnings": ["FILE_RENAME_NOT_INCLUDED"],
        "next": "apply the specifier edits with apply_edit_plan (preview -> confirm), then move \
                 the file itself: weavatrix.edit-plan.v1 carries text edits only, so the rename \
                 is a separate, deliberate step.",
    })
}
