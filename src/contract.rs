//! The frozen tool contract, embedded rather than restated.
//!
//! `contract/refactor-tools.v1.json` was recorded from the shipping JavaScript implementation.
//! It is compiled into the binary and is the only source of the tool catalog, so the Rust host
//! cannot drift from the schemas agents already depend on: a change to a name, a schema or a
//! status has to change that file, and changing it is a contract-version decision.

use blazingly_json::Value;
use std::collections::BTreeSet;
use std::sync::OnceLock;

const FROZEN: &str = include_str!("../contract/refactor-tools.v1.json");

/// One tool exactly as the contract records it.
#[derive(Debug, Clone)]
pub struct ToolContract {
    /// Tool name as agents call it.
    pub name: String,
    /// Description shown in the catalog.
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
}

/// A status an operation is allowed to answer with.
///
/// An operation that needs a state outside this set is not conformant; the contract must be
/// versioned first. This is the rule that keeps two implementations answerable to one client.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResultState(pub String);

struct Frozen {
    tools: Vec<ToolContract>,
    states: BTreeSet<ResultState>,
    catalog: Value,
}

fn frozen() -> &'static Frozen {
    static FROZEN_ONCE: OnceLock<Frozen> = OnceLock::new();
    FROZEN_ONCE.get_or_init(|| {
        let parsed: Value = blazingly_json::from_str(FROZEN)
            .expect("the embedded contract is written by the build and must parse");
        let entries = parsed
            .get("tools")
            .and_then(Value::as_array)
            .expect("contract must carry a tools array")
            .clone();
        let tools = entries
            .iter()
            .map(|tool| ToolContract {
                name: field(tool, "name"),
                description: field(tool, "description"),
                input_schema: tool
                    .get("inputSchema")
                    .cloned()
                    .expect("every contract tool declares an inputSchema"),
            })
            .collect::<Vec<_>>();
        let mut states = BTreeSet::new();
        if let Some(groups) = parsed.get("resultStates").and_then(Value::as_object) {
            for (_, group) in groups {
                for state in group.as_array().into_iter().flatten() {
                    if let Some(text) = state.as_str() {
                        states.insert(ResultState(text.to_owned()));
                    }
                }
            }
        }
        Frozen {
            tools,
            states,
            catalog: Value::Array(entries),
        }
    })
}

fn field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Every tool the contract freezes, in contract order.
#[must_use]
pub fn tools() -> &'static [ToolContract] {
    &frozen().tools
}

/// The catalog exactly as the contract records it, ready for an MCP `tools/list`.
#[must_use]
pub fn catalog_value() -> &'static Value {
    &frozen().catalog
}

/// Whether `name` is one of the frozen tools.
#[must_use]
pub fn declares(name: &str) -> bool {
    frozen().tools.iter().any(|tool| tool.name == name)
}

/// Whether `state` is a status the contract permits an operation to answer with.
#[must_use]
pub fn permits_state(state: &str) -> bool {
    frozen().states.contains(&ResultState(state.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{catalog_value, declares, permits_state, tools};

    #[test]
    fn the_contract_freezes_eleven_tools() {
        assert_eq!(tools().len(), 11);
        for name in [
            "rename_symbol",
            "rename_related_symbols",
            "apply_edit_plan",
            "rollback_last_apply",
            "change_signature",
            "edit_symbol",
            "bulk_replace",
            "organize_imports",
            "move_file",
            "move_symbol",
            "delete_readiness",
        ] {
            assert!(declares(name), "{name} is missing from the frozen contract");
        }
    }

    #[test]
    fn every_tool_carries_a_description_and_a_schema() {
        for tool in tools() {
            assert!(
                !tool.description.is_empty(),
                "{} has no description",
                tool.name
            );
            assert!(
                tool.input_schema.get("type").is_some(),
                "{} has no input schema",
                tool.name
            );
        }
    }

    #[test]
    fn load_bearing_states_are_permitted_and_invented_ones_are_not() {
        for state in [
            "PREVIEW_OK",
            "APPLIED",
            "STALE",
            "REPO_BUSY",
            "ROLLBACK_INCOMPLETE",
            "INVALID_ARGS",
            "PARTIAL",
            "COMPLETE",
            "EXACT_LSP",
        ] {
            assert!(permits_state(state), "{state} must be a contract state");
        }
        assert!(!permits_state("MOSTLY_FINE"));
        assert!(!permits_state("OK_PROBABLY"));
    }

    #[test]
    fn the_catalog_is_the_contract_itself() {
        let catalog = catalog_value().as_array().expect("catalog is an array");
        assert_eq!(catalog.len(), tools().len());
    }
}
