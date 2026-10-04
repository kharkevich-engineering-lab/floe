//! The server side of the GitHub mirror (D49, `docs/design/github-mirror.md`
//! §B.8/§D.2): the placement-gated follow nudge and the loop's spawn. The only
//! coupling between the server and `floe-mirror`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use floe_git::RepoId;
use tokio::sync::Semaphore;

use crate::AppState;

/// Nudged follows running at once on this host: a pass can nudge every new
/// repository (initial clones) and every pushed one, and git serving must not
/// compete with all of them (the follow loop itself is sequential).
const NUDGE_CONCURRENCY: usize = 2;
/// A nudged follow holds its slot at most this long (a stuck fetch must not
/// block the others; the follow loop's backstop still covers its repository).
const NUDGE_HOLD_MAX: Duration = Duration::from_secs(3600);

/// Run a `follow` op now for a repository **this host maintains** (D28/D30);
/// elsewhere a no-op, and the maintaining host's follow loop picks the change
/// up within `upstream.follow_interval`. A running `follow` task is joined (the
/// task lock), so a nudge never runs a second fetch against the scratch. At
/// most [`NUDGE_CONCURRENCY`] nudged follows run at once; the rest queue.
pub fn nudge(state: Arc<AppState>) -> floe_mirror::Nudge {
    let slots = Arc::new(Semaphore::new(NUDGE_CONCURRENCY));
    Box::new(move |id: &RepoId| {
        if !state.cfg.placement.maintains(id.owner(), id.name()) {
            return;
        }
        let st = state.clone();
        let id = id.clone();
        let slots = slots.clone();
        tokio::spawn(async move {
            let Ok(_slot) = slots.acquire_owned().await else {
                return;
            };
            if floe_wal::tasks::draining() {
                return;
            }
            match crate::ops::start(st, id.clone(), "follow", HashMap::new()).await {
                Ok(task) => {
                    if !task.wait_done(NUDGE_HOLD_MAX).await {
                        tracing::warn!(repo = %id, "nudged follow still running; releasing its slot");
                    }
                }
                Err(crate::ops::StartError::AlreadyRunning(_)) => {}
                Err(crate::ops::StartError::UnknownOp) => {
                    tracing::debug!(repo = %id, "follow nudge: repository not openable here");
                }
            }
        });
    })
}

/// The mirror loop for a maintainer with `github_mirror.enabled`: its own loop,
/// never a unit of the priority loop (discovery must not wait behind a base
/// rebuild). Returns when draining.
pub async fn run_loop(state: Arc<AppState>) {
    let source = match floe_mirror::github::GithubSource::new(&state.cfg.github_mirror) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!(error = %e, "github mirror disabled: client setup failed");
            return;
        }
    };
    let target = floe_mirror::WalTarget::new(
        state.registry.clone(),
        state.store.clone(),
        nudge(state.clone()),
    );
    let mirror = Arc::new(floe_mirror::Mirror {
        cfg: state.cfg.clone(),
        source,
        target: Arc::new(target),
        store: state.store.clone(),
    });
    // Catalog telemetry (`sync_runs`/`repo_inventory`, §C.6) hooks in here once
    // `floe-catalog`'s Recorder lands on this branch.
    floe_mirror::run_loop(mirror, Box::new(|_: &floe_mirror::PassReport| {})).await;
}

#[cfg(test)]
mod tests {
    /// §B.7.2: the mirror's read-only policy parses and refuses every push.
    #[test]
    fn read_only_policy_refuses_pushes() {
        use floe_proto::v1::{RefTransaction, RefUpdate};
        let policy =
            crate::policy::parse_document(floe_mirror::target::READ_ONLY_POLICY.as_bytes()).unwrap();
        let oid = "1".repeat(40);
        let txn = RefTransaction {
            updates: vec![
                RefUpdate {
                    name: "refs/heads/main".into(),
                    old_oid: String::new(),
                    new_oid: oid.clone(),
                    ..RefUpdate::default()
                },
                RefUpdate {
                    name: "refs/tags/v1".into(),
                    old_oid: oid,
                    new_oid: String::new(),
                    ..RefUpdate::default()
                },
            ],
            push_options: Vec::new(),
            atomic: false,
        };
        let eval = crate::policy::evaluate(&policy, "alice", &txn, |_| false);
        assert!(eval.per_ref.iter().all(|(_, r)| r.as_ref().is_err_and(|e| e.contains("github-mirror-read-only"))), "{:?}", eval.per_ref);
        assert!(!eval.any_allowed());
    }
}
