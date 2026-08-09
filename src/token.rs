//! Gate three: a confirmation bound to one plan, one repository, and one use.
//!
//! The token is not a password — it is proof that the exact plan being applied is the one a
//! preview already checked against the working tree. So it carries a fingerprint of the plan
//! rather than a random value alone, and presenting it for a different plan fails as loudly as
//! presenting no token at all.

use blazingly_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use weavatrix_refactor_plan::EditPlan;

/// How long a confirmation stays valid. Long enough to read a preview, short enough that a stale
/// tree cannot hide behind it.
const TOKEN_TTL: Duration = Duration::from_secs(5 * 60);

/// A confirmation handed to the caller after a successful preview.
pub struct ConfirmToken {
    pub value: String,
    pub expires_at: u64,
}

struct Issued {
    fingerprint: String,
    repository: String,
    expires_at: u64,
}

/// The issued confirmations of one server process.
#[derive(Default)]
pub struct TokenStore(Mutex<HashMap<String, Issued>>);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A stable fingerprint of everything in the plan that decides what gets written.
///
/// Ordering, paths, hashes, ranges and both texts all take part: change any of them and the
/// token stops matching, which is exactly the property that makes it a proof rather than a
/// formality.
fn fingerprint(plan: &EditPlan) -> String {
    let mut material = String::from(&plan.operation);
    for file in &plan.files {
        material.push('\u{1}');
        material.push_str(&file.path);
        material.push('\u{2}');
        material.push_str(&file.sha256);
        for edit in &file.edits {
            use std::fmt::Write as _;
            material.push('\u{3}');
            let _ = write!(
                material,
                "{}:{}:{}:{}:{}:{}:{}",
                edit.start_line,
                edit.start_char,
                edit.end_line,
                edit.end_char,
                edit.before,
                edit.after,
                edit.provenance.as_str()
            );
        }
    }
    // A non-cryptographic digest is enough: this value never leaves the process and an attacker
    // who can call this server can call the planner too.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in material.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

impl TokenStore {
    /// Issues a confirmation for a previewed plan.
    pub fn issue(&self, plan: &EditPlan, repository: &Path) -> ConfirmToken {
        let expires_at = now() + TOKEN_TTL.as_secs();
        let fingerprint = fingerprint(plan);
        let value = format!(
            "{fingerprint}{:016x}",
            now().wrapping_mul(0x9e37_79b9_7f4a_7c15)
        );
        if let Ok(mut issued) = self.0.lock() {
            issued.retain(|_, token| token.expires_at > now());
            issued.insert(
                value.clone(),
                Issued {
                    fingerprint,
                    repository: repository.display().to_string(),
                    expires_at,
                },
            );
        }
        ConfirmToken { value, expires_at }
    }

    /// Consumes a confirmation, or returns the refusal that says why it could not be.
    ///
    /// Consuming happens whether or not the checks pass: a token that was presented is spent,
    /// so a caller cannot probe with the same value twice.
    pub fn consume(
        &self,
        presented: Option<&str>,
        plan: &EditPlan,
        repository: &Path,
    ) -> Option<Value> {
        let Some(presented) = presented else {
            return Some(json!({
                "status": "TOKEN_UNKNOWN",
                "reason": "mode=\"apply\" requires the confirm_token issued by a preview of this \
                           exact plan. Nothing was written.",
            }));
        };
        let mut issued = self.0.lock().ok()?;
        let Some(token) = issued.remove(presented) else {
            return Some(json!({
                "status": "TOKEN_UNKNOWN",
                "reason": "the confirmation is not one this server issued, or it was already used. \
                           Nothing was written.",
            }));
        };
        if token.expires_at <= now() {
            return Some(json!({
                "status": "TOKEN_EXPIRED",
                "reason": "the confirmation expired; preview again to get a fresh one. Nothing was written.",
            }));
        }
        if token.repository != repository.display().to_string() {
            return Some(json!({
                "status": "TOKEN_REPOSITORY_MISMATCH",
                "reason": "the confirmation belongs to a different repository. Nothing was written.",
            }));
        }
        if token.fingerprint != fingerprint(plan) {
            return Some(json!({
                "status": "TOKEN_PLAN_MISMATCH",
                "reason": "the plan changed after it was previewed; the confirmation proves a \
                           different plan. Nothing was written.",
            }));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::TokenStore;
    use blazingly_json::Value;
    use std::path::Path;
    use weavatrix_refactor_plan::{EditPlan, FileEdit, Provenance, TextEdit};

    fn plan(after: &str) -> EditPlan {
        EditPlan::new(
            "rename_symbol",
            vec![FileEdit::new(
                "src/a.rs",
                "0".repeat(64),
                vec![TextEdit {
                    start_line: 1,
                    start_char: 0,
                    end_line: 1,
                    end_char: 3,
                    before: "one".to_owned(),
                    after: after.to_owned(),
                    provenance: Provenance::new(Provenance::EXACT_LSP),
                    extensions: std::collections::BTreeMap::new(),
                }],
            )],
        )
    }

    fn status(value: Option<&Value>) -> Option<&str> {
        value?.get("status")?.as_str()
    }

    #[test]
    fn a_previewed_plan_applies_once() {
        let store = TokenStore::default();
        let repository = Path::new("/repo");
        let token = store.issue(&plan("two"), repository);
        assert!(
            store
                .consume(Some(&token.value), &plan("two"), repository)
                .is_none()
        );
        // The same value a second time is spent, whatever it proved the first time.
        let replay = store.consume(Some(&token.value), &plan("two"), repository);
        assert_eq!(status(replay.as_ref()), Some("TOKEN_UNKNOWN"));
    }

    #[test]
    fn a_token_does_not_travel_to_another_plan() {
        let store = TokenStore::default();
        let repository = Path::new("/repo");
        let token = store.issue(&plan("two"), repository);
        let refusal = store.consume(Some(&token.value), &plan("three"), repository);
        assert_eq!(status(refusal.as_ref()), Some("TOKEN_PLAN_MISMATCH"));
    }

    #[test]
    fn a_token_does_not_travel_to_another_repository() {
        let store = TokenStore::default();
        let token = store.issue(&plan("two"), Path::new("/repo"));
        let refusal = store.consume(Some(&token.value), &plan("two"), Path::new("/other"));
        assert_eq!(status(refusal.as_ref()), Some("TOKEN_REPOSITORY_MISMATCH"));
    }

    #[test]
    fn applying_without_a_confirmation_is_refused() {
        let store = TokenStore::default();
        let refusal = store.consume(None, &plan("two"), Path::new("/repo"));
        assert_eq!(status(refusal.as_ref()), Some("TOKEN_UNKNOWN"));
    }

    #[test]
    fn an_invented_confirmation_is_refused() {
        let store = TokenStore::default();
        let refusal = store.consume(Some("deadbeef"), &plan("two"), Path::new("/repo"));
        assert_eq!(status(refusal.as_ref()), Some("TOKEN_UNKNOWN"));
    }
}
