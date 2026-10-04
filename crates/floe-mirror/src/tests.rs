//! Passes end to end over a real [`floe_wal::Registry`] on a [`MemoryStore`]
//! (no server, no network): `FakeSource` scripts the forge.

use std::sync::Arc;
use std::time::Duration;

use floe_git::RepoId;
use floe_store::memory::MemoryStore;
use floe_store::{DynStore, ObjectStoreExt};
use floe_wal::Registry;
use parking_lot::Mutex;

use super::*;
use crate::fake::{FakeSource, remote};

struct Rig {
    _cache: tempfile::TempDir,
    store: DynStore,
    registry: Arc<Registry>,
    source: Arc<FakeSource>,
    nudged: Arc<Mutex<Vec<String>>>,
    mirror: Mirror,
}

fn rig_with(edit: impl FnOnce(&mut Config)) -> Rig {
    let cache = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.cache.dir = cache.path().to_path_buf();
    cfg.store.bucket = "test".into();
    cfg.wal.freshness_ttl = Duration::ZERO;
    cfg.wal.snapshot_every_entries = 0;
    cfg.wal.checkpoint_interval = Duration::ZERO;
    cfg.wal.checkpoint_tail_bytes = floe_config::ByteSize::b(0);
    cfg.github_mirror.users = vec!["@me".into()];
    edit(&mut cfg);
    let cfg = Arc::new(cfg);
    let store: DynStore = MemoryStore::shared();
    let registry = Registry::new(store.clone(), cfg.clone());
    let source = Arc::new(FakeSource::new("https://github.com"));
    let nudged = Arc::new(Mutex::new(Vec::new()));
    let n = nudged.clone();
    let target = WalTarget::new(
        registry.clone(),
        store.clone(),
        Box::new(move |id: &RepoId| n.lock().push(id.to_string())),
    );
    let mirror = Mirror {
        cfg,
        source: source.clone(),
        target: Arc::new(target),
        store: store.clone(),
    };
    Rig {
        _cache: cache,
        store,
        registry,
        source,
        nudged,
        mirror,
    }
}

fn rig() -> Rig {
    rig_with(|_| {})
}

impl Rig {
    async fn pass(&self) -> PassReport {
        reconcile_once(&self.mirror, PassOptions::default(), None)
            .await
            .unwrap()
    }

    async fn state(&self) -> MirrorState {
        state::load(self.store.as_ref(), "github").await.unwrap()
    }

    async fn settings(&self, floe: &str) -> floe_proto::v1::RepoSettings {
        let h = self.registry.open(&naming::parse(floe).unwrap()).await.unwrap();
        drop(h.sync_refs_only().await.unwrap());
        h.settings().unwrap_or_default()
    }

    async fn upstream(&self, floe: &str) -> toml::Table {
        settings::upstream_of(&self.settings(floe).await.toml)
    }
}

fn s(t: &toml::Table, k: &str) -> String {
    t.get(k).and_then(toml::Value::as_str).unwrap_or_default().to_string()
}

#[tokio::test]
async fn reconcile_creates_repos_with_settings_and_policy_then_is_idempotent() {
    let r = rig();
    let mut w = remote("42", "Acme", "Widgets");
    w.private = true;
    r.source.set_repos(vec![w, remote("7", "acme", ".github")], true);
    let rep = r.pass().await;
    assert_eq!(rep.outcome, "ok");
    assert_eq!(rep.created, 2, "{rep:?}");
    assert_eq!(rep.published, 2);
    assert_eq!(rep.errors, 0, "{rep:?}");

    let up = r.upstream("gh-acme/widgets").await;
    assert_eq!(s(&up, "source"), "github:42");
    assert_eq!(s(&up, "git"), "https://github.com/Acme/Widgets.git");
    assert_eq!(s(&up, "lfs"), "https://github.com/Acme/Widgets.git/info/lfs");
    assert_eq!(s(&up, "head"), "refs/heads/main");
    assert_eq!(r.settings("gh-acme/widgets").await.author, settings::AUTHOR);
    assert_eq!(s(&r.upstream("gh-acme/_.github").await, "source"), "github:7");

    // The read-only policy, exactly the published constant.
    let (_, policy) = r
        .store
        .get_bytes(&floe_proto::keys::policy_key("gh-acme", "widgets"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(policy.as_ref(), target::READ_ONLY_POLICY.as_bytes());

    let st = r.state().await;
    let e = st.repos.get("42").unwrap();
    assert_eq!(e.floe.as_deref(), Some("gh-acme/widgets"));
    assert_eq!(e.status, Status::Active);
    assert_eq!(e.settings_revision, 1);
    assert_eq!(st.token_login.as_deref(), Some("me"));
    let mut nudged = r.nudged.lock().clone();
    nudged.sort();
    assert_eq!(nudged, ["gh-acme/_.github", "gh-acme/widgets"]);

    // Second pass: nothing to do, nothing written but the bookkeeping.
    let rep = r.pass().await;
    assert!(rep.plan.is_empty(), "{:?}", rep.plan);
    assert_eq!((rep.created, rep.published, rep.nudged), (0, 0, 0));
    let st2 = r.state().await;
    assert_eq!(st2.repos.get("42").unwrap().settings_sha, e.settings_sha);
    assert_eq!(r.settings("gh-acme/widgets").await.revision, 1);
}

#[tokio::test]
async fn dry_run_writes_nothing() {
    let r = rig();
    r.source.set_repos(vec![remote("1", "a", "b")], true);
    let rep = reconcile_once(&r.mirror, PassOptions { dry_run: true }, None)
        .await
        .unwrap();
    assert_eq!(
        rep.plan.first().map(String::as_str),
        Some("create gh-a/b ← a/b (public, 0.0 MiB)")
    );
    assert!(r.registry.list().await.unwrap().is_empty());
    assert!(r.state().await.repos.is_empty());
}

#[tokio::test]
async fn crash_between_create_and_settings_is_repaired() {
    let r = rig();
    // The create landed, the state CAS did not.
    let id = RepoId::new("gh-acme", "widgets").unwrap();
    r.registry.create(&id, floe_git::ObjectFormat::Sha1).await.unwrap();
    r.source.set_repos(vec![remote("42", "Acme", "Widgets")], true);
    let rep = r.pass().await;
    assert_eq!(rep.errors, 0, "{rep:?}");
    assert_eq!(rep.created, 0, "adopted, not created");
    assert_eq!(s(&r.upstream("gh-acme/widgets").await, "source"), "github:42");
    assert_eq!(
        r.state().await.repos.get("42").unwrap().floe.as_deref(),
        Some("gh-acme/widgets")
    );
}

#[tokio::test]
async fn crash_after_publish_before_state_cas_is_not_detached() {
    let r = rig();
    let w = remote("42", "Acme", "Widgets");
    r.source.set_repos(vec![w.clone()], true);
    r.pass().await;
    let before = r.state().await;
    // A rename publish lands, then the state CAS is lost: restore the old state.
    let mut renamed = w;
    renamed.name = "Gadgets".into();
    r.source.set_repos(vec![renamed], true);
    r.pass().await;
    let mut lost = before.clone();
    lost.generation = r.state().await.generation;
    state::save(r.store.as_ref(), "github", &mut lost, "test").await.unwrap();
    let rep = r.pass().await;
    let e = r.state().await.repos.get("42").cloned().unwrap();
    assert_eq!(e.status, Status::Active, "{e:?}");
    assert_eq!(rep.published, 0, "re-adopted, nothing republished");
    assert_eq!(r.settings("gh-acme/widgets").await.revision, 2);
}

#[tokio::test]
async fn rename_keeps_floe_name_and_updates_url() {
    let r = rig();
    let w = remote("42", "Acme", "Widgets");
    r.source.set_repos(vec![w.clone()], true);
    r.pass().await;
    let mut renamed = w;
    renamed.owner = "NewCo".into();
    renamed.name = "Gizmos".into();
    r.source.set_repos(vec![renamed], true);
    let rep = r.pass().await;
    assert_eq!(rep.published, 1);
    let up = r.upstream("gh-acme/widgets").await;
    assert_eq!(s(&up, "git"), "https://github.com/NewCo/Gizmos.git");
    let st = r.state().await;
    let e = st.repos.get("42").unwrap();
    assert_eq!(e.full_name, "NewCo/Gizmos");
    assert_eq!(e.floe.as_deref(), Some("gh-acme/widgets"));
    assert!(r.registry.open(&RepoId::new("gh-newco", "gizmos").unwrap()).await.is_err());
}

#[tokio::test]
async fn freeze_publishes_empty_follow_and_resume_restores_it() {
    let r = rig_with(|c| c.github_mirror.gone_after = Duration::ZERO);
    r.source.set_repos(vec![remote("42", "Acme", "Widgets")], true);
    r.pass().await;
    // Deleted at the source.
    r.source.set_repos(vec![], true);
    r.source.set_lookup("42", Lookup::Gone);
    r.pass().await;
    let st = r.state().await;
    assert_eq!(st.repos.get("42").unwrap().status, Status::Gone);
    let up = r.upstream("gh-acme/widgets").await;
    assert_eq!(up.get("follow"), Some(&toml::Value::Array(vec![])));
    assert!(r.registry.open(&RepoId::new("gh-acme", "widgets").unwrap()).await.is_ok());
    // Restored.
    r.nudged.lock().clear();
    r.source.set_repos(vec![remote("42", "Acme", "Widgets")], true);
    let rep = r.pass().await;
    assert_eq!(rep.published, 1);
    let up = r.upstream("gh-acme/widgets").await;
    assert_eq!(up.get("follow").and_then(|v| v.as_array()).map(Vec::len), Some(2));
    assert_eq!(r.nudged.lock().as_slice(), ["gh-acme/widgets"]);
}

#[tokio::test]
async fn existing_unmanaged_repo_is_never_touched() {
    let r = rig();
    // A human's repository already holds the plain name, with settings.
    let id = RepoId::new("gh-acme", "widgets").unwrap();
    let h = r.registry.create(&id, floe_git::ObjectFormat::Sha1).await.unwrap();
    h.publish_settings("[bundles]\nmain_only = true\n", "alice", "mine")
        .await
        .unwrap();
    r.source.set_repos(vec![remote("42", "Acme", "Widgets")], true);
    let rep = r.pass().await;
    assert_eq!(rep.errors, 0, "{rep:?}");
    assert_eq!(r.settings("gh-acme/widgets").await.author, "alice");
    assert_eq!(
        r.state().await.repos.get("42").unwrap().floe.as_deref(),
        Some("gh-acme/widgets--42")
    );
    assert_eq!(s(&r.upstream("gh-acme/widgets--42").await, "source"), "github:42");
}

#[tokio::test]
async fn human_edit_of_upstream_detaches() {
    let r = rig();
    let w = remote("42", "Acme", "Widgets");
    r.source.set_repos(vec![w.clone()], true);
    r.pass().await;
    let h = r.registry.open(&RepoId::new("gh-acme", "widgets").unwrap()).await.unwrap();
    h.publish_settings(
        "[upstream]\ngit = \"https://example.com/fork.git\"\nfollow = [\"refs/heads/main\"]\n",
        "alice",
        "my fork",
    )
    .await
    .unwrap();
    let mut branch = w;
    branch.default_branch = Some("trunk".into());
    r.source.set_repos(vec![branch], true);
    let rep = r.pass().await;
    assert_eq!(rep.published, 0);
    assert_eq!(r.state().await.repos.get("42").unwrap().status, Status::Detached);
    assert_eq!(r.settings("gh-acme/widgets").await.author, "alice");
    // And stays so.
    let rep = r.pass().await;
    assert!(rep.plan.is_empty());
    // An operator editing only another section keeps it managed.
    let r2 = rig();
    r2.source.set_repos(vec![remote("1", "a", "b")], true);
    r2.pass().await;
    let h = r2.registry.open(&RepoId::new("gh-a", "b").unwrap()).await.unwrap();
    let cur = h.settings().unwrap_or_default().toml;
    h.publish_settings(&format!("{cur}\n[bundles]\nmain_only = true\n"), "alice", "bundles")
        .await
        .unwrap();
    let mut moved = remote("1", "a", "b");
    moved.default_branch = Some("trunk".into());
    r2.source.set_repos(vec![moved], true);
    let rep = r2.pass().await;
    assert_eq!(rep.published, 1, "{rep:?}");
    let st = r2.settings("gh-a/b").await;
    assert!(st.toml.contains("main_only = true"), "{}", st.toml);
    assert!(st.toml.contains("refs/heads/trunk"), "{}", st.toml);
}

#[tokio::test]
async fn too_large_handoff_is_adopted() {
    let r = rig();
    let mut big = remote("9", "Acme", "Mono");
    big.size_kb = 3 * 1024 * 1024;
    r.source.set_repos(vec![big], true);
    let rep = r.pass().await;
    assert_eq!(rep.created, 0);
    assert!(r.registry.list().await.unwrap().is_empty());
    assert_eq!(r.state().await.repos.get("9").unwrap().status, Status::TooLarge);
    // The operator imports it and sets the marker (the logged recipe).
    let id = RepoId::new("gh-acme", "mono").unwrap();
    let h = r.registry.create(&id, floe_git::ObjectFormat::Sha1).await.unwrap();
    h.publish_settings("[upstream]\nsource = \"github:9\"\n", "ops", "handoff")
        .await
        .unwrap();
    let rep = r.pass().await;
    assert_eq!(rep.errors, 0, "{rep:?}");
    assert_eq!(rep.published, 1);
    let e = r.state().await.repos.get("9").cloned().unwrap();
    assert_eq!(e.status, Status::Active);
    assert_eq!(e.floe.as_deref(), Some("gh-acme/mono"));
    assert_eq!(s(&r.upstream("gh-acme/mono").await, "git"), "https://github.com/Acme/Mono.git");
}

#[tokio::test]
async fn max_new_per_pass_bounds_creations() {
    let r = rig_with(|c| c.github_mirror.max_new_per_pass = 2);
    r.source.set_repos(
        (1..=5).map(|i| remote(&i.to_string(), "a", &format!("r{i}"))).collect(),
        true,
    );
    assert_eq!(r.pass().await.created, 2);
    assert_eq!(r.pass().await.created, 2);
    assert_eq!(r.pass().await.created, 1);
    assert_eq!(r.registry.list().await.unwrap().len(), 5);
}

#[tokio::test]
async fn unauthorized_pass_fails_and_changes_nothing() {
    let r = rig();
    r.source.fail("401");
    assert!(matches!(
        reconcile_once(&r.mirror, PassOptions::default(), None).await,
        Err(PassError::Source(SourceError::Unauthorized))
    ));
    assert!(r.state().await.repos.is_empty());
}

#[tokio::test]
async fn a_held_lease_keeps_a_second_reconciler_out() {
    let r = rig_with(|c| c.github_mirror.lease_ttl = Duration::from_millis(300));
    r.source.set_repos(vec![remote("1", "a", "b")], true);
    let other = coord::try_acquire(
        r.store.clone(),
        &lease_key("github"),
        "someone-else",
        "github-mirror",
        Duration::from_mins(1),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(run_once_leased(&r.mirror).await.unwrap().is_none());
    assert!(r.state().await.repos.is_empty());
    assert_eq!(
        lease_holder(&r.store, "github").await.map(|(h, _)| h).as_deref(),
        Some("someone-else")
    );
    other.release().await.unwrap();
    let rep = run_once_leased(&r.mirror).await.unwrap().unwrap();
    assert_eq!(rep.created, 1);
    assert_eq!(r.state().await.holder, coord::instance_id());
    // Released after the pass.
    assert!(lease_holder(&r.store, "github").await.is_none());
}

#[tokio::test]
async fn a_lost_lease_stops_the_pass_before_its_next_step() {
    let r = rig();
    r.source.set_repos(vec![remote("1", "a", "b")], true);
    let lost = AtomicBool::new(true);
    assert!(matches!(
        reconcile_once(&r.mirror, PassOptions::default(), Some(&lost)).await,
        Err(PassError::LeaseLost)
    ));
    assert!(r.registry.list().await.unwrap().is_empty());
}
