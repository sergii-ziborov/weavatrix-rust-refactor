//! What moving a declaration to another file would do to the dependency structure.
//!
//! This one produces no edits on purpose. Relocating a declaration is a mechanical change any
//! editor makes correctly; what an agent cannot see is whether the move introduces a cycle, and
//! that is a property of the graph rather than of the text. So the answer is a projection —
//! which edges would appear, which would vanish, and whether the result still has no cycles.
//!
//! It is a prediction from simulated edges, not a rebuild, and says so.

use crate::evidence::declaring_file;
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use weavatrix_rust::{EdgeKind, RepositoryState};

/// Which file each symbol lives in, by node id.
fn owners(state: &RepositoryState) -> BTreeMap<String, String> {
    state
        .graph()
        .nodes()
        .iter()
        .filter_map(|node| {
            let file = node
                .span
                .as_ref()
                .map_or_else(|| node.label.clone(), |span| span.file.clone());
            (!file.is_empty()).then(|| (node.id.as_str().to_owned(), file))
        })
        .collect()
}

/// File-level dependency edges, with the moved symbol's own edges redirected.
///
/// Only relations that make one file need another take part. Containment says where a symbol
/// lives, which is exactly what the move changes, so counting it would report the move as its
/// own consequence.
fn file_edges(
    state: &RepositoryState,
    owners: &BTreeMap<String, String>,
    moved: Option<(&str, &str)>,
) -> BTreeSet<(String, String)> {
    let owner = |id: &str| -> Option<String> {
        // The moved symbol is treated as already living in its destination.
        match moved {
            Some((moved_id, destination)) if id == moved_id => Some(destination.to_owned()),
            _ => owners.get(id).cloned(),
        }
    };
    let mut edges = BTreeSet::new();
    for edge in state.graph().edges() {
        if matches!(edge.kind, EdgeKind::Contains) {
            continue;
        }
        let (Some(source), Some(target)) =
            (owner(edge.source.as_str()), owner(edge.target.as_str()))
        else {
            continue;
        };
        if source != target {
            edges.insert((source, target));
        }
    }
    edges
}

/// Files reachable from `start`, for cycle detection.
fn reaches(edges: &BTreeSet<(String, String)>, start: &str, goal: &str) -> bool {
    let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (source, target) in edges {
        adjacency
            .entry(source.as_str())
            .or_default()
            .push(target.as_str());
    }
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([start]);
    while let Some(current) = queue.pop_front() {
        if current == goal && !seen.is_empty() {
            return true;
        }
        if !seen.insert(current.to_owned()) {
            continue;
        }
        for next in adjacency.get(current).into_iter().flatten() {
            if *next == goal {
                return true;
            }
            queue.push_back(next);
        }
    }
    false
}

/// Every file-to-file cycle the edge set contains, as ordered pairs.
fn cycles(edges: &BTreeSet<(String, String)>) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (source, target) in edges {
        if source < target && edges.contains(&(target.clone(), source.clone())) {
            found.push((source.clone(), target.clone()));
        } else if source != target && reaches(edges, target, source) {
            let pair = (source.clone(), target.clone());
            if !found.contains(&pair) {
                found.push(pair);
            }
        }
    }
    found
}

pub(super) fn move_symbol(state: &RepositoryState, arguments: &Value) -> Value {
    let symbol = arguments.get("symbol").and_then(Value::as_str);
    let to_file = arguments.get("to_file").and_then(Value::as_str);
    let (Some(symbol), Some(to_file)) = (symbol, to_file) else {
        return super::invalid_args("move_symbol", &["symbol", "to_file"]);
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return super::not_found(symbol);
    };
    let Some(node) = state.graph().node_at(index) else {
        return super::not_found(symbol);
    };
    let Some(from) = declaring_file(node) else {
        return json!({
            "status": "NOT_A_SYMBOL",
            "reason": "the selected node has no declaring file, so there is nothing to move",
            "symbol": symbol,
        });
    };
    if from == to_file {
        return json!({
            "status": "NO_CHANGE",
            "reason": format!("{symbol} already lives in {to_file}"),
        });
    }

    let id = node.id.as_str().to_owned();
    let owners = owners(state);
    let before = file_edges(state, &owners, None);
    let after = file_edges(state, &owners, Some((&id, to_file)));
    let cycles_before = cycles(&before);
    let cycles_after = cycles(&after);
    let introduced = cycles_after
        .iter()
        .filter(|cycle| !cycles_before.contains(cycle))
        .collect::<Vec<_>>();
    let removed = cycles_before
        .iter()
        .filter(|cycle| !cycles_after.contains(cycle))
        .collect::<Vec<_>>();

    let new_dependencies = after
        .iter()
        .filter(|edge| !before.contains(*edge))
        .map(|(source, target)| json!({"from": source, "to": target}))
        .collect::<Vec<_>>();
    let importers = state
        .graph()
        .edges()
        .iter()
        .filter(|edge| edge.kind != EdgeKind::Contains && edge.target.as_str() == id)
        .filter_map(|edge| owners.get(edge.source.as_str()).cloned())
        .filter(|file| *file != from)
        .collect::<BTreeSet<_>>();

    let verdict = if introduced.is_empty() {
        "FEASIBLE"
    } else {
        "WOULD_VIOLATE"
    };
    json!({
        "status": "EVALUATED",
        "verdict": verdict,
        "move": {"symbol": node.label, "from": from, "to": to_file},
        "cycles": {
            "introduced": introduced.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
            "removed": removed.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
            "before": cycles_before.len(),
            "after": cycles_after.len(),
        },
        "blastRadius": {
            "importers": importers.iter().collect::<Vec<_>>(),
            "newDependencies": new_dependencies,
        },
        "fidelity": "PROJECTED_FROM_GRAPH_EDGES",
        "next": "this dry-run computes no byte edits. Move the declaration with your editor or \
                 edit_symbol, then run verified_change phase=verify for the authoritative result.",
    })
}
