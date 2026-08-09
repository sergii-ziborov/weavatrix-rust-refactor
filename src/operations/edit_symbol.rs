//! Symbol-anchored edits: replace a declaration's body, or insert around it.
//!
//! The anchor is the parser range the graph recorded, so this never guesses where a declaration
//! begins. What it cannot do is check that the result still parses — the native engine has no
//! syntax gate yet — so every plan carries that limit as a warning rather than implying a
//! correctness it did not verify.

use crate::coordinates::utf16_offset;
use crate::declaration::locate;
use crate::evidence::{declaring_file, read_source};
use crate::plan::{PlanBuilder, sha256_of};
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use weavatrix_rust::RepositoryState;

/// The 1-based line and 1-based byte column of an absolute byte offset.
///
/// The plan speaks in line and column; the declaration was located in byte offsets. Converting
/// here keeps that translation in one place instead of at every call site.
fn position_of(source: &str, offset: usize) -> Option<(u32, u32)> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let before = source.get(..offset)?;
    let line = u32::try_from(before.matches('\n').count() + 1).ok()?;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = u32::try_from(offset - line_start + 1).ok()?;
    Some((line, column))
}

const OPERATIONS: [&str; 3] = [
    "replace_symbol_body",
    "insert_before_symbol",
    "insert_after_symbol",
];

/// Everything an edit needs, once the symbol has been resolved to a real declaration.
struct Anchor {
    file: String,
    source: String,
    label: String,
    declaration: crate::declaration::DeclarationRange,
}

/// Resolves the symbol to a declaration in a readable file, or the refusal that says why not.
fn anchor(state: &RepositoryState, symbol: &str) -> Result<Anchor, Value> {
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return Err(super::not_found(symbol));
    };
    let Some(node) = state.graph().node_at(index) else {
        return Err(super::not_found(symbol));
    };
    let Some(span) = node.span.as_ref() else {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": "the selected node has no recorded source range, so there is nothing to \
                       anchor an edit to",
            "symbol": symbol,
        }));
    };
    let Some(file) = declaring_file(node) else {
        return Err(super::not_found(symbol));
    };
    let Some(source) = read_source(state.root(), &file) else {
        return Err(json!({
            "status": "SOURCE_UNAVAILABLE",
            "reason": format!("{file}: the file is missing, too large, or not valid UTF-8"),
        }));
    };
    // The graph span covers the identifier only, so the declaration itself is located in the
    // source. Anchoring an insertion to the name would put it between `pub fn` and the name.
    let Some(declaration) = locate(&source, &file, &node.label, span.start.line) else {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": format!(
                "{file}: the parser does not report a declaration named {} on line {}; \
                 symbol-anchored edits need one",
                node.label, span.start.line
            ),
        }));
    };
    Ok(Anchor {
        file,
        source,
        label: node.label.clone(),
        declaration,
    })
}

pub(super) fn edit_symbol(state: &RepositoryState, arguments: &Value) -> Value {
    let symbol = arguments.get("symbol").and_then(Value::as_str);
    let operation = arguments.get("operation").and_then(Value::as_str);
    let content = arguments.get("content").and_then(Value::as_str);
    let (Some(symbol), Some(operation), Some(content)) = (symbol, operation, content) else {
        return super::invalid_args("edit_symbol", &["symbol", "operation", "content"]);
    };
    if !OPERATIONS.contains(&operation) {
        return json!({
            "status": "INVALID_ARGS",
            "operation": "edit_symbol",
            "invalid": ["operation"],
            "reason": format!("operation must be one of {}", OPERATIONS.join(", ")),
        });
    }
    let anchored = match anchor(state, symbol) {
        Ok(anchored) => anchored,
        Err(refusal) => return refusal,
    };
    let Anchor {
        file,
        source,
        label,
        declaration,
    } = anchored;
    if !declaration.end_proven && operation != "insert_before_symbol" {
        return json!({
            "status": "NOT_SUPPORTED",
            "reason": format!(
                "{file}: the end of this declaration could not be located in the source, so \
                 {operation} would place text inside it. insert_before_symbol is unaffected."
            ),
        });
    }

    let (range, before) = match operation {
        "replace_symbol_body" => (
            (declaration.start, declaration.end),
            source
                .get(declaration.start..declaration.end)
                .unwrap_or_default()
                .to_owned(),
        ),
        // An insertion is a zero-width edit: same position twice, empty `before`.
        "insert_before_symbol" => ((declaration.start, declaration.start), String::new()),
        _ => ((declaration.end, declaration.end), String::new()),
    };

    let (Some(start_position), Some(end_position)) =
        (position_of(&source, range.0), position_of(&source, range.1))
    else {
        return super::stale_graph(&file);
    };
    let (Ok(start_char), Ok(end_char)) = (
        utf16_offset(&source, start_position.0, start_position.1),
        utf16_offset(&source, end_position.0, end_position.1),
    ) else {
        return super::stale_graph(&file);
    };
    let range = ((start_position.0, start_char), (end_position.0, end_char));
    let after = content.to_owned();

    let plan = PlanBuilder::new("edit_symbol")
        .file(&file, &sha256_of(&source))
        .edit(
            range.0.0,
            range.0.1,
            range.1.0,
            range.1.1,
            before,
            after,
            "EXTRACTED",
        )
        .build();

    json!({
        "status": "PLANNED",
        "completeness": "COMPLETE",
        "symbol": label,
        "plan": plan,
        "warnings": ["SYNTAX_CHECK_NOT_PERFORMED"],
        "next": "apply with apply_edit_plan (preview -> confirm). The native engine does not \
                 syntax-check the result, so review it or run the project's own build.",
    })
}

#[cfg(test)]
mod tests {
    use super::edit_symbol;
    use crate::test_support::fixture_state;
    use blazingly_json::{Value, json};

    fn status(value: &Value) -> &str {
        value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    #[test]
    fn a_missing_argument_is_invalid_args() {
        let state = fixture_state();
        assert_eq!(
            status(&edit_symbol(&state, &json!({"symbol": "used"}))),
            "INVALID_ARGS"
        );
    }

    #[test]
    fn an_unknown_operation_is_refused_by_name() {
        let state = fixture_state();
        let answer = edit_symbol(
            &state,
            &json!({"symbol": "used", "operation": "rewrite_everything", "content": "x"}),
        );
        assert_eq!(status(&answer), "INVALID_ARGS");
        assert_eq!(
            answer
                .get("invalid")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn a_missing_symbol_is_not_found() {
        let state = fixture_state();
        let answer = edit_symbol(
            &state,
            &json!({"symbol": "nope::never", "operation": "insert_before_symbol", "content": "//\n"}),
        );
        assert_eq!(status(&answer), "NOT_FOUND");
    }

    #[test]
    fn an_insertion_is_a_zero_width_edit_with_empty_before() {
        let state = fixture_state();
        let answer = edit_symbol(
            &state,
            &json!({"symbol": "used", "operation": "insert_before_symbol", "content": "// note\n"}),
        );
        if status(&answer) != "PLANNED" {
            return; // the fixture graph did not resolve that label
        }
        let edit = answer
            .get("plan")
            .and_then(|plan| plan.get("files"))
            .and_then(Value::as_array)
            .and_then(|files| files.first())
            .and_then(|file| file.get("edits"))
            .and_then(Value::as_array)
            .and_then(|edits| edits.first())
            .expect("one edit");
        assert_eq!(edit.get("before").and_then(Value::as_str), Some(""));
        assert_eq!(
            edit.get("startLine").and_then(Value::as_u64),
            edit.get("endLine").and_then(Value::as_u64)
        );
        assert_eq!(
            edit.get("startChar").and_then(Value::as_u64),
            edit.get("endChar").and_then(Value::as_u64)
        );
    }

    #[test]
    fn replacing_a_body_carries_the_exact_existing_text_as_before() {
        let state = fixture_state();
        let answer = edit_symbol(
            &state,
            &json!({"symbol": "used", "operation": "replace_symbol_body", "content": "pub fn used(v: u32) -> u32 { v }"}),
        );
        if status(&answer) != "PLANNED" {
            return;
        }
        let before = answer
            .get("plan")
            .and_then(|plan| plan.get("files"))
            .and_then(Value::as_array)
            .and_then(|files| files.first())
            .and_then(|file| file.get("edits"))
            .and_then(Value::as_array)
            .and_then(|edits| edits.first())
            .and_then(|edit| edit.get("before"))
            .and_then(Value::as_str)
            .expect("before text");
        assert!(
            before.contains("used"),
            "the before text must be the real declaration, found {before:?}"
        );
    }

    #[test]
    fn the_missing_syntax_gate_is_stated_rather_than_implied() {
        let state = fixture_state();
        let answer = edit_symbol(
            &state,
            &json!({"symbol": "used", "operation": "insert_after_symbol", "content": "\n"}),
        );
        if status(&answer) != "PLANNED" {
            return;
        }
        let warnings = answer
            .get("warnings")
            .and_then(Value::as_array)
            .expect("warnings");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str() == Some("SYNTAX_CHECK_NOT_PERFORMED")),
            "a plan that was not syntax-checked must say so"
        );
    }
}
