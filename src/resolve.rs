//! Turning what an agent typed into exactly one graph node.
//!
//! Resolution is deliberately narrow. An exact id wins; a label wins only when it is unique.
//! An ambiguous label resolves to nothing rather than to the first match, because picking one
//! silently is how a refactor edits the wrong symbol — the same trap the benchmark caught, where
//! two files declared the same name and only one of them was the target.

use weavatrix_graph::{Graph, NodeIndex};

/// Resolves `query` to one node, or `None` when it matches nothing or more than one thing.
#[must_use]
pub fn resolve_symbol(graph: &Graph, query: &str) -> Option<NodeIndex> {
    let indexed = |slot: usize| NodeIndex::new(u32::try_from(slot).unwrap_or(u32::MAX));
    let nodes = graph.nodes();
    if let Some(slot) = nodes.iter().position(|node| node.id.as_str() == query) {
        return Some(indexed(slot));
    }
    let mut matches = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.label == query || node.id.as_str().ends_with(query));
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(indexed(first.0))
}

#[cfg(test)]
mod tests {
    use super::resolve_symbol;
    use crate::test_support::fixture_state;

    #[test]
    fn an_exact_id_resolves() {
        let state = fixture_state();
        let Some(first) = state
            .graph()
            .nodes()
            .first()
            .map(|node| node.id.as_str().to_owned())
        else {
            return;
        };
        assert!(resolve_symbol(state.graph(), &first).is_some());
    }

    #[test]
    fn an_unknown_query_resolves_to_nothing() {
        let state = fixture_state();
        assert!(resolve_symbol(state.graph(), "definitely::not::here").is_none());
    }

    #[test]
    fn an_ambiguous_label_refuses_rather_than_guessing() {
        let state = fixture_state();
        let graph = state.graph();
        // A label carried by two nodes must not resolve; editing the wrong one is the failure
        // mode this whole product exists to prevent.
        let mut seen = std::collections::BTreeMap::<&str, usize>::new();
        for node in graph.nodes() {
            *seen.entry(node.label.as_str()).or_default() += 1;
        }
        if let Some((label, _)) = seen.iter().find(|(_, count)| **count > 1) {
            assert!(
                resolve_symbol(graph, label).is_none(),
                "ambiguous label {label} must not resolve"
            );
        }
    }
}
