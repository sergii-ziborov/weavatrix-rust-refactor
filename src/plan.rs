//! Building a `weavatrix.edit-plan.v1` envelope.
//!
//! Every planner emits the same shape, so it is built in one place. The alternative — each
//! engine assembling its own JSON — is how one of them ends up omitting a hash or spelling a
//! field differently, and the applier would refuse it long after the mistake was made.

use blazingly_json::{Value, json};
use std::collections::BTreeMap;

/// The hash the applier will re-check before writing.
#[must_use]
pub fn sha256_of(text: &str) -> String {
    weavatrix_worktree::Sha256Hash::compute(text.as_bytes()).to_string()
}

/// Accumulates files and their edits into one envelope.
pub struct PlanBuilder {
    operation: String,
    files: Vec<(String, String, Vec<Value>)>,
    index: BTreeMap<String, usize>,
}

impl PlanBuilder {
    /// Starts a plan for one operation.
    #[must_use]
    pub fn new(operation: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            files: Vec::new(),
            index: BTreeMap::new(),
        }
    }

    /// Opens or reuses a file entry. Edits added after this land on it.
    ///
    /// Reusing keeps one entry per path: two entries for the same file would each carry the
    /// same "before" hash and the second would be applied to bytes the first already changed.
    #[must_use]
    pub fn file(mut self, path: &str, sha256: &str) -> Self {
        if !self.index.contains_key(path) {
            self.index.insert(path.to_owned(), self.files.len());
            self.files
                .push((path.to_owned(), sha256.to_owned(), Vec::new()));
        }
        self
    }

    /// Adds one byte-exact edit to the file opened last.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn edit(
        mut self,
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
        before: impl Into<String>,
        after: impl Into<String>,
        provenance: &str,
    ) -> Self {
        if let Some((_, _, edits)) = self.files.last_mut() {
            edits.push(json!({
                "startLine": start_line,
                "startChar": start_char,
                "endLine": end_line,
                "endChar": end_char,
                "before": before.into(),
                "after": after.into(),
                "provenance": provenance,
            }));
        }
        self
    }

    /// Whether anything would actually be written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|(_, _, edits)| edits.is_empty())
    }

    /// Finishes the envelope.
    #[must_use]
    pub fn build(self) -> Value {
        json!({
            "schemaVersion": "weavatrix.edit-plan.v1",
            "operation": self.operation,
            "files": self.files.into_iter()
                .filter(|(_, _, edits)| !edits.is_empty())
                .map(|(path, sha256, edits)| json!({
                    "path": path,
                    "sha256": sha256,
                    "edits": edits,
                }))
                .collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{PlanBuilder, sha256_of};
    use blazingly_json::Value;

    #[test]
    fn the_envelope_carries_the_frozen_schema_version() {
        let plan = PlanBuilder::new("edit_symbol")
            .file("a.rs", &sha256_of("x"))
            .edit(1, 0, 1, 1, "x", "y", "EXTRACTED")
            .build();
        assert_eq!(
            plan.get("schemaVersion").and_then(Value::as_str),
            Some("weavatrix.edit-plan.v1")
        );
    }

    #[test]
    fn one_path_gets_one_file_entry_however_many_edits() {
        let plan = PlanBuilder::new("bulk_replace")
            .file("a.rs", &sha256_of("x"))
            .edit(1, 0, 1, 1, "x", "y", "LEXICAL_EXACT")
            .file("a.rs", &sha256_of("x"))
            .edit(2, 0, 2, 1, "x", "y", "LEXICAL_EXACT")
            .build();
        let files = plan.get("files").and_then(Value::as_array).expect("files");
        assert_eq!(
            files.len(),
            1,
            "a second entry would apply to already-edited bytes"
        );
        assert_eq!(
            files[0]
                .get("edits")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn a_file_with_no_edits_is_dropped_rather_than_shipped_empty() {
        let plan = PlanBuilder::new("bulk_replace")
            .file("untouched.rs", &sha256_of("x"))
            .build();
        assert_eq!(
            plan.get("files").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn emptiness_is_visible_before_building() {
        let empty = PlanBuilder::new("bulk_replace").file("a.rs", "hash");
        assert!(empty.is_empty());
        let filled = PlanBuilder::new("bulk_replace").file("a.rs", "hash").edit(
            1,
            0,
            1,
            1,
            "x",
            "y",
            "LEXICAL_EXACT",
        );
        assert!(!filled.is_empty());
    }

    #[test]
    fn the_hash_is_the_one_the_applier_recomputes() {
        // Same input, same digest as weavatrix-worktree computes when it re-reads the file.
        let text = "pub fn one() {}\n";
        assert_eq!(
            sha256_of(text),
            weavatrix_worktree::Sha256Hash::compute(text.as_bytes()).to_string()
        );
    }
}
