//! Risk signals that make an absence of references unprovable.
//!
//! None of these say a symbol *is* used. They say the graph cannot see whether it is — dynamic
//! dispatch, reflection, a framework entry point, a public surface with consumers outside this
//! repository. Reporting them by name is the difference between "unproven" and a bare "no".

use std::fs;
use std::path::Path;
use weavatrix_graph::AttributeValue;
use weavatrix_rust::{Node, NodeKind};

/// Bytes of a declaring file this scan will read. A declaration that lives in a file larger than
/// this is a review problem of its own; refusing to read it is safer than a partial scan that
/// silently misses the pattern it was looking for.
const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;

const DYNAMIC_PATTERNS: [&str; 6] = [
    "require(",
    "import(",
    "eval(",
    "Function(",
    "libloading",
    "dlopen",
];

const REFLECTION_PATTERNS: [&str; 6] = [
    "Reflect.",
    "getattr(",
    "__getattr__",
    "std::any::",
    "TypeId::of",
    "Any>::downcast",
];

/// One named signal and what the scan found.
pub struct RiskSignal {
    pub signal: &'static str,
    pub status: &'static str,
    blocking: bool,
}

/// The risk assessment for one declaration.
pub struct Risk(Vec<RiskSignal>);

impl Risk {
    /// Assesses a declaration against its own source, when the source could be read.
    #[must_use]
    pub fn assess(node: &Node, source: Option<&str>) -> Self {
        let public = matches!(node.kind, NodeKind::File)
            || matches!(
                node.attributes.get("exported"),
                Some(AttributeValue::Bool(true))
            );
        let unread = source.is_none();
        let contains = |patterns: &[&str]| {
            source.is_some_and(|text| patterns.iter().any(|pattern| text.contains(pattern)))
        };
        Self(vec![
            RiskSignal {
                signal: "EXTERNAL_CONSUMERS",
                status: if public {
                    "NOT_POSSIBLE_FROM_REPOSITORY_GRAPH"
                } else {
                    "NOT_APPLICABLE_INTERNAL"
                },
                blocking: public,
            },
            RiskSignal {
                signal: "DYNAMIC_LOADING",
                status: status_for(unread, contains(&DYNAMIC_PATTERNS)),
                blocking: unread || contains(&DYNAMIC_PATTERNS),
            },
            RiskSignal {
                signal: "REFLECTION",
                status: status_for(unread, contains(&REFLECTION_PATTERNS)),
                blocking: unread || contains(&REFLECTION_PATTERNS),
            },
            RiskSignal {
                // The native engine has no exact reference backend yet, so it may never claim the
                // zero-reference proof that would make a deletion safe.
                signal: "EXACT_REFERENCES",
                status: "NOT_SUPPORTED_BY_NATIVE_ENGINE",
                blocking: true,
            },
        ])
    }

    /// Signal names that stop this from being a proof.
    #[must_use]
    pub fn blocking(&self) -> Vec<&'static str> {
        self.0
            .iter()
            .filter(|signal| signal.blocking)
            .map(|signal| signal.signal)
            .collect()
    }

    /// The full assessment, reported whatever the verdict.
    #[must_use]
    pub fn report(&self) -> blazingly_json::Value {
        blazingly_json::Value::Array(
            self.0
                .iter()
                .map(|signal| blazingly_json::json!({"signal": signal.signal, "status": signal.status}))
                .collect(),
        )
    }
}

const fn status_for(unread: bool, present: bool) -> &'static str {
    if unread {
        "NOT_CHECKED_SOURCE_UNAVAILABLE"
    } else if present {
        "PRESENT"
    } else {
        "NOT_OBSERVED_IN_DECLARING_FILE"
    }
}

/// The repository-relative file a node was declared in.
#[must_use]
pub fn declaring_file(node: &Node) -> Option<String> {
    node.span.as_ref().map(|span| span.file.clone())
}

/// Reads a declaring file, or `None` when it is missing, too large, or not valid UTF-8.
///
/// A caller that gets `None` must report the risk as unchecked rather than as absent.
#[must_use]
pub fn read_source(root: &Path, file: &str) -> Option<String> {
    let path = root.join(file);
    if fs::metadata(&path).ok()?.len() > MAX_SOURCE_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::{Risk, status_for};
    use weavatrix_rust::{Node, NodeKind};

    fn node() -> Node {
        Node::new("src/a.rs#one", "one", NodeKind::Function).expect("node")
    }

    #[test]
    fn the_native_engine_never_claims_an_exact_zero_reference_proof() {
        let risk = Risk::assess(&node(), Some("fn one() {}\n"));
        assert!(
            risk.blocking().contains(&"EXACT_REFERENCES"),
            "a missing exact backend must always block the proof"
        );
    }

    #[test]
    fn unreadable_source_is_unchecked_not_absent() {
        let risk = Risk::assess(&node(), None);
        assert!(risk.blocking().contains(&"DYNAMIC_LOADING"));
        assert!(risk.blocking().contains(&"REFLECTION"));
        assert_eq!(status_for(true, false), "NOT_CHECKED_SOURCE_UNAVAILABLE");
    }

    #[test]
    fn observed_dynamic_code_is_reported_present() {
        let risk = Risk::assess(&node(), Some("let m = require('./x');\n"));
        let report = risk.report();
        let dynamic = report
            .as_array()
            .and_then(|signals| {
                signals.iter().find(|signal| {
                    signal.get("signal").and_then(|value| value.as_str()) == Some("DYNAMIC_LOADING")
                })
            })
            .and_then(|signal| signal.get("status").and_then(|value| value.as_str()));
        assert_eq!(dynamic, Some("PRESENT"));
    }

    #[test]
    fn every_signal_is_reported_whatever_the_verdict() {
        let risk = Risk::assess(&node(), Some("fn one() {}\n"));
        assert_eq!(risk.report().as_array().map(Vec::len), Some(4));
    }
}
