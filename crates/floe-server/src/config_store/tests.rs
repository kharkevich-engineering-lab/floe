//! The config store on the in-memory store: CAS and monotonic revisions,
//! history with and without bucket versioning, rollback, sealed secrets,
//! redaction, fail-closed validation, and live apply.

use std::sync::Arc;

use floe_config::{Config, HistoryMode, Secret};
use floe_store::memory::MemoryStore;
use floe_store::{DynStore, ObjectStore, ObjectStoreExt, PutMode};
use serde_json::json;

use super::seal::SealKey;
use super::*;

const KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

fn store(versioned: bool) -> (Arc<MemoryStore>, DynStore) {
    let m = Arc::new(MemoryStore::new());
    m.versioning
        .store(versioned, std::sync::atomic::Ordering::Relaxed);
    let d: DynStore = m.clone();
    (m, d)
}

fn cs(store: &DynStore, mode: HistoryMode, key: bool) -> ConfigStore {
    ConfigStore::new(
        store.clone(),
        mode,
        key.then(|| SealKey::from_base64(KEY).unwrap()),
    )
}

fn bootstrap() -> Config {
    Config::default()
}

fn mirror_doc(token: serde_json::Value) -> serde_json::Value {
    json!({
        "github_mirror": {
            "enabled": true,
            "users": ["@me"],
            "private_visible_to_all_readers": true,
            "token": token,
        }
    })
}

async fn put(c: &ConfigStore, doc: &serde_json::Value, base: Option<u64>) -> Result<Published, PublishError> {
    c.publish(
        &PublishRequest {
            document: doc,
            author: "alice",
            message: "test",
            base_revision: base,
            rolled_back_from: None,
        },
        &bootstrap(),
    )
    .await
}

#[tokio::test]
async fn publish_is_cas_with_monotonic_revisions() {
    let (_, s) = store(false);
    let c = cs(&s, HistoryMode::Records, false);
    assert!(c.current().await.unwrap().is_none());
    let p1 = put(&c, &json!({}), Some(0)).await.unwrap();
    assert_eq!(p1.record.revision, 1);
    let p2 = put(&c, &json!({"events": {"sweep_interval": "1m"}}), Some(1)).await.unwrap();
    assert_eq!(p2.record.revision, 2);
    assert!(p2.record.diff.iter().any(|d| d.path == "events.sweep_interval" && d.op == DiffOp::Changed));
    assert_eq!(p2.restart_required, vec!["events.sweep_interval".to_string()]);
    // A stale editor: 409, nothing written.
    match put(&c, &json!({}), Some(1)).await {
        Err(PublishError::Conflict { current: 2 }) => {}
        other => panic!("expected a conflict, got {other:?}"),
    }
    // No base (the CLI): goes on top.
    assert_eq!(put(&c, &json!({}), None).await.unwrap().record.revision, 3);
    let (_, cur) = c.current().await.unwrap().unwrap();
    assert_eq!(cur.revision, 3);
    assert_eq!(cur.author, "alice");
}

#[tokio::test]
async fn invalid_documents_write_nothing() {
    let (m, s) = store(false);
    let c = cs(&s, HistoryMode::Records, false);
    let before = m.len();
    for bad in [
        json!({"server": {"listen": "0.0.0.0:1"}}),
        json!({"github_mirror": {"enabled": true, "include": ["nope"]}}),
        json!({"events": {"webhook_url": "ftp://x"}}),
        json!({"github_mirror": {"interval": "soon"}}),
    ] {
        match put(&c, &bad, None).await {
            Err(PublishError::Invalid(errors)) => assert!(!errors.is_empty()),
            other => panic!("{bad}: expected invalid, got {other:?}"),
        }
    }
    assert_eq!(m.len(), before, "nothing written");
    let Err(PublishError::Invalid(errors)) =
        put(&c, &json!({"github_mirror": {"enabled": true, "private_visible_to_all_readers": true, "include": ["nope"]}}), None).await
    else {
        panic!("expected invalid")
    };
    assert_eq!(errors[0].path.as_deref(), Some("github_mirror.include"), "{errors:?}");
}

#[tokio::test]
async fn secrets_are_sealed_redacted_and_kept() {
    let (m, s) = store(false);
    // Without a key a plain value is refused; an env reference is fine.
    let nokey = cs(&s, HistoryMode::Records, false);
    let Err(PublishError::Invalid(errors)) = put(&nokey, &mirror_doc(json!({"value": "ghp_x"})), None).await else {
        panic!("a value without a key must be refused")
    };
    assert_eq!(errors[0].path.as_deref(), Some("github_mirror.token"));
    put(&nokey, &mirror_doc(json!({"env": "MY_TOKEN"})), None).await.unwrap();

    let c = cs(&s, HistoryMode::Records, true);
    let p = put(&c, &mirror_doc(json!({"value": "ghp_secret"})), None).await.unwrap();
    let raw = String::from_utf8(m.get_bytes("current.json").await.unwrap().unwrap().1.to_vec()).unwrap();
    assert!(!raw.contains("ghp_secret"), "plain text at rest: {raw}");
    assert!(raw.contains("\"sealed\""));
    let token_change = p.record.diff.iter().find(|d| d.path == "github_mirror.token").unwrap();
    assert_eq!(token_change.new, Some(json!("(secret)")));
    assert_eq!(token_change.old, Some(json!({"env": "MY_TOKEN"})));
    // Reads are redacted.
    let red = redact(&p.record.document);
    assert_eq!(red["github_mirror"]["token"], json!({"redacted": true}));
    // Keep: a redacted placeholder carries the stored value; no diff.
    let p2 = put(&c, &mirror_doc(json!({"redacted": true})), None).await.unwrap();
    assert_eq!(p2.record.document["github_mirror"]["token"], p.record.document["github_mirror"]["token"]);
    assert!(p2.record.diff.iter().all(|d| d.path != "github_mirror.token"));
    let opened = c.open_document(&p2.record.document).unwrap();
    assert_eq!(opened.github_mirror.token, Secret::Value("ghp_secret".into()));
    // Another instance without the key cannot keep or apply it.
    assert!(nokey.open_document(&p2.record.document).is_err());
    let Err(PublishError::Invalid(errors)) = put(&nokey, &mirror_doc(json!({"redacted": true})), None).await else {
        panic!("keeping a sealed value needs the key")
    };
    assert!(errors[0].message.contains("FLOE_CONFIG_KEY"), "{errors:?}");
}

#[tokio::test]
async fn redacted_with_nothing_stored_is_refused() {
    let (_, s) = store(false);
    let c = cs(&s, HistoryMode::Records, true);
    let Err(PublishError::Invalid(errors)) = put(&c, &mirror_doc(json!({"redacted": true})), None).await else {
        panic!("nothing to keep")
    };
    assert_eq!(errors[0].path.as_deref(), Some("github_mirror.token"));
}

async fn history_and_rollback(versioned: bool, mode: HistoryMode) {
    let (m, s) = store(versioned);
    let c = cs(&s, mode, false);
    for i in 1..=3 {
        put(&c, &json!({"events": {"sweep_interval": format!("{i}m")}}), None).await.unwrap();
    }
    let h = c.history(None, 10).await.unwrap();
    assert_eq!(h.iter().map(|e| e.revision).collect::<Vec<_>>(), vec![3, 2, 1]);
    assert!(h.iter().all(|e| e.document.is_none()), "list form has no documents");
    let page = c.history(Some(3), 1).await.unwrap();
    assert_eq!(page[0].revision, 2);
    let r1 = c.revision(1).await.unwrap();
    assert_eq!(r1.document["events"]["sweep_interval"], "1m");
    let rb = c.rollback(1, "bob", "", Some(3), &bootstrap()).await.unwrap();
    assert_eq!(rb.record.revision, 4);
    assert_eq!(rb.record.rolled_back_from, Some(1));
    assert_eq!(rb.record.document["events"]["sweep_interval"], "1m");
    assert!(matches!(c.revision(9).await, Err(PublishError::NotFound(9))));
    let h1 = m.get_bytes(&history_key(1)).await.unwrap().unwrap().1;
    let entry: HistoryEntry = serde_json::from_slice(&h1).unwrap();
    match mode {
        HistoryMode::Versions => {
            assert!(entry.document.is_none() && entry.object_version.is_some(), "{entry:?}");
        }
        _ => assert!(entry.document.is_some() && entry.object_version.is_none(), "{entry:?}"),
    }
}

#[tokio::test]
async fn history_records_without_bucket_versioning() {
    history_and_rollback(false, HistoryMode::Records).await;
}

#[tokio::test]
async fn history_object_versions_with_bucket_versioning() {
    history_and_rollback(true, HistoryMode::Versions).await;
}

#[tokio::test]
async fn auto_history_follows_the_bucket() {
    for versioned in [false, true] {
        let (_, s) = store(versioned);
        let mode = resolve_history(HistoryMode::Auto, &s).await.unwrap();
        assert_eq!(mode, if versioned { HistoryMode::Versions } else { HistoryMode::Records });
    }
    let (_, s) = store(false);
    assert!(resolve_history(HistoryMode::Versions, &s).await.is_err(), "fail closed");
}

#[tokio::test]
async fn an_expired_object_version_is_gone() {
    let (m, s) = store(true);
    let c = cs(&s, HistoryMode::Versions, false);
    put(&c, &json!({}), None).await.unwrap();
    put(&c, &json!({}), None).await.unwrap();
    // The bucket's lifecycle dropped revision 1's body: simulate with a fresh
    // versioned store holding only the history index.
    let (_, s2) = store(true);
    let h1 = m.get_bytes(&history_key(1)).await.unwrap().unwrap().1;
    let cur = m.get_bytes(CURRENT).await.unwrap().unwrap().1;
    s2.put_bytes(&history_key(1), h1, PutMode::Create).await.unwrap();
    s2.put_bytes(CURRENT, cur, PutMode::Create).await.unwrap();
    let c2 = cs(&s2, HistoryMode::Versions, false);
    assert!(matches!(c2.revision(1).await, Err(PublishError::Gone(1))));
}

#[tokio::test]
async fn a_lost_history_write_is_healed_by_the_next_publish() {
    let (m, s) = store(false);
    let c = cs(&s, HistoryMode::Records, false);
    put(&c, &json!({}), None).await.unwrap();
    m.delete(&history_key(1), None).await.unwrap();
    put(&c, &json!({}), None).await.unwrap();
    assert!(m.get_bytes(&history_key(1)).await.unwrap().is_some());
    assert_eq!(c.revision(1).await.unwrap().revision, 1);
}

#[tokio::test]
async fn live_applies_new_revisions_and_reports_restarts() {
    let (_, s) = store(false);
    let c = Arc::new(cs(&s, HistoryMode::Records, false));
    let live = live::Live::start(Arc::new(bootstrap()), c.clone()).await;
    assert_eq!(live.current().revision, 0);
    assert!(!live.current().cfg.github_mirror.enabled);
    let mut rx = live.subscribe();
    put(&c, &mirror_doc(json!({"env": "FLOE_TEST_LIVE_TOKEN"})), None).await.unwrap();
    assert_eq!(live.revalidate().await, 1);
    assert!(rx.has_changed().unwrap());
    let applied = rx.borrow_and_update().clone();
    assert!(applied.cfg.github_mirror.enabled, "the mirror section applies live");
    assert!(live.status().restart_required.is_empty());
    put(&c, &json!({"events": {"webhook_url": "https://hooks.example/x"}}), None).await.unwrap();
    assert_eq!(live.revalidate().await, 2);
    assert_eq!(live.status().restart_required, vec!["events.webhook_url".to_string()]);
    // Unchanged: a conditional GET, nothing applied.
    assert_eq!(live.revalidate().await, 2);
    // A second instance starts on the current revision.
    let other = live::Live::start(Arc::new(bootstrap()), c.clone()).await;
    assert_eq!(other.current().revision, 2);
    assert_eq!(other.current().cfg.events.webhook_url.as_deref(), Some("https://hooks.example/x"));
}

#[tokio::test]
async fn a_document_this_instance_cannot_open_is_not_applied() {
    let (_, s) = store(false);
    let with_key = Arc::new(cs(&s, HistoryMode::Records, true));
    let without = Arc::new(cs(&s, HistoryMode::Records, false));
    let live = live::Live::start(Arc::new(bootstrap()), without.clone()).await;
    put(&with_key, &mirror_doc(json!({"value": "ghp_x"})), None).await.unwrap();
    assert_eq!(live.revalidate().await, 0, "kept the previous (no) revision");
    let err = live.status().apply_error.unwrap();
    assert!(err.contains("FLOE_CONFIG_KEY"), "{err}");
}

#[tokio::test]
async fn heartbeats_list_instances() {
    let (_, s) = store(false);
    let c = cs(&s, HistoryMode::Records, false);
    let mut st = InstanceStatus {
        instance: "host/1".into(),
        version: "v".into(),
        roles: vec!["all".into()],
        started_at: Utc::now(),
        seen_at: Utc::now(),
        applied_revision: 3,
        restart_required: vec![],
        apply_error: None,
    };
    c.heartbeat(&st).await.unwrap();
    st.instance = "old/2".into();
    st.seen_at = Utc::now() - chrono::TimeDelta::days(2);
    c.heartbeat(&st).await.unwrap();
    let got = c.instances().await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].instance, "host/1");
    assert_eq!(c.instances().await.unwrap().len(), 1, "the stale one was deleted");
}

#[test]
fn error_paths_are_extracted() {
    assert_eq!(error_path("github_mirror.include entry \"x\" must"), Some("github_mirror.include".into()));
    assert_eq!(error_path("catalog.max_buffer_rows (5) must be"), Some("catalog.max_buffer_rows".into()));
    assert_eq!(error_path("events.webhook_url must be"), Some("events.webhook_url".into()));
    assert_eq!(error_path("server.listen is bad"), None);
    assert_eq!(error_path("unknown field `x`"), None);
}
