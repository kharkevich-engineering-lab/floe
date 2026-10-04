//! Upstream follow (`[upstream] follow`, `floe_server::follow`): the maintaining
//! host brings followed refs up to an upstream git host's through the WAL — the
//! same PUSH entry a push produces. `on_rewrite = "refuse"` is D33 (fast-forward
//! only); the default `"archive"` (D48) keeps a rewritten or deleted ref's old tip
//! under `refs/archive/<ts>/<ref>` in the same entry. The upstream here is a
//! second floe instance (smart HTTP v2 over 127.0.0.1, real `git fetch`).

mod harness;

use harness::{Server, git, git_in};

macro_rules! step {
    ($name:literal, $e:expr) => {
        tokio::time::timeout(std::time::Duration::from_secs(60), $e)
            .await
            .unwrap_or_else(|_| panic!("step timed out: {}", $name))
    };
}

fn commit(work: &std::path::Path, name: &str) -> anyhow::Result<String> {
    std::fs::write(work.join(name), name)?;
    git_in(work, &["add", "."])?;
    git_in(work, &["commit", "-q", "-m", name])?;
    Ok(git_in(work, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// `git ls-remote <url>` → (ref → oid), peeled lines included as `<ref>^{}`.
fn ls_remote(url: &str) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let out = std::process::Command::new("git")
        .args(["ls-remote", url])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "ls-remote: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            l.split_once('\t')
                .map(|(o, n)| (n.to_string(), o.to_string()))
        })
        .collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_rewrite_refuse_keeps_d33_behaviour() -> anyhow::Result<()> {
    // Upstream: its own instance and store, repo u/src with 3 commits and an annotated tag.
    let up = step!("start upstream", Server::start())?;
    step!("put upstream repo", up.put_repo("u", "src"))?;
    let up_url = up.repo_url("u", "src");
    let work = tempfile::tempdir()?;
    git_in(work.path(), &["init", "-q", "-b", "main"])?;
    git_in(work.path(), &["config", "user.email", "t@t"])?;
    git_in(work.path(), &["config", "user.name", "Tester"])?;
    let c1 = commit(work.path(), "a")?;
    let c2 = commit(work.path(), "b")?;
    let c3 = commit(work.path(), "c")?;
    git_in(work.path(), &["tag", "-a", "v1", "-m", "v1", &c2])?;
    git(&["push", "-q", &up_url, "main", "v1"], work.path())?;

    // Follower: maintainer of everything, o/r follows u/src's main and v1.
    let fo = step!(
        "start follower",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![floe_config::Role::Serve, floe_config::Role::Maintain];
            c.upstream.git = Some(up_url.clone());
            c.upstream.follow = vec!["refs/heads/main".into(), "refs/tags/v1".into()];
            c.upstream.on_rewrite = floe_config::OnRewrite::Refuse;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.wal.snapshot_every_entries = 0;
        })
    )?;
    step!("put follower repo", fo.put_repo("o", "r"))?;
    let fo_url = fo.repo_url("o", "r");
    let id = floe_git::RepoId::new("o", "r")?;

    // Round 1: the whole history, both refs, one PUSH entry by `upstream`.
    let r = step!("round 1", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!(
        (r.repos, r.behind, r.published, r.failed),
        (1, 1, 1, 0),
        "{r:?}"
    );
    let refs = ls_remote(&fo_url)?;
    assert_eq!(refs.get("refs/heads/main"), Some(&c3));
    assert_eq!(
        refs.get("refs/tags/v1^{}"),
        Some(&c2),
        "peeled tag advertised: {refs:?}"
    );
    let h = step!("open", fo.state.registry.open(&id))?;
    assert_eq!(h.manifest().head_seq, 1);
    let entries = step!("log", h.read_log(1, None))?;
    assert_eq!(
        entries[0].meta.get("principal").map(String::as_str),
        Some("upstream")
    );
    assert_eq!(
        entries[0].meta.get("upstream").map(String::as_str),
        Some(up_url.as_str())
    );
    assert!(
        entries[0].pack.is_some(),
        "the delta travels as a pack in the entry"
    );
    let tasks = step!("tasks", fo.get_text("/o/r/api/tasks", &[]))?;
    assert!(tasks.contains("\"follow\""), "{tasks}");

    // Round 2: nothing moved — no entry, no task.
    let r = step!("round 2", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (0, 0, 0), "{r:?}");
    assert_eq!(h.manifest().head_seq, 1);
    assert_eq!(
        tasks.matches("\"follow\"").count(),
        step!("tasks again", fo.get_text("/o/r/api/tasks", &[]))?
            .matches("\"follow\"")
            .count()
    );

    // Upstream moves forward: the delta (one commit) is published.
    let c4 = commit(work.path(), "d")?;
    git(&["push", "-q", &up_url, "main"], work.path())?;
    let r = step!("round 3", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (1, 1, 0), "{r:?}");
    assert_eq!(ls_remote(&fo_url)?.get("refs/heads/main"), Some(&c4));
    assert_eq!(h.manifest().head_seq, 2);

    // Upstream rewinds main to c2: refused (not a fast-forward), nothing published, visible as a failed round.
    git(
        &[
            "push",
            "-q",
            "--force",
            &up_url,
            &format!("{c2}:refs/heads/main"),
        ],
        work.path(),
    )?;
    let r = step!("round 4", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (1, 0, 1), "{r:?}");
    assert_eq!(ls_remote(&fo_url)?.get("refs/heads/main"), Some(&c4));
    assert_eq!(h.manifest().head_seq, 2);

    // The same rewind next round: still refused, but without another task (the
    // op would refuse the same old/new again).
    let follow_tasks = step!("tasks", fo.get_text("/o/r/api/tasks", &[]))?
        .matches("\"follow\"")
        .count();
    let r = step!("round 4b", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (0, 0, 0), "{r:?}");
    assert_eq!(
        step!("tasks", fo.get_text("/o/r/api/tasks", &[]))?
            .matches("\"follow\"")
            .count(),
        follow_tasks
    );
    let last = fo.state.follow.get("o/r").expect("a round ran");
    assert_eq!(last.outcome, "refused", "{last:?}");
    assert!(last.detail.contains("refs/heads/main"), "{last:?}");

    // Upstream goes forward again past our tip: followed.
    git_in(work.path(), &["reset", "-q", "--hard", &c4])?;
    let c5 = commit(work.path(), "e")?;
    git(&["push", "-q", "--force", &up_url, "main"], work.path())?;
    let r = step!("round 5", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (1, 1, 0), "{r:?}");
    assert_eq!(ls_remote(&fo_url)?.get("refs/heads/main"), Some(&c5));

    // The Settings tab sees the configuration and this instance's last round.
    let d: serde_json::Value = serde_json::from_str(&step!(
        "describe",
        fo.get_text("/o/r/api/settings/describe", &[])
    )?)?;
    assert_eq!(d["upstream"]["git"], serde_json::json!(up_url));
    assert_eq!(
        d["upstream"]["follow"],
        serde_json::json!(["refs/heads/main", "refs/tags/v1"])
    );
    assert_eq!(
        d["upstream"]["last_round"]["outcome"], "published",
        "{}",
        d["upstream"]
    );
    assert_eq!(
        d["upstream"]["last_round"]["upstream"]["refs/heads/main"],
        serde_json::json!(c5)
    );

    // The manual op (UI/CLI) fetches itself; in sync now.
    let task = step!("op", crate::start_op(&fo, &id)).map_err(|e| anyhow::anyhow!(e))?;
    assert!(task.wait_done(std::time::Duration::from_secs(30)).await);
    let outcome = task.outcome().expect("finished");
    assert!(
        outcome
            .as_ref()
            .is_ok_and(|o| o.task.summary.contains("in sync")),
        "{outcome:?}"
    );

    // What the follower serves is complete: a fresh clone has the whole history.
    let clone = tempfile::tempdir()?;
    git(&["clone", "-q", &fo_url, "c"], clone.path())?;
    assert_eq!(
        git_in(&clone.path().join("c"), &["rev-parse", "HEAD"])?.trim(),
        c5
    );
    assert_eq!(
        git_in(&clone.path().join("c"), &["rev-list", "--count", "HEAD"])?.trim(),
        "5"
    );

    // Upstream deletes the tag: refused every round, but in the loop alone — no
    // task, no Serve-level sync, not a failure — and the tag stays.
    git(&["push", "-q", &up_url, ":refs/tags/v1"], work.path())?;
    let follow_tasks = step!("tasks", fo.get_text("/o/r/api/tasks", &[]))?
        .matches("\"follow\"")
        .count();
    for round in ["round 6", "round 7"] {
        let r = step!("round", floe_server::follow::run_pass(&fo.state))?;
        assert_eq!(
            (r.behind, r.published, r.failed),
            (0, 0, 0),
            "{round}: {r:?}"
        );
        let last = fo.state.follow.get("o/r").expect("a round ran");
        assert_eq!(last.outcome, "refused", "{round}: {last:?}");
        assert!(
            last.detail.contains("refs/tags/v1: deleted upstream"),
            "{round}: {last:?}"
        );
    }
    assert_eq!(
        step!("tasks", fo.get_text("/o/r/api/tasks", &[]))?
            .matches("\"follow\"")
            .count(),
        follow_tasks
    );
    assert!(ls_remote(&fo_url)?.contains_key("refs/tags/v1"));
    let _ = c1;
    Ok(())
}

/// An in-sync round reads refs only: it must not pull the packs of a repository
/// the cache evicted (no background prefetch from the follow loop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_sync_round_does_not_rematerialize_evicted_packs() -> anyhow::Result<()> {
    let up = step!("start upstream", Server::start())?;
    step!("put upstream repo", up.put_repo("u", "src"))?;
    let up_url = up.repo_url("u", "src");
    let work = tempfile::tempdir()?;
    git_in(work.path(), &["init", "-q", "-b", "main"])?;
    git_in(work.path(), &["config", "user.email", "t@t"])?;
    git_in(work.path(), &["config", "user.name", "Tester"])?;
    commit(work.path(), "a")?;
    git(&["push", "-q", &up_url, "main"], work.path())?;
    let fo = step!(
        "start follower",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![floe_config::Role::Serve, floe_config::Role::Maintain];
            c.upstream.git = Some(up_url.clone());
            c.upstream.follow = vec!["refs/heads/*".into()];
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.wal.snapshot_every_entries = 0;
            c.wal.prefetch_packs = true;
            // Evict every idle repository on demand.
            c.cache.max_bytes = floe_config::ByteSize::b(0);
            c.cache.evict_idle_after = std::time::Duration::ZERO;
        })
    )?;
    step!("put follower repo", fo.put_repo("o", "r"))?;
    let r = step!("round 1", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (1, 1, 0), "{r:?}");
    assert!(fo.registry_has_packs("o", "r").await);

    let evicted = step!("evict", fo.state.registry.evict_idle())?;
    assert!(evicted.evicted >= 1, "{evicted:?}");
    let r = step!("round 2", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!(
        (r.repos, r.behind, r.published, r.failed),
        (1, 0, 0, 0),
        "{r:?}"
    );
    // A prefetch would be a background task: give it time to land.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(
        !fo.registry_has_packs("o", "r").await,
        "the in-sync round pulled the evicted pack set back"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_globs_force_push_and_delete_are_archived_then_applied() -> anyhow::Result<()> {
    let up = step!("start upstream", Server::start())?;
    step!("put upstream repo", up.put_repo("u", "src"))?;
    let up_url = up.repo_url("u", "src");
    let work = tempfile::tempdir()?;
    git_in(work.path(), &["init", "-q", "-b", "main"])?;
    git_in(work.path(), &["config", "user.email", "t@t"])?;
    git_in(work.path(), &["config", "user.name", "Tester"])?;
    let c1 = commit(work.path(), "a")?;
    let c2 = commit(work.path(), "b")?;
    git_in(work.path(), &["tag", "-a", "v1", "-m", "v1", &c1])?;
    git_in(work.path(), &["branch", "feat/x", &c1])?;
    git_in(work.path(), &["branch", "wip/y", &c1])?;
    git(
        &["push", "-q", &up_url, "main", "feat/x", "wip/y", "v1"],
        work.path(),
    )?;

    // Default on_rewrite (archive); globs with a negative.
    let fo = step!(
        "start follower",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![floe_config::Role::Serve, floe_config::Role::Maintain];
            c.upstream.git = Some(up_url.clone());
            c.upstream.follow = vec![
                "refs/heads/*".into(),
                "refs/tags/*".into(),
                "^refs/heads/wip/*".into(),
            ];
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.wal.snapshot_every_entries = 0;
        })
    )?;
    step!("put follower repo", fo.put_repo("o", "r"))?;
    let fo_url = fo.repo_url("o", "r");
    let id = floe_git::RepoId::new("o", "r")?;

    // Round 1: every matching branch and tag is created; wip/* is excluded.
    let r = step!("round 1", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (1, 1, 0), "{r:?}");
    let refs = ls_remote(&fo_url)?;
    assert_eq!(refs.get("refs/heads/main"), Some(&c2), "{refs:?}");
    assert_eq!(refs.get("refs/heads/feat/x"), Some(&c1), "{refs:?}");
    assert!(refs.contains_key("refs/tags/v1"), "{refs:?}");
    assert!(!refs.contains_key("refs/heads/wip/y"), "{refs:?}");
    let h = step!("open", fo.state.registry.open(&id))?;
    assert_eq!(h.manifest().head_seq, 1);

    // A fast-forward is not archived.
    let c3 = commit(work.path(), "c")?;
    git(&["push", "-q", &up_url, "main"], work.path())?;
    let r = step!("round 2", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (1, 1, 0), "{r:?}");
    assert!(
        !ls_remote(&fo_url)?
            .keys()
            .any(|k| k.starts_with("refs/archive/"))
    );

    // Upstream force-pushes main back to c1 and deletes feat/x: one PUSH entry
    // archives both old tips and applies upstream's state.
    git(
        &[
            "push",
            "-q",
            "--force",
            &up_url,
            &format!("{c1}:refs/heads/main"),
            ":refs/heads/feat/x",
        ],
        work.path(),
    )?;
    let r = step!("round 3", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (1, 1, 0), "{r:?}");
    let refs = ls_remote(&fo_url)?;
    assert_eq!(refs.get("refs/heads/main"), Some(&c1), "{refs:?}");
    assert!(!refs.contains_key("refs/heads/feat/x"), "{refs:?}");
    let archived_main: Vec<(&String, &String)> = refs
        .iter()
        .filter(|(k, _)| k.starts_with("refs/archive/") && k.ends_with("/refs/heads/main"))
        .collect();
    assert_eq!(archived_main.len(), 1, "{refs:?}");
    assert_eq!(archived_main[0].1, &c3);
    let archived_feat = refs
        .iter()
        .find(|(k, _)| k.starts_with("refs/archive/") && k.ends_with("/refs/heads/feat/x"));
    assert_eq!(archived_feat.map(|(_, v)| v), Some(&c1), "{refs:?}");
    assert_eq!(h.manifest().head_seq, 3, "one entry for the whole round");
    let entries = step!("log", h.read_log(3, None))?;
    let meta = entries[0]
        .meta
        .get("follow.archived")
        .cloned()
        .unwrap_or_default();
    assert_eq!(meta.lines().count(), 2, "{meta}");
    assert!(
        meta.lines()
            .any(|l| l.ends_with(&format!("refs/heads/main {c3} {c1}"))),
        "{meta}"
    );
    assert!(
        meta.lines()
            .any(|l| l.ends_with(&format!("refs/heads/feat/x {c1} -"))),
        "{meta}"
    );
    let d: serde_json::Value = serde_json::from_str(&step!(
        "describe",
        fo.get_text("/o/r/api/settings/describe", &[])
    )?)?;
    assert_eq!(
        d["upstream"]["last_round"]["outcome"], "archived",
        "{}",
        d["upstream"]
    );
    assert_eq!(d["upstream"]["on_rewrite"], "archive");

    // A clone still reaches the rewritten commit through its archive ref.
    let clone = tempfile::tempdir()?;
    git(&["clone", "-q", "--mirror", &fo_url, "c.git"], clone.path())?;
    git_in(&clone.path().join("c.git"), &["cat-file", "-e", &c3])?;

    // Nothing moved: in sync, no new entry.
    let r = step!("round 4", floe_server::follow::run_pass(&fo.state))?;
    assert_eq!((r.behind, r.published, r.failed), (0, 0, 0), "{r:?}");
    assert_eq!(h.manifest().head_seq, 3);
    Ok(())
}

async fn start_op(
    fo: &Server,
    id: &floe_git::RepoId,
) -> Result<std::sync::Arc<floe_wal::tasks::TaskState>, String> {
    match floe_server::ops::start(
        fo.state.clone(),
        id.clone(),
        "follow",
        std::collections::HashMap::new(),
    )
    .await
    {
        Ok(t) => Ok(t),
        Err(floe_server::ops::StartError::AlreadyRunning(t)) => Ok(t),
        Err(floe_server::ops::StartError::UnknownOp) => Err("unknown op".into()),
    }
}
