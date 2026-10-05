//! Per-repo push policy: HTTP get/put/delete and receive-pack enforcement.
// Integration tests fail by panicking; clippy.toml's allow-*-in-tests only reaches #[test] fns,
// not the helpers around them, so the panic-path lints are lifted for the whole test crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "test code: a panic is how a test fails"
)]
mod harness;

type TestResult = anyhow::Result<()>;
use anyhow::Context;
use harness::{Server, TestRepo, git_in};
use std::process::Command;

const PROTECT_MAIN: &str = r#"{
  "version": 1,
  "rules": [
    {
      "name": "lock-main",
      "match": { "refs": ["refs/heads/main"] },
      "effect": {
        "protect": { "restricts": ["delete", "force-push"] }
      }
    }
  ]
}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_http_roundtrip() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let client = reqwest::Client::new();
    let url = format!("{}/t/r/policy", server.base_url);

    let empty = client.get(&url).send().await?;
    assert_eq!(empty.status(), 200);
    let body: serde_json::Value = empty.json().await?;
    assert_eq!(body["rules"].as_array().unwrap().len(), 0);

    let put = client
        .put(&url)
        .header("content-type", "application/json")
        .body(PROTECT_MAIN)
        .send()
        .await?;
    assert_eq!(
        put.status(),
        204,
        "{}",
        put.text().await.unwrap_or_default()
    );

    let got = client.get(&url).send().await?;
    let body: serde_json::Value = got.json().await?;
    assert_eq!(body["rules"][0]["name"], "lock-main");
    assert_eq!(body["rules"][0]["match"]["refs"][0], "refs/heads/main");

    let del = client.delete(&url).send().await?;
    assert_eq!(del.status(), 204);
    let empty = client.get(&url).send().await?;
    let body: serde_json::Value = empty.json().await?;
    assert_eq!(body["rules"].as_array().unwrap().len(), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_missing_repo_is_404() -> TestResult {
    let server = Server::start().await?;
    let status = reqwest::Client::new()
        .get(format!("{}/no/such/policy", server.base_url))
        .send()
        .await?
        .status();
    assert_eq!(status, 404);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protected_main_rejects_force_and_delete() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;
    let put = reqwest::Client::new()
        .put(format!("{}/t/r/policy", server.base_url))
        .header("content-type", "application/json")
        .body(PROTECT_MAIN)
        .send()
        .await?;
    assert_eq!(put.status(), 204);

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    // Unrelated history + --force: policy, not CAS, must reject.
    git_in(&src, &["checkout", "--orphan", "other"])?;
    git_in(&src, &["commit", "--allow-empty", "-m", "other"])?;
    let force = Command::new("git")
        .current_dir(&*src)
        .args(["push", "--force", "origin", "other:main"])
        .output()?;
    let stderr = String::from_utf8_lossy(&force.stderr);
    assert!(
        !force.status.success(),
        "force-push of protected main succeeded: {stderr}"
    );
    assert!(
        stderr.contains("lock-main") || stderr.contains("rejected by rule"),
        "stderr should name the rule: {stderr}"
    );

    let del = Command::new("git")
        .current_dir(&*src)
        .args(["push", "origin", ":refs/heads/main"])
        .output()?;
    let stderr = String::from_utf8_lossy(&del.stderr);
    assert!(!del.status.success(), "delete of protected main succeeded");
    assert!(
        stderr.contains("lock-main") || stderr.contains("rejected by rule"),
        "stderr should name the rule: {stderr}"
    );

    // Unprotected branch may be force-pushed (orphan onto a new name).
    git_in(&src, &["checkout", "--orphan", "topic"])?;
    git_in(&src, &["commit", "--allow-empty", "-m", "topic"])?;
    git_in(&src, &["push", "origin", "topic"])?;
    git_in(&src, &["checkout", "--orphan", "topic2"])?;
    git_in(&src, &["commit", "--allow-empty", "-m", "topic-other"])?;
    git_in(&src, &["push", "--force", "origin", "topic2:topic"])
        .context("force-push of unprotected topic")?;

    // After clearing policy, force-push of main is allowed.
    let cleared = reqwest::Client::new()
        .delete(format!("{}/t/r/policy", server.base_url))
        .send()
        .await?;
    assert_eq!(cleared.status(), 204);
    git_in(&src, &["push", "--force", "origin", "other:main"])?;
    Ok(())
}

/// D48: `refs/archive/*` is upstream follow's; with no policy file at all a
/// pusher can neither forge nor delete an archive name (built-in rule
/// `archive-immutable`), while every other ref stays allow-all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_refs_are_immutable_without_a_policy() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    let forge = Command::new("git")
        .current_dir(&*src)
        .args(["push", "origin", "main:refs/archive/1791072000/refs/heads/main"])
        .output()?;
    let stderr = String::from_utf8_lossy(&forge.stderr);
    assert!(!forge.status.success(), "creating an archive ref succeeded: {stderr}");
    assert!(
        stderr.contains("archive-immutable"),
        "stderr should name the built-in rule: {stderr}"
    );
    let ls = Command::new("git")
        .current_dir(&*src)
        .args(["ls-remote", "origin", "refs/archive/*"])
        .output()?;
    assert!(ls.status.success());
    assert!(ls.stdout.is_empty(), "nothing was published under refs/archive/");

    // Everything else is still allow-all.
    git_in(&src, &["push", "origin", "main:refs/heads/topic"])?;
    git_in(&src, &["push", "origin", ":refs/heads/topic"])?;
    Ok(())
}

/// Per-repo policy and settings writes from an editor are conditional: a
/// document another admin changed meanwhile is a 409, never overwritten.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_and_settings_writes_are_conditional() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;
    let client = reqwest::Client::new();
    let url = format!("{}/t/r/policy", server.base_url);

    let got = client.get(&url).send().await?;
    let etag = got.headers().get("etag").unwrap().to_str()?.to_string();
    assert_eq!(etag, "\"none\"");
    let put = |if_match: String| {
        client
            .put(&url)
            .header("content-type", "application/json")
            .header("if-match", if_match)
            .body(PROTECT_MAIN)
            .send()
    };
    assert_eq!(put(etag.clone()).await?.status(), 204);
    // The same (now stale) ETag again: someone else's write in between.
    assert_eq!(put(etag).await?.status(), 409);
    let fresh = client.get(&url).send().await?;
    let etag = fresh.headers().get("etag").unwrap().to_str()?.to_string();
    assert_ne!(etag, "\"none\"");
    assert_eq!(put(etag).await?.status(), 204);

    let settings = format!("{}/t/r/settings", server.base_url);
    let put_settings = |base: u64| {
        client
            .put(format!("{settings}?base_revision={base}"))
            .header("content-type", "application/toml")
            .body("[compaction]\nenabled = false\n")
            .send()
    };
    let first = put_settings(0).await?;
    assert_eq!(first.status(), 200, "{}", first.text().await?);
    assert_eq!(put_settings(0).await?.status(), 409, "stale base revision");
    assert_eq!(put_settings(1).await?.status(), 200);
    Ok(())
}
