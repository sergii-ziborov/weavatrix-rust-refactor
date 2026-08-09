//! Reading a `weavatrix.edit-plan.v1` envelope into the typed plan the worktree applies.
//!
//! The mapping is written out rather than derived, because every field here is load-bearing and
//! a silent default would be a hole in the proof: a missing `sha256` that became an empty string
//! would turn a stale-file check into a no-op, and an unknown provenance that fell back to a
//! permissive value would let an inferred edit through the gate that exists to stop it.

use blazingly_json::Value;
use weavatrix_refactor_plan::{EditPlan, FileEdit, Provenance, TextEdit};

/// Why an envelope could not be read, in the vocabulary the contract already uses.
pub struct EnvelopeError {
    pub code: &'static str,
    pub reason: String,
}

impl EnvelopeError {
    fn invalid(reason: impl Into<String>) -> Self {
        Self {
            code: "INVALID_PLAN",
            reason: reason.into(),
        }
    }
}

/// Provenance classes this package will apply.
///
/// The applicable set is `weavatrix-edit`'s to define, so this asks rather than restates it —
/// a second copy of that list here would be one more place for `INFERRED` to leak through the
/// gate that exists to stop it.
fn provenance(value: &str) -> Option<Provenance> {
    let declared = Provenance::new(value);
    declared.is_applicable().then_some(declared)
}

fn required_str(value: &Value, key: &str, context: &str) -> Result<String, EnvelopeError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| EnvelopeError::invalid(format!("{context}: {key} must be a string")))
}

fn required_u32(value: &Value, key: &str, context: &str) -> Result<u32, EnvelopeError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| {
            EnvelopeError::invalid(format!("{context}: {key} must be a non-negative integer"))
        })
}

fn text_edit(value: &Value, context: &str) -> Result<TextEdit, EnvelopeError> {
    let declared = required_str(value, "provenance", context)?;
    let Some(provenance) = provenance(&declared) else {
        return Err(EnvelopeError {
            code: "INVALID_PLAN",
            reason: format!(
                "{context}: provenance {declared} is not applyable; only EXACT_LSP, RESOLVED, \
                 EXTRACTED and LEXICAL_EXACT edits are ever written"
            ),
        });
    };
    Ok(TextEdit {
        start_line: required_u32(value, "startLine", context)?,
        start_char: required_u32(value, "startChar", context)?,
        end_line: required_u32(value, "endLine", context)?,
        end_char: required_u32(value, "endChar", context)?,
        before: required_str(value, "before", context)?,
        after: required_str(value, "after", context)?,
        provenance,
        extensions: std::collections::BTreeMap::new(),
    })
}

/// Reads an envelope, refusing anything the applier could not prove.
///
/// # Errors
///
/// Returns `INVALID_PLAN` with the field at fault. Nothing is read from disk here, so a refusal
/// costs nothing and never half-touches the working tree.
pub fn read_envelope(plan: &Value) -> Result<EditPlan, EnvelopeError> {
    let schema = required_str(plan, "schemaVersion", "plan")?;
    if schema != "weavatrix.edit-plan.v1" {
        return Err(EnvelopeError::invalid(format!(
            "plan: schemaVersion must be weavatrix.edit-plan.v1, found {schema}"
        )));
    }
    let operation = required_str(plan, "operation", "plan")?;
    let files = plan
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| EnvelopeError::invalid("plan: files must be an array"))?;
    if files.is_empty() {
        return Err(EnvelopeError::invalid("plan: files must not be empty"));
    }
    let mut parsed = Vec::with_capacity(files.len());
    for file in files {
        let path = required_str(file, "path", "plan file")?;
        let sha256 = required_str(file, "sha256", &format!("plan file {path}"))?;
        let edits = file.get("edits").and_then(Value::as_array).ok_or_else(|| {
            EnvelopeError::invalid(format!("plan file {path}: edits must be an array"))
        })?;
        if edits.is_empty() {
            return Err(EnvelopeError::invalid(format!(
                "plan file {path}: edits must not be empty"
            )));
        }
        let mut typed = Vec::with_capacity(edits.len());
        for edit in edits {
            typed.push(text_edit(edit, &format!("plan file {path}"))?);
        }
        parsed.push(FileEdit::new(path, sha256, typed));
    }
    Ok(EditPlan::new(operation, parsed))
}

#[cfg(test)]
mod tests {
    use super::read_envelope;
    use blazingly_json::json;

    fn envelope(provenance: &str) -> blazingly_json::Value {
        json!({
            "schemaVersion": "weavatrix.edit-plan.v1",
            "operation": "rename_symbol",
            "files": [{
                "path": "src/a.rs",
                "sha256": "0".repeat(64),
                "edits": [{
                    "startLine": 1, "startChar": 4, "endLine": 1, "endChar": 7,
                    "before": "one", "after": "two", "provenance": provenance,
                }],
            }],
        })
    }

    #[test]
    fn a_proven_envelope_reads() {
        for proven in ["EXACT_LSP", "RESOLVED", "EXTRACTED", "LEXICAL_EXACT"] {
            let plan = read_envelope(&envelope(proven)).map_err(|error| error.reason);
            assert!(plan.is_ok(), "{proven} must be applyable: {plan:?}");
        }
    }

    #[test]
    fn an_inferred_edit_is_refused() {
        let error = read_envelope(&envelope("INFERRED")).unwrap_err();
        assert_eq!(error.code, "INVALID_PLAN");
        assert!(error.reason.contains("not applyable"));
    }

    #[test]
    fn a_missing_hash_is_refused_rather_than_defaulted() {
        let mut plan = envelope("EXACT_LSP");
        if let Some(file) = plan
            .get_mut("files")
            .and_then(|files| files.as_array_mut())
            .and_then(|files| files.first_mut())
            .and_then(|file| file.as_object_mut())
        {
            file.remove("sha256");
        }
        let error = read_envelope(&plan).unwrap_err();
        assert!(error.reason.contains("sha256"));
    }

    #[test]
    fn the_wrong_schema_is_refused() {
        let mut plan = envelope("EXACT_LSP");
        if let Some(object) = plan.as_object_mut() {
            object.insert("schemaVersion".to_owned(), "weavatrix.edit-plan.v2".into());
        }
        assert!(read_envelope(&plan).is_err());
    }

    #[test]
    fn an_empty_plan_is_refused() {
        let mut plan = envelope("EXACT_LSP");
        if let Some(object) = plan.as_object_mut() {
            object.insert("files".to_owned(), blazingly_json::Value::Array(vec![]));
        }
        assert!(read_envelope(&plan).is_err());
    }
}
