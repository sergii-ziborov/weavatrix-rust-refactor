//! Renaming several symbols as one transaction.
//!
//! The reason this is not a loop over `rename_symbol` is that the interesting cases only exist
//! between the renames. Two symbols given the same new name collide. Two renames landing on the
//! same identifier contradict each other. And a batch that half-applies leaves the tree in a
//! state no single rename would have produced, so any sub-rename that refuses stops all of them.
//!
//! Chains (`a -> b` while `b -> c`) and swaps (`a -> b` while `b -> a`) are *not* refused. Every
//! edit here is anchored to a position and carries its own `before`, so the whole batch is
//! applied simultaneously rather than in sequence: `b`'s sites become `c` and `a`'s become `b`,
//! with no cascade. That is almost certainly what was meant, but it is the opposite of what
//! running two renames back to back would do, so it is reported rather than left to be assumed.

use super::rename_symbol::{Rename, Site, build_plan, sites};
use crate::envelope::read_envelope;
use crate::token::TokenStore;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use weavatrix_rust::RepositoryState;
use weavatrix_worktree::{UndoRetention, Worktree, WorktreeOperation, WorktreePlan};

/// One requested rename, as the agent wrote it.
struct Request {
    symbol: String,
    new_name: String,
}

/// Reads and validates the `renames` array before any graph work happens.
fn requests(arguments: &Value) -> Result<Vec<Request>, Value> {
    let Some(entries) = arguments.get("renames").and_then(Value::as_array) else {
        return Err(super::invalid_args("rename_related_symbols", &["renames"]));
    };
    if entries.is_empty() || entries.len() > 50 {
        return Err(json!({
            "status": "INVALID_ARGS",
            "operation": "rename_related_symbols",
            "reason": format!(
                "renames must hold between 1 and 50 entries; {} were given",
                entries.len()
            ),
        }));
    }
    let mut requests = Vec::with_capacity(entries.len());
    for (position, entry) in entries.iter().enumerate() {
        let symbol = entry.get("symbol").and_then(Value::as_str);
        let new_name = entry.get("new_name").and_then(Value::as_str);
        let (Some(symbol), Some(new_name)) = (symbol, new_name) else {
            return Err(json!({
                "status": "INVALID_ARGS",
                "operation": "rename_related_symbols",
                "reason": format!("renames[{position}] needs both symbol and new_name"),
            }));
        };
        requests.push(Request {
            symbol: symbol.to_owned(),
            new_name: new_name.to_owned(),
        });
    }
    Ok(requests)
}

/// Two renames that would write different names over the same identifier.
///
/// This is the failure a batch exists to catch. Each rename is individually correct; together
/// they disagree about one position, and whichever the applier ordered last would silently win.
fn overlaps(renames: &[Rename]) -> Vec<Value> {
    let mut claimed: BTreeMap<(&str, Site), &str> = BTreeMap::new();
    let mut found = Vec::new();
    for rename in renames {
        for (path, (_, file_sites)) in &rename.files {
            for site in file_sites {
                if let Some(previous) = claimed.insert((path, *site), &rename.new_name)
                    && previous != rename.new_name
                {
                    found.push(json!({
                        "file": path,
                        "line": site.line,
                        "kind": "CONTESTED_SITE",
                        "reason": format!(
                            "two renames claim this identifier, one writing {previous:?} and one \
                             writing {:?}",
                            rename.new_name
                        ),
                    }));
                }
            }
        }
    }
    found
}

/// Renames that would leave two symbols sharing one name.
fn collisions(renames: &[Rename]) -> Vec<Value> {
    let mut by_new_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for rename in renames {
        by_new_name
            .entry(&rename.new_name)
            .or_default()
            .push(&rename.old_name);
    }
    by_new_name
        .into_iter()
        .filter(|(_, sources)| sources.len() > 1)
        .map(|(new_name, sources)| {
            json!({
                "kind": "NAME_COLLISION",
                "newName": new_name,
                "from": sources,
                "reason": "more than one symbol would end up with this name",
            })
        })
        .collect()
}

/// Renames whose target name is another rename's source, reported rather than refused.
fn chains(renames: &[Rename]) -> Vec<Value> {
    let sources: BTreeSet<&str> = renames.iter().map(|r| r.old_name.as_str()).collect();
    let mut found = Vec::new();
    for rename in renames {
        if !sources.contains(rename.new_name.as_str()) {
            continue;
        }
        let swap = renames
            .iter()
            .any(|other| other.old_name == rename.new_name && other.new_name == rename.old_name);
        found.push(json!({
            "kind": if swap { "SWAP" } else { "CHAIN" },
            "from": rename.old_name,
            "to": rename.new_name,
            "reason": "the target name is also being renamed; every edit is positional, so the \
                       batch applies simultaneously and this does not cascade",
        }));
    }
    found
}

/// `rename_related_symbols`: one plan, one token, all or nothing.
pub(super) fn rename_related_symbols(
    state: &RepositoryState,
    tokens: &TokenStore,
    arguments: &Value,
    write_allowed: bool,
) -> Value {
    let requested = match requests(arguments) {
        Ok(requested) => requested,
        Err(refusal) => return refusal,
    };

    // Any sub-rename that refuses stops the batch, and its own refusal is carried out unchanged:
    // an agent that knows how to read NOT_FOUND from rename_symbol reads it here too.
    let mut renames = Vec::with_capacity(requested.len());
    for request in &requested {
        match sites(state, &request.symbol, &request.new_name) {
            Ok(found) => renames.push(found),
            Err(refusal) => {
                return json!({
                    "status": "BLOCKED",
                    "reason": format!(
                        "{} could not be renamed, so none of the {} renames were planned",
                        request.symbol,
                        requested.len()
                    ),
                    "failed": {"symbol": request.symbol, "newName": request.new_name},
                    "cause": refusal,
                });
            }
        }
    }

    let contested = overlaps(&renames);
    let colliding = collisions(&renames);
    if !contested.is_empty() || !colliding.is_empty() {
        return json!({
            "status": "CONFLICT",
            "reason": "the requested renames contradict each other; nothing was planned",
            "conflicts": contested.into_iter().chain(colliding).collect::<Vec<_>>(),
        });
    }

    let batch = Batch {
        plan: build_plan("rename_related_symbols", &renames),
        uncertain: renames
            .iter()
            .flat_map(|rename| rename.uncertain.iter().cloned())
            .collect(),
        names: renames
            .iter()
            .map(|rename| json!({"from": rename.old_name, "to": rename.new_name}))
            .collect(),
        total: renames.iter().map(Rename::edits).sum(),
        ordering: chains(&renames),
    };

    if arguments.get("mode").and_then(Value::as_str) != Some("apply") {
        return preview(state.root(), tokens, &batch);
    }
    if !write_allowed {
        return json!({
            "status": "WRITE_GATE_CLOSED",
            "reason": "the server was started without source edits enabled",
        });
    }
    apply(state.root(), tokens, arguments, &batch)
}

/// Everything the batch resolved to, shared by the preview and the write.
///
/// Both halves report the same renames and the same count, so they read one value rather than
/// each rebuilding it — a preview that described a different batch than the apply wrote would be
/// the worst possible bug in a tool whose whole promise is that you confirm what you saw.
struct Batch {
    plan: Value,
    names: Vec<Value>,
    ordering: Vec<Value>,
    uncertain: Vec<Value>,
    total: usize,
}

/// Verifies the batch against the working tree and issues the token that unlocks it.
fn preview(root: &std::path::Path, tokens: &TokenStore, batch: &Batch) -> Value {
    let envelope = match read_envelope(&batch.plan) {
        Ok(envelope) => envelope,
        Err(error) => return json!({"status": error.code, "reason": error.reason}),
    };
    let tree = match Worktree::open(root) {
        Ok(tree) => tree,
        Err(error) => {
            return json!({
                "status": "REPO_BUSY",
                "reason": format!("the repository could not be opened for writing: {error}"),
            });
        }
    };
    match tree.dry_run(&envelope) {
        Ok(report) => {
            let token = tokens.issue(&envelope, root);
            json!({
                "status": "PREVIEW_OK",
                "completeness": "PARTIAL",
                "backend": "graph+lexical",
                "renames": batch.names.clone(),
                "renamedEdits": batch.total,
                "files": report.files().iter()
                    .map(|file| file.path().to_owned())
                    .collect::<Vec<_>>(),
                "ordering": batch.ordering.clone(),
                "uncertainReferences": batch.uncertain.clone(),
                "warnings": ["GRAPH_PROVEN_SITES_ONLY"],
                "plan": batch.plan.clone(),
                "confirmToken": token.value,
                "expiresAt": token.expires_at,
                "next": "call again with the identical renames, mode=\"apply\", and this \
                         confirm_token. The token is single-use and bound to this exact plan.",
            })
        }
        Err(error) => json!({
            "status": "PREVIEW_BLOCKED",
            "reason": format!(
                "the plan does not match the working tree, so nothing can be applied. Preview \
                 again for a fresh plan: {error}"
            ),
        }),
    }
}

/// Writes the batch atomically, keeping the previous contents for `rollback_last_apply`.
fn apply(root: &std::path::Path, tokens: &TokenStore, arguments: &Value, batch: &Batch) -> Value {
    let envelope = match read_envelope(&batch.plan) {
        Ok(envelope) => envelope,
        Err(error) => return json!({"status": error.code, "reason": error.reason}),
    };
    let presented = arguments.get("confirm_token").and_then(Value::as_str);
    // The token is bound to the plan, and the plan was just recomputed from the current graph.
    // A batch whose sites moved therefore fails here rather than writing something else.
    if let Some(refusal) = tokens.consume(presented, &envelope, root) {
        return refusal;
    }
    let tree = match Worktree::open(root) {
        Ok(tree) => tree,
        Err(error) => {
            return json!({
                "status": "REPO_BUSY",
                "reason": format!("the repository could not be opened for writing: {error}"),
            });
        }
    };
    let worktree_plan = WorktreePlan::new(
        envelope.operation.clone(),
        envelope
            .files
            .iter()
            .cloned()
            .map(WorktreeOperation::Modify)
            .collect(),
    );
    match tree.apply_plan_retained(&worktree_plan, UndoRetention::default()) {
        Ok(report) => json!({
            "status": "APPLIED",
            "transactionId": report.apply().transaction_id(),
            "undoId": report.undo_id().to_string(),
            "renames": batch.names.clone(),
            "renamedEdits": batch.total,
            "files": envelope.files.iter().map(|file| file.path.clone()).collect::<Vec<_>>(),
            "next": "every rename landed in one transaction; rollback_last_apply undoes all of them.",
        }),
        Err(error) => json!({
            "status": "STALE",
            "reason": format!("nothing was written; the batch is unchanged on disk: {error}"),
        }),
    }
}
