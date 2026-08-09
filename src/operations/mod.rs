//! The refactor operation surface.
//!
//! Dispatch is deliberately total: every frozen tool has an arm, and an engine that has not
//! landed yet answers `NOT_SUPPORTED` with the reason. That is the shape the migration needs —
//! the host can boot and expose the real eleven tools from day one, and each engine is replaced
//! against the frozen contract instead of behind a flag that hides which half is live.

use crate::contract;
use blazingly_json::{Value, json};

/// A refactor operation, resolved from a tool name the contract declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    RenameSymbol,
    RenameRelatedSymbols,
    ApplyEditPlan,
    RollbackLastApply,
    ChangeSignature,
    EditSymbol,
    BulkReplace,
    OrganizeImports,
    MoveFile,
    MoveSymbol,
    DeleteReadiness,
}

impl Operation {
    /// Resolves a tool name, or `None` when the contract does not declare it.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "rename_symbol" => Some(Self::RenameSymbol),
            "rename_related_symbols" => Some(Self::RenameRelatedSymbols),
            "apply_edit_plan" => Some(Self::ApplyEditPlan),
            "rollback_last_apply" => Some(Self::RollbackLastApply),
            "change_signature" => Some(Self::ChangeSignature),
            "edit_symbol" => Some(Self::EditSymbol),
            "bulk_replace" => Some(Self::BulkReplace),
            "organize_imports" => Some(Self::OrganizeImports),
            "move_file" => Some(Self::MoveFile),
            "move_symbol" => Some(Self::MoveSymbol),
            "delete_readiness" => Some(Self::DeleteReadiness),
            _ => None,
        }
    }

    /// Whether the operation can write to the repository once every gate is satisfied.
    ///
    /// The plan producers are reads. Only these four ever touch a file, and only behind the
    /// environment gate plus a plan-bound single-use token.
    #[must_use]
    pub const fn writes(self) -> bool {
        matches!(
            self,
            Self::RenameSymbol
                | Self::RenameRelatedSymbols
                | Self::ApplyEditPlan
                | Self::RollbackLastApply
        )
    }

    #[must_use]
    const fn name(self) -> &'static str {
        match self {
            Self::RenameSymbol => "rename_symbol",
            Self::RenameRelatedSymbols => "rename_related_symbols",
            Self::ApplyEditPlan => "apply_edit_plan",
            Self::RollbackLastApply => "rollback_last_apply",
            Self::ChangeSignature => "change_signature",
            Self::EditSymbol => "edit_symbol",
            Self::BulkReplace => "bulk_replace",
            Self::OrganizeImports => "organize_imports",
            Self::MoveFile => "move_file",
            Self::MoveSymbol => "move_symbol",
            Self::DeleteReadiness => "delete_readiness",
        }
    }
}

/// The tool catalog, taken from the frozen contract.
#[must_use]
pub fn catalog() -> Value {
    contract::catalog_value().clone()
}

/// Names of every tool this crate exposes.
#[must_use]
pub fn catalog_names() -> Vec<String> {
    contract::tools()
        .iter()
        .map(|tool| tool.name.clone())
        .collect()
}

/// Calls one refactor operation.
///
/// # Errors
///
/// Returns an error only when `name` is not a tool the contract declares. Every other refusal
/// is a value carrying a contract status, because an agent branches on statuses and cannot
/// branch on a transport error.
pub fn call(name: &str, _arguments: &Value) -> Result<Value, String> {
    let Some(operation) = Operation::from_name(name) else {
        return Err(format!("unknown refactor operation: {name}"));
    };
    Ok(pending(operation))
}

/// The honest answer while an engine is still being ported.
///
/// `NOT_SUPPORTED` is a contract status, so a client that already handles the JavaScript host
/// handles this without a change; the reason says which implementation to use meanwhile.
fn pending(operation: Operation) -> Value {
    json!({
        "status": "NOT_SUPPORTED",
        "operation": operation.name(),
        "reason": format!(
            "{} has not been ported to the native engine yet; use weavatrix-refactor-js for this \
             operation until it lands",
            operation.name()
        ),
        "writes": operation.writes(),
        "contractVersion": crate::CONTRACT_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::{Operation, call, catalog, catalog_names};
    use crate::contract;

    #[test]
    fn every_contract_tool_resolves_to_an_operation() {
        for tool in contract::tools() {
            assert!(
                Operation::from_name(&tool.name).is_some(),
                "{} is in the contract with no operation arm",
                tool.name
            );
        }
    }

    #[test]
    fn no_operation_exists_outside_the_contract() {
        for name in catalog_names() {
            assert!(contract::declares(&name));
        }
        assert_eq!(catalog_names().len(), contract::tools().len());
    }

    #[test]
    fn exactly_four_operations_can_write() {
        let writers = contract::tools()
            .iter()
            .filter_map(|tool| Operation::from_name(&tool.name))
            .filter(|operation| operation.writes())
            .count();
        assert_eq!(writers, 4);
    }

    #[test]
    fn a_pending_engine_answers_with_a_contract_status_not_an_error() {
        let answer = call("rename_symbol", &blazingly_json::json!({})).expect("declared tool");
        let status = answer
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        assert!(
            contract::permits_state(status),
            "{status} is outside the contract"
        );
    }

    #[test]
    fn an_undeclared_tool_is_an_error_not_a_status() {
        assert!(call("reformat_universe", &blazingly_json::json!({})).is_err());
    }

    #[test]
    fn the_catalog_matches_the_contract() {
        assert_eq!(
            catalog().as_array().map(Vec::len),
            Some(contract::tools().len())
        );
    }
}
