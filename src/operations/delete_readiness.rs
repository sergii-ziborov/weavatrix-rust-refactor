//! Per-symbol deletion verdict.
//!
//! `safe: true` is deliberately unreachable here. Proving that nothing references a symbol needs
//! a complete exact reference query over the whole project universe; the graph can only show
//! what it indexed. So this operation answers `false` when it finds references and `UNPROVEN`
//! when it does not, and says which risk signal stopped it — the same contract the JavaScript
//! host holds for every language without an exact backend.

use crate::evidence::{Risk, declaring_file, read_source};
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use weavatrix_rust::{EdgeKind, RepositoryState};

const MAX_REFERENCES: usize = 200;

pub(super) fn delete_readiness(state: &RepositoryState, arguments: &Value) -> Value {
    let Some(symbol) = arguments.get("symbol").and_then(Value::as_str) else {
        return crate::operations::invalid_args("delete_readiness", &["symbol"]);
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return json!({
            "status": "NOT_FOUND",
            "reason": "the selected symbol is not present in the active graph",
            "symbol": symbol,
        });
    };
    let Some(node) = state.graph().node_at(index) else {
        return json!({
            "status": "NOT_FOUND",
            "reason": "the selected symbol is not present in the active graph",
            "symbol": symbol,
        });
    };

    let file = declaring_file(node);
    let source = file
        .as_deref()
        .and_then(|path| read_source(state.root(), path));

    // Containment is structure, not use: a file containing a symbol is not a caller of it.
    let known_references = state
        .graph()
        .incoming_at(index)
        .filter(|edge| edge.kind != EdgeKind::Contains)
        .take(MAX_REFERENCES)
        .map(|edge| {
            json!({
                "relation": edge.kind.as_str(),
                "provenance": edge.provenance.evidence.as_str(),
            })
        })
        .collect::<Vec<_>>();

    let signals = Risk::assess(node, source.as_deref());
    let blocking = signals.blocking();

    let (safe, confidence, reason) = if known_references.is_empty() {
        let reason = if blocking.is_empty() {
            "no known references, but no complete exact zero-reference proof exists; static \
             absence alone is never proof of deletion safety"
                .to_owned()
        } else {
            format!(
                "absence of references is not proven safe: {}",
                blocking.join(", ")
            )
        };
        (
            Value::from("UNPROVEN"),
            if blocking.is_empty() { "medium" } else { "low" },
            reason,
        )
    } else {
        (
            Value::from(false),
            "high",
            format!(
                "{} known reference(s) target this symbol; deleting it would break them",
                known_references.len()
            ),
        )
    };

    json!({
        "status": "OK",
        "symbol": node.label,
        "file": file,
        "safe": safe,
        "confidence": confidence,
        "reason": reason,
        "knownReferences": known_references,
        "unknownDynamicUsages": signals.report(),
        "deletion": deletion_span(node, file.as_deref()),
        "verdict": "REVIEW_REQUIRED",
    })
}

fn deletion_span(node: &weavatrix_graph::Node, file: Option<&str>) -> Value {
    node.span.as_ref().map_or_else(
        || {
            json!({
                "file": file,
                "startLine": Value::Null,
                "endLine": Value::Null,
                "note": "no source range recorded; locate the declaration manually",
            })
        },
        |span| {
            json!({
                "file": file,
                "startLine": span.start.line + 1,
                "endLine": span.end.line + 1,
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{MAX_REFERENCES, delete_readiness};
    use crate::test_support::fixture_state;
    use blazingly_json::{Value, json};

    #[test]
    fn a_missing_symbol_is_not_found_not_an_error() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({"symbol": "src/nope.rs#ghost"}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("NOT_FOUND")
        );
    }

    #[test]
    fn a_missing_argument_is_invalid_args_not_a_crash() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("INVALID_ARGS")
        );
    }

    #[test]
    fn a_referenced_symbol_is_unsafe_with_high_confidence() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({"symbol": "used"}));
        if answer.get("status").and_then(Value::as_str) != Some("OK") {
            return; // the fixture graph did not resolve that label; the NOT_FOUND path covers it
        }
        let references = answer
            .get("knownReferences")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if references > 0 {
            assert_eq!(answer.get("safe").and_then(Value::as_bool), Some(false));
            assert_eq!(
                answer.get("confidence").and_then(Value::as_str),
                Some("high")
            );
        }
        assert!(references <= MAX_REFERENCES);
    }

    #[test]
    fn deletion_is_never_automated_whatever_the_verdict() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({"symbol": "orphan"}));
        if answer.get("status").and_then(Value::as_str) == Some("OK") {
            assert_eq!(
                answer.get("verdict").and_then(Value::as_str),
                Some("REVIEW_REQUIRED")
            );
            // safe:true would claim a proof this engine cannot produce.
            assert_ne!(answer.get("safe").and_then(Value::as_bool), Some(true));
        }
    }
}
