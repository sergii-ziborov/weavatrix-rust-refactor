//! The refactor operation surface.
//!
//! Dispatch is deliberately total: every frozen tool has an arm, and an engine that has not
//! landed yet answers `NOT_SUPPORTED` with the reason. That is the shape the migration needs —
//! the host can boot and expose the real eleven tools from day one, and each engine is replaced
//! against the frozen contract instead of behind a flag that hides which half is live.

mod apply;
mod bulk_replace;
mod delete_readiness;
mod edit_symbol;
mod move_symbol;

use crate::contract;
use crate::token::TokenStore;
use blazingly_json::{Value, json};
use weavatrix_rust::RepositoryState;

/// One server's refactor surface: the confirmations it has issued and whether it may write.
///
/// The write gate lives here rather than at each call site so there is exactly one place that
/// decides it, and it is fixed when the session is created — a gate re-read per call could be
/// changed underneath a running server.
pub struct RefactorSession {
    tokens: TokenStore,
    write_allowed: bool,
}

impl RefactorSession {
    /// Opens a session. `write_allowed` is the environment gate, already decided by the host.
    #[must_use]
    pub fn new(write_allowed: bool) -> Self {
        Self {
            tokens: TokenStore::default(),
            write_allowed,
        }
    }

    /// A session that will never write, for callers that only plan.
    ///
    /// Named rather than a bare `new(false)` so a caller that meant to pass a real gate cannot
    /// silently get a closed one — which is exactly the bug this replaced.
    #[must_use]
    pub fn read_only() -> Self {
        Self::new(false)
    }

    /// Calls one refactor operation.
    ///
    /// # Errors
    ///
    /// Returns an error only when `name` is not a tool the contract declares. Every other
    /// refusal is a value carrying a contract status, because an agent branches on statuses and
    /// cannot branch on a transport error.
    pub fn call(
        &self,
        state: &RepositoryState,
        name: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        let Some(operation) = Operation::from_name(name) else {
            return Err(format!("unknown refactor operation: {name}"));
        };
        Ok(match operation {
            Operation::DeleteReadiness => delete_readiness::delete_readiness(state, arguments),
            Operation::EditSymbol => edit_symbol::edit_symbol(state, arguments),
            Operation::BulkReplace => bulk_replace::bulk_replace(state, arguments),
            Operation::MoveSymbol => move_symbol::move_symbol(state, arguments),
            Operation::ApplyEditPlan => {
                apply::apply_edit_plan(state.root(), &self.tokens, arguments, self.write_allowed)
            }
            Operation::RollbackLastApply => {
                apply::rollback_last_apply(state.root(), self.write_allowed)
            }
            _ => pending(operation),
        })
    }
}

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
pub fn call(state: &RepositoryState, name: &str, arguments: &Value) -> Result<Value, String> {
    RefactorSession::read_only().call(state, name, arguments)
}

/// The symbol an agent named is not in the graph.
pub(crate) fn not_found(symbol: &str) -> Value {
    json!({
        "status": "NOT_FOUND",
        "reason": "the selected symbol is not present in the active graph, or the name matches \
                   more than one symbol; pass an exact id",
        "symbol": symbol,
    })
}

/// The graph and the file no longer agree, so no range from it can be trusted.
pub(crate) fn stale_graph(file: &str) -> Value {
    json!({
        "status": "STALE_GRAPH",
        "reason": format!(
            "{file}: the recorded source range no longer matches the file. Rebuild the graph; \
             nothing was planned from a range that cannot be located."
        ),
    })
}

/// A missing or wrongly typed argument, named rather than described.
///
/// The engines below state their preconditions by returning this, never by panicking: an agent
/// branches on a status and cannot branch on a crash.
pub(crate) fn invalid_args(operation: &str, missing: &[&str]) -> Value {
    json!({
        "status": "INVALID_ARGS",
        "operation": operation,
        "invalid": missing,
        "reason": format!(
            "missing or invalid required argument(s): {}. Nothing was planned or written.",
            missing.join(", ")
        ),
    })
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
    fn every_operation_answers_with_a_contract_status_ported_or_not() {
        let state = crate::test_support::fixture_state();
        for tool in contract::tools() {
            let answer =
                call(&state, &tool.name, &blazingly_json::json!({})).expect("declared tool");
            let status = answer
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            assert!(
                contract::permits_state(status),
                "{} answered {status}, which is outside the contract",
                tool.name
            );
        }
    }

    #[test]
    fn an_undeclared_tool_is_an_error_not_a_status() {
        let state = crate::test_support::fixture_state();
        assert!(call(&state, "reformat_universe", &blazingly_json::json!({})).is_err());
    }

    #[test]
    fn the_catalog_matches_the_contract() {
        assert_eq!(
            catalog().as_array().map(Vec::len),
            Some(contract::tools().len())
        );
    }
}
