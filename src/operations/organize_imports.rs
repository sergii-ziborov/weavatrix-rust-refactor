//! Removing named imports that provably nothing uses.
//!
//! The rule is deliberately narrow: a named binding goes only when its identifier occurs exactly
//! once in the whole file — the import itself. Anything that reads like a use keeps it, including
//! a property access or an object shorthand that merely shares the name. Being wrong in that
//! direction leaves a tidy-up undone; being wrong in the other direction deletes a binding the
//! program needs, and the file no longer compiles.
//!
//! Default and namespace imports are never removed. A default import can be used by a transform
//! rather than by name — the classic case is `React` under JSX — so counting its occurrences
//! proves nothing. They are reported instead.
//!
//! Other languages get the same analysis and no edits. In Rust `use std::io::Write` is used by
//! calling `write_all`, never by naming `Write`; in Python an import in `__init__.py` is often
//! the public API. Both would pass an occurrence count and both would break on removal, so the
//! answer there is the candidate list and `UNPROVEN`, not a plan.

use crate::coordinates::utf16_offset;
use crate::evidence::read_source;
use crate::plan::{PlanBuilder, sha256_of};
use blazingly_json::{Value, json};
use weavatrix_parse::{Language, Token, TokenKind, tokenize};
use weavatrix_rust::RepositoryState;

/// Files past this are not analysed; the answer would be slow and nobody hand-edits them.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// One named binding inside `{ ... }`, and the exact bytes it occupies.
struct Binding {
    /// The name this file would refer to it by: `z` in `y as z`.
    local: String,
    start: usize,
    end: usize,
}

/// One `import` statement, located but not yet judged.
struct Import {
    /// The whole statement, `import` through the terminating `;`.
    start: usize,
    end: usize,
    named: Vec<Binding>,
    /// Byte range of the `{ ... }` group, when the statement has one.
    braces: Option<(usize, usize)>,
    /// Default and namespace bindings, which are reported and never removed.
    unnamed: Vec<String>,
}

/// Tokens that carry program meaning, so an identifier in a comment is not a use.
///
/// String literals are kept: the module specifier is one, and a statement whose range stopped
/// before it would leave `'./lib.js'` behind after the import was deleted. They cannot be
/// mistaken for uses either — the tokenizer emits a literal as a single token, so no identifier
/// is ever produced from inside one.
fn is_code(token: &Token) -> bool {
    !matches!(
        token.kind,
        TokenKind::Whitespace
            | TokenKind::Newline
            | TokenKind::Indent
            | TokenKind::LineComment
            | TokenKind::BlockComment
            | TokenKind::Unterminated
    )
}

/// Reads the bindings of one `{ ... }` group, starting just past the `{`.
///
/// Returns the bindings and the index of the token after the closing `}`.
fn read_group(source: &str, code: &[&Token], mut at: usize) -> (Vec<Binding>, usize) {
    let mut bindings = Vec::new();
    while at < code.len() {
        let text = code[at].text(source);
        if text == "}" {
            return (bindings, at + 1);
        }
        if text == "," {
            at += 1;
            continue;
        }
        if code[at].kind != TokenKind::Identifier {
            at += 1;
            continue;
        }
        let start = code[at].start;
        // `import { type Foo }` names Foo; the `type` marker is part of the binding's bytes.
        if text == "type"
            && code
                .get(at + 1)
                .is_some_and(|next| next.kind == TokenKind::Identifier)
        {
            at += 1;
        }
        let name = code[at];
        at += 1;
        // `y as z` binds z; the file refers to the alias, so that is the name to count.
        let alias = code
            .get(at)
            .filter(|token| token.text(source) == "as")
            .and_then(|_| code.get(at + 1))
            .filter(|token| token.kind == TokenKind::Identifier);
        let (local, end) = match alias {
            Some(alias) => {
                at += 2;
                (alias.text(source).to_owned(), alias.end)
            }
            None => (name.text(source).to_owned(), name.end),
        };
        bindings.push(Binding { local, start, end });
    }
    (bindings, at)
}

/// Locates every static `import` statement in the file.
///
/// `import(...)` and `import.meta` are skipped: they are expressions, not statements, and have no
/// bindings to remove.
fn imports(source: &str, tokens: &[Token]) -> Vec<Import> {
    let code: Vec<&Token> = tokens.iter().filter(|token| is_code(token)).collect();
    let mut found = Vec::new();
    let mut at = 0;
    while at < code.len() {
        if code[at].kind != TokenKind::Identifier || code[at].text(source) != "import" {
            at += 1;
            continue;
        }
        let next = code.get(at + 1).map(|token| token.text(source));
        if next == Some("(") || next == Some(".") {
            at += 1;
            continue;
        }
        let start = code[at].start;
        let mut statement = Import {
            start,
            end: code[at].end,
            named: Vec::new(),
            braces: None,
            unnamed: Vec::new(),
        };
        at += 1;
        // Everything up to the module specifier belongs to this statement's bindings.
        while at < code.len() {
            let token = code[at];
            let text = token.text(source);
            if text == "{" {
                let open = token.start;
                let (bindings, after) = read_group(source, &code, at + 1);
                let close = code
                    .get(after.saturating_sub(1))
                    .map_or(token.end, |token| token.end);
                statement.named = bindings;
                statement.braces = Some((open, close));
                at = after;
                continue;
            }
            if text == "from" || text == ";" {
                break;
            }
            if token.kind == TokenKind::Identifier && text != "type" && text != "as" {
                statement.unnamed.push(text.to_owned());
            }
            if text == "*" {
                statement.unnamed.push("*".to_owned());
            }
            at += 1;
        }
        // The statement ends at its `;`, or at the module specifier when there is none.
        while at < code.len() {
            let text = code[at].text(source);
            statement.end = code[at].end;
            at += 1;
            if text == ";" {
                break;
            }
            if code.get(at).is_some_and(|token| {
                token.text(source) == "import" || token.line > code[at - 1].line
            }) {
                break;
            }
        }
        found.push(statement);
    }
    found
}

/// How many times each identifier appears in code outside every import statement.
fn uses_outside_imports(source: &str, tokens: &[Token], statements: &[Import]) -> Vec<String> {
    tokens
        .iter()
        .filter(|token| is_code(token) && token.kind == TokenKind::Identifier)
        .filter(|token| {
            !statements
                .iter()
                .any(|statement| token.start >= statement.start && token.end <= statement.end)
        })
        .map(|token| token.text(source).to_owned())
        .collect()
}

/// The (line, byte column) of a byte offset, both 1-based.
fn position_of(source: &str, offset: usize) -> (u32, u32) {
    let before = &source[..offset.min(source.len())];
    let line = u32::try_from(before.matches('\n').count() + 1).unwrap_or(u32::MAX);
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = u32::try_from(offset - line_start + 1).unwrap_or(u32::MAX);
    (line, column)
}

/// Adds one deletion to the plan, or nothing when its position cannot be expressed exactly.
fn delete(builder: PlanBuilder, source: &str, range: (usize, usize)) -> PlanBuilder {
    let (start_line, start_column) = position_of(source, range.0);
    let (end_line, end_column) = position_of(source, range.1);
    let (Ok(start_char), Ok(end_char)) = (
        utf16_offset(source, start_line, start_column),
        utf16_offset(source, end_line, end_column),
    ) else {
        return builder;
    };
    let Some(before) = source.get(range.0..range.1) else {
        return builder;
    };
    builder.edit(
        start_line,
        start_char,
        end_line,
        end_char,
        before,
        "",
        "LEXICAL_EXACT",
    )
}

/// The bytes to delete for a run of consecutive unused bindings.
///
/// A run is taken whole rather than one binding at a time, because the separating comma belongs
/// to whichever neighbour survives — and two per-binding ranges that each claimed a comma would
/// overlap, which the applier rejects.
fn runs(named: &[Binding], unused: &[bool]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut at = 0;
    while at < named.len() {
        if !unused[at] {
            at += 1;
            continue;
        }
        let mut last = at;
        while last + 1 < named.len() && unused[last + 1] {
            last += 1;
        }
        if last + 1 < named.len() {
            // A binding survives after the run: take the run and the comma that follows it.
            ranges.push((named[at].start, named[last + 1].start));
        } else if at > 0 {
            // The run ends the group: take the comma that precedes it.
            ranges.push((named[at - 1].end, named[last].end));
        }
        at = last + 1;
    }
    ranges
}

/// Extends a statement's range to swallow its own line when nothing else shares it.
fn whole_lines(source: &str, statement: &Import) -> (usize, usize) {
    let line_start = source[..statement.start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let start = if source[line_start..statement.start]
        .chars()
        .all(char::is_whitespace)
    {
        line_start
    } else {
        statement.start
    };
    let rest = &source[statement.end..];
    let trailing = rest
        .find('\n')
        .filter(|index| rest[..*index].chars().all(char::is_whitespace))
        .map_or(0, |index| index + 1);
    (start, statement.end + trailing)
}

/// Plans one statement's removals and records which bindings they took.
fn plan_statement(
    mut builder: PlanBuilder,
    source: &str,
    statement: &Import,
    used: &[String],
    removed: &mut Vec<String>,
) -> PlanBuilder {
    let unused: Vec<bool> = statement
        .named
        .iter()
        .map(|binding| !used.contains(&binding.local))
        .collect();
    if !statement.named.is_empty() && unused.iter().all(|flag| *flag) {
        // Nothing named survives. With no default beside it the statement goes; with one, only
        // the braces group does, and the comma before it belongs to the group.
        let range = if statement.unnamed.is_empty() {
            whole_lines(source, statement)
        } else if let Some((open, close)) = statement.braces {
            // Bounded to this statement: a file-wide search would find the comma of an earlier
            // import and delete everything between the two.
            let comma = source[statement.start..open]
                .rfind(',')
                .map_or(open, |index| statement.start + index);
            (comma, close)
        } else {
            return builder;
        };
        removed.extend(statement.named.iter().map(|binding| binding.local.clone()));
        return delete(builder, source, range);
    }
    for range in runs(&statement.named, &unused) {
        builder = delete(builder, source, range);
    }
    removed.extend(
        statement
            .named
            .iter()
            .zip(&unused)
            .filter(|(_, flag)| **flag)
            .map(|(binding, _)| binding.local.clone()),
    );
    builder
}

pub(super) fn organize_imports(state: &RepositoryState, arguments: &Value) -> Value {
    let Some(path) = arguments.get("file").and_then(Value::as_str) else {
        return super::invalid_args("organize_imports", &["file"]);
    };
    let Some(source) = read_source(state.root(), path) else {
        return json!({
            "status": "SOURCE_UNAVAILABLE",
            "reason": format!("{path} could not be read from the active repository"),
        });
    };
    if source.len() > MAX_BYTES {
        return json!({
            "status": "FILE_TOO_LARGE",
            "reason": format!("{path} is {} bytes; the limit is {MAX_BYTES}", source.len()),
        });
    }
    let Some(language) = path
        .rsplit_once('.')
        .and_then(|(_, ext)| Language::from_extension(ext))
    else {
        return json!({
            "status": "UNPROVEN",
            "reason": format!("{path} has no recognised language, so its imports were not read"),
        });
    };

    let tokens = tokenize(&source, language);
    let statements = imports(&source, &tokens);
    let candidates: Vec<&Binding> = statements
        .iter()
        .flat_map(|statement| statement.named.iter())
        .collect();

    if !matches!(language, Language::JavaScript | Language::TypeScript) {
        return unproven(path, language, &candidates);
    }
    if statements.is_empty() {
        return json!({
            "status": "NO_UNUSED_IMPORTS",
            "file": path,
            "reason": "the file declares no static imports",
        });
    }

    let used = uses_outside_imports(&source, &tokens, &statements);
    let mut builder = PlanBuilder::new("organize_imports").file(path, &sha256_of(&source));
    let mut removed = Vec::new();
    let reported = statements
        .iter()
        .flat_map(|statement| statement.unnamed.iter().cloned())
        .map(|name| {
            json!({
                "binding": name,
                "kind": "UNCERTAIN",
                "reason": "a default or namespace import can be used by a transform rather than \
                           by name, so its occurrences prove nothing",
            })
        })
        .collect::<Vec<_>>();

    for statement in &statements {
        builder = plan_statement(builder, &source, statement, &used, &mut removed);
    }

    if removed.is_empty() {
        return json!({
            "status": "NO_UNUSED_IMPORTS",
            "file": path,
            "checkedBindings": candidates.len(),
            "uncertain": reported,
            "reason": "every named binding occurs somewhere else in the file",
        });
    }
    json!({
        "status": "PLANNED",
        "completeness": "PARTIAL",
        "file": path,
        "removed": removed,
        "uncertain": reported,
        "plan": builder.build(),
        "warnings": ["OCCURRENCE_COUNTED_WITHIN_FILE"],
        "next": "apply with apply_edit_plan (preview -> confirm). Sorting is left to the \
                 formatter; only provably-unused named bindings were removed.",
    })
}

/// The answer for a language where an unnamed occurrence count does not prove disuse.
fn unproven(path: &str, language: Language, candidates: &[&Binding]) -> Value {
    json!({
        "status": "UNPROVEN",
        "file": path,
        "language": format!("{language:?}"),
        "candidates": candidates.iter().map(|binding| binding.local.clone()).collect::<Vec<_>>(),
        "reason": "outside JavaScript and TypeScript an import can be used without its name \
                   appearing - a Rust trait through one of its methods, a Python re-export \
                   through the module's public API - so an occurrence count cannot prove a \
                   binding is unused and nothing was planned",
    })
}
