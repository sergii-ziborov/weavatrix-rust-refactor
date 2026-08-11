//! The write path: preview, apply, roll back.
//!
//! The transaction itself belongs to `weavatrix-worktree` — journal, atomic replacement, crash
//! recovery and undo are its job, and reimplementing any of that here would be a second, weaker
//! copy of a thing that already works. What this module owns is the contract: which statuses an
//! agent sees, and the rule that a preview is the only way to get permission to write.

use crate::envelope::read_envelope;
use crate::token::TokenStore;
use blazingly_json::{Value, json};
use weavatrix_refactor_plan::EditPlan;
use weavatrix_worktree::{
    UndoRetention, Worktree, WorktreeError, WorktreeErrorCode, WorktreeOperation, WorktreePlan,
};

/// The transaction takes a plan of operations; a text edit plan is the all-`Modify` case of one.
///
/// Going through the retained-apply path rather than the plain one is what makes
/// `rollback_last_apply` possible at all: only that path keeps the previous contents.
fn as_worktree_plan(plan: &EditPlan) -> WorktreePlan {
    WorktreePlan::new(
        plan.operation.clone(),
        plan.files
            .iter()
            .cloned()
            .map(WorktreeOperation::Modify)
            .collect(),
    )
}

fn worktree(root: &std::path::Path) -> Result<Worktree, Value> {
    Worktree::open(root).map_err(|error| {
        json!({
            "status": "REPO_BUSY",
            "reason": format!("the repository could not be opened for writing: {error}"),
        })
    })
}

/// Turns a transaction failure into the contract status that describes it.
///
/// Matched on the error's own code rather than on its prose. Sniffing a message for "hash" is
/// how a stale file gets reported as an invalid plan the day someone rewords the error, and the
/// difference matters: a stale tree is fixed by previewing again, an invalid plan never is.
fn failure(error: &WorktreeError) -> Value {
    use WorktreeErrorCode as Code;
    let status = match error.code() {
        Code::SourceHashMismatch | Code::ConcurrentModification => "STALE",
        Code::RootBusy | Code::RecoveryRequired => "REPO_BUSY",
        Code::RollbackFailed | Code::JournalCorrupt => "ROLLBACK_INCOMPLETE",
        _ => "INVALID_PLAN",
    };
    json!({"status": status, "reason": error.to_string()})
}

/// Whether a failure means the working tree moved under the plan.
fn is_stale(error: &WorktreeError) -> bool {
    matches!(
        error.code(),
        WorktreeErrorCode::SourceHashMismatch | WorktreeErrorCode::ConcurrentModification
    )
}

/// `apply_edit_plan`: preview verifies, apply writes.
///
/// An apply may present the confirmation alone. The preview already carried the plan to the
/// agent; making it echo the plan back is paying for the same bytes twice, the second time as
/// completion tokens. The token names exactly one previewed plan, so the server supplies it.
pub(super) fn apply_edit_plan(
    root: &std::path::Path,
    tokens: &TokenStore,
    arguments: &Value,
    write_allowed: bool,
) -> Value {
    let apply = arguments.get("mode").and_then(Value::as_str) == Some("apply");
    let presented = arguments.get("confirm_token").and_then(Value::as_str);
    if apply && arguments.get("plan").is_none() {
        if !write_allowed {
            return json!({
                "status": "WRITE_GATE_CLOSED",
                "reason": "the server was started without source edits enabled",
            });
        }
        let plan = match tokens.consume_for_plan(presented, root) {
            Ok(plan) => plan,
            Err(refusal) => return refusal,
        };
        let tree = match worktree(root) {
            Ok(tree) => tree,
            Err(refusal) => return refusal,
        };
        return write_plan(&tree, &plan);
    }
    let Some(plan_value) = arguments.get("plan") else {
        return super::invalid_args("apply_edit_plan", &["plan"]);
    };
    let plan = match read_envelope(plan_value) {
        Ok(plan) => plan,
        Err(error) => {
            return json!({
                "status": error.code,
                "reason": error.reason,
            });
        }
    };
    let tree = match worktree(root) {
        Ok(tree) => tree,
        Err(refusal) => return refusal,
    };

    if !apply {
        return match tree.dry_run(&plan) {
            Ok(report) => {
                let token = tokens.issue(&plan, root);
                json!({
                    "status": "PREVIEW_OK",
                    "files": report.files().iter().map(|file| json!({
                        "path": file.path(),
                        "status": if file.changed() { "READY" } else { "NO_CHANGE" },
                    })).collect::<Vec<_>>(),
                    "totalEdits": report.total_edits(),
                    "confirmToken": token.value,
                    "expiresAt": token.expires_at,
                    "next": "call apply_edit_plan again with the same plan, mode=\"apply\", and this \
                             confirm_token. The token is single-use and bound to this exact plan \
                             and repository.",
                })
            }
            // At preview time a moved tree is not an error state to recover from, it is the
            // answer: this plan cannot be applied, produce a fresh one.
            Err(error) if is_stale(&error) => json!({
                "status": "PREVIEW_BLOCKED",
                "reason": format!(
                    "the plan does not match the working tree, so nothing can be applied. \
                     Re-run the producing tool for a fresh plan: {error}"
                ),
            }),
            Err(error) => failure(&error),
        };
    }

    // Gate two is the environment, checked by the host before this runs. Gate three is the token.
    if !write_allowed {
        return json!({
            "status": "WRITE_GATE_CLOSED",
            "reason": "the server was started without source edits enabled",
        });
    }
    if let Some(refusal) = tokens.consume(presented, &plan, root) {
        return refusal;
    }
    write_plan(&tree, &plan)
}

/// Writes a plan whose gates have all been passed, retaining the previous contents.
fn write_plan(tree: &Worktree, plan: &EditPlan) -> Value {
    match tree.apply_plan_retained(&as_worktree_plan(plan), UndoRetention::default()) {
        Ok(report) => json!({
            "status": "APPLIED",
            "transactionId": report.apply().transaction_id(),
            "undoId": report.undo_id().to_string(),
            "files": plan.files.iter().map(|file| file.path.clone()).collect::<Vec<_>>(),
            "totalEdits": plan.files.iter().map(|file| file.edits.len()).sum::<usize>(),
            "next": "the previous contents are retained; rollback_last_apply restores them.",
        }),
        Err(error) => failure(&error),
    }
}

/// `rollback_last_apply`: restores the most recent retained transaction.
pub(super) fn rollback_last_apply(root: &std::path::Path, write_allowed: bool) -> Value {
    if !write_allowed {
        return json!({
            "status": "WRITE_GATE_CLOSED",
            "reason": "the server was started without source edits enabled",
        });
    }
    let tree = match worktree(root) {
        Ok(tree) => tree,
        Err(refusal) => return refusal,
    };
    let receipts = match tree.undo_receipts() {
        Ok(receipts) => receipts,
        Err(error) => return failure(&error),
    };
    let Some(latest) = receipts.last() else {
        return json!({
            "status": "NO_CHANGE",
            "reason": "no retained apply exists for this repository; there is nothing to roll back",
        });
    };
    match tree.rollback_undo(latest.id()) {
        Ok(report) => json!({
            "status": "ROLLED_BACK",
            "restored": report.restored_paths(),
            "rollbackTransactionId": report.rollback_transaction_id(),
            "next": "run verified_change phase=verify if the refactor is being retried.",
        }),
        Err(error) => {
            let text = error.to_string();
            json!({
                "status": "ROLLBACK_INCOMPLETE",
                "reason": format!(
                    "the retained contents are kept and the rollback can be retried: {text}"
                ),
            })
        }
    }
}
