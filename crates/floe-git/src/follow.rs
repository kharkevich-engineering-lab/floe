//! Fetch the delta that brings refs of a serving copy up to the same refs on an
//! upstream git host, as one self-contained pack ready for `LocalRepo::ingest_pack`
//! — the maintainer's `follow` unit (`[upstream] follow`).
//!
//! A **scratch bare repository per followed repository** (`<dir>/<owner>/<name>.git`,
//! kept between rounds) whose `objects/info/alternates` is the serving copy's object
//! directory. Before each fetch its `refs/follow/*` are set to exactly the values the
//! WAL has for the refs matching the follow patterns (D48, `floe_config::refpattern`),
//! so `git fetch --prune` negotiates from exactly our tips and leaves exactly
//! upstream's matching set behind; `index-pack --fix-thin` completes the thin pack from
//! our own objects, and the pack on disk is self-contained. [`probe`] (one `ls-remote`)
//! tells the caller whether a fetch is needed at all. Nothing is written into the serving copy
//! here: the caller streams the pack through `ingest_pack` like any push and then
//! calls [`FetchedDelta::discard_pack`]. Token (optional) goes through a one-shot
//! credential helper that reads it from the environment — never argv.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use floe_config::refpattern::{RefPatterns, scratch_ref};

use crate::GitError;

pub struct FetchedDelta {
    /// The scratch repository (persistent; see module docs).
    pub dir: PathBuf,
    /// `ref → oid` as fetched (upstream's current tips for the asked refs).
    pub tips: HashMap<String, String>,
    /// The pack `git fetch` wrote, when anything was fetched (self-contained).
    pub pack: Option<PathBuf>,
}

impl FetchedDelta {
    /// Remove the fetched pack (+ index) from the scratch once its objects are in
    /// the serving copy; the scratch's refs stay as negotiation tips.
    pub async fn discard_pack(&self) {
        if let Some(p) = &self.pack {
            let _ = tokio::fs::remove_file(p).await;
            for ext in ["idx", "rev", "keep", "promisor", "mtimes"] {
                let _ = tokio::fs::remove_file(p.with_extension(ext)).await;
            }
        }
    }
}

/// What upstream advertises right now, filtered by the follow patterns
/// ([`probe`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    /// `ref → oid` of every advertised ref that `patterns.matches`.
    pub tips: HashMap<String, String>,
    /// Whether upstream advertised **any** ref at all, before filtering (the
    /// empty-advertisement guard: an upstream that suddenly has nothing is an
    /// outage or a wiped repository, not a mass delete).
    pub advertised_any: bool,
}

/// One `git ls-remote <upstream>` (the same single `ls-refs` round trip a fetch's
/// advertisement costs; no objects, no scratch), filtered by `patterns` in floe:
/// `ls-remote`'s own patterns are tail matches, not refspecs.
pub async fn probe(
    upstream: &str,
    token: Option<&str>,
    patterns: &RefPatterns,
) -> Result<Probe, GitError> {
    let out = git_cmd(None, token)
        .args(["-c", "protocol.version=2", "ls-remote", upstream])
        .output()
        .await
        .map_err(GitError::Io)?;
    let out = ok(out, "git ls-remote <upstream>")?;
    let mut probe = Probe::default();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((oid, name)) = line.split_once('\t') else {
            continue;
        };
        if name.ends_with("^{}") {
            continue; // peeled tag line: the tag object's oid is what we follow
        }
        probe.advertised_any = true;
        if patterns.matches(name) {
            probe.tips.insert(name.to_string(), oid.to_string());
        }
    }
    Ok(probe)
}

/// Fetch the refs matching `patterns` from `upstream` into the scratch, negotiating
/// from `have` (every WAL ref matching `patterns`, `ref → oid`). Exact entries the
/// fresh `probe` did not see are left out of the refspecs (git fails the whole fetch
/// on a missing exact ref) and are absent from the result's tips: a deletion.
/// After the fetch (`--prune`) the scratch's `refs/follow/*` is exactly upstream's
/// matching set, so `have − tips` is what upstream deleted.
pub async fn fetch_refs(
    upstream: &str,
    token: Option<&str>,
    serving_objects: &Path,
    have: &HashMap<String, String>,
    patterns: &RefPatterns,
    probe: &Probe,
    scratch: &Path,
) -> Result<FetchedDelta, GitError> {
    let git = |args: &[&str]| {
        let mut c = git_cmd(Some(scratch), token);
        c.args(args);
        c
    };

    // Scratch: create once; its alternates are the serving copy's objects.
    if !scratch.join("HEAD").exists() {
        tokio::fs::create_dir_all(scratch)
            .await
            .map_err(GitError::Io)?;
        ok(
            git(&["init", "-q", "--bare"])
                .output()
                .await
                .map_err(GitError::Io)?,
            "git init",
        )?;
    }
    // Absolute: a relative alternates line is resolved against the scratch's objects dir.
    let serving_objects = std::path::absolute(serving_objects).map_err(GitError::Io)?;
    tokio::fs::create_dir_all(scratch.join("objects/info"))
        .await
        .map_err(GitError::Io)?;
    tokio::fs::write(
        scratch.join("objects/info/alternates"),
        format!("{}\n", serving_objects.display()),
    )
    .await
    .map_err(GitError::Io)?;
    // Leftover packs from a round whose ingest/publish failed: the objects are
    // fetched again (the WAL never saw them), so they are garbage here.
    let pack_dir = scratch.join("objects/pack");
    if let Ok(mut rd) = tokio::fs::read_dir(&pack_dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
    let absent: Vec<&str> = patterns
        .exact()
        .filter(|r| !probe.tips.contains_key(*r))
        .collect();
    // Negotiation tips = exactly the WAL's current values of the matching refs
    // (minus exact refs upstream no longer has): every other `refs/follow/*` goes,
    // since with globs the set changes from round to round.
    {
        use std::fmt::Write as _;
        let mut input = String::new();
        for name in read_scratch(scratch).await?.tips.keys() {
            if !have.contains_key(name) || absent.contains(&name.as_str()) {
                let _ = writeln!(input, "delete {}", scratch_ref(name));
            }
        }
        for (name, oid) in have {
            if !absent.contains(&name.as_str()) {
                let _ = writeln!(input, "update {} {oid}", scratch_ref(name));
            }
        }
        let mut child = git(&["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(GitError::Io)?;
        {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| GitError::InvalidInput("update-ref: no stdin pipe".into()))?;
            stdin
                .write_all(input.as_bytes())
                .await
                .map_err(GitError::Io)?;
        }
        ok(
            child.wait_with_output().await.map_err(GitError::Io)?,
            "git update-ref --stdin",
        )?;
    }

    let refspecs = patterns.refspecs(&absent);
    if refspecs.is_empty() {
        // Every positive entry is an exact ref upstream deleted: nothing to fetch.
        return read_scratch(scratch).await;
    }
    let mut args: Vec<&str> = vec![
        // Always a pack, never loose objects (ingest_pack takes a pack).
        "-c",
        "fetch.unpackLimit=1",
        "-c",
        "transfer.unpackLimit=1",
        "-c",
        "fetch.writeCommitGraph=false",
        "-c",
        "gc.auto=0",
        "-c",
        "protocol.version=2",
        "fetch",
        "--no-tags",
        "--prune",
        "--no-write-fetch-head",
        "--no-auto-gc",
        "--quiet",
        upstream,
    ];
    args.extend(refspecs.iter().map(String::as_str));
    ok(
        git(&args).output().await.map_err(GitError::Io)?,
        "git fetch <upstream> <refspecs>",
    )?;
    read_scratch(scratch).await
}

/// `git` with `--git-dir` (when given), no terminal prompt, and the token (when
/// given) through a one-shot credential helper that reads it from the environment:
/// it appears neither on a command line nor in a config file.
fn git_cmd(git_dir: Option<&Path>, token: Option<&str>) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("git");
    if let Some(d) = git_dir {
        c.arg("--git-dir").arg(d);
    }
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(t) = token {
        c.env("FLOE_UPSTREAM_TOKEN", t)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env(
                "GIT_CONFIG_VALUE_0",
                "!f() { echo username=x-access-token; echo \"password=$FLOE_UPSTREAM_TOKEN\"; }; f",
            );
    }
    c
}

fn ok(out: std::process::Output, what: &str) -> Result<std::process::Output, GitError> {
    if out.status.success() {
        Ok(out)
    } else {
        Err(GitError::Subprocess {
            cmd: what.to_string(),
            status: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// What a previous [`fetch_refs`] left in the scratch: upstream's tips
/// (`refs/follow/*`) and the pack it wrote, if any.
pub async fn read_scratch(scratch: &Path) -> Result<FetchedDelta, GitError> {
    let out = tokio::process::Command::new("git")
        .arg("--git-dir")
        .arg(scratch)
        .args([
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/follow/",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(GitError::Io)?;
    if !out.status.success() {
        return Err(GitError::Subprocess {
            cmd: "git for-each-ref".into(),
            status: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    let mut tips = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((oid, name)) = line.split_once(' ')
            && let Some(r) = name.strip_prefix("refs/follow/")
        {
            tips.insert(format!("refs/{r}"), oid.to_string());
        }
    }
    // The pack git wrote (one per fetch; none when nothing moved).
    let mut pack = None;
    if let Ok(mut rd) = tokio::fs::read_dir(scratch.join("objects/pack")).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "pack") {
                pack = Some(p);
            }
        }
    }
    Ok(FetchedDelta {
        dir: scratch.to_path_buf(),
        tips,
        pack,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn pats(list: &[&str]) -> RefPatterns {
        RefPatterns::parse(&list.iter().map(|s| (*s).to_string()).collect::<Vec<_>>()).unwrap()
    }

    /// An upstream work repository with one commit on `main`; returns (dir, url, oid).
    fn upstream() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a"), "a").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "a"]);
        let oid = run(dir.path(), &["rev-parse", "HEAD"]);
        let url = format!("file://{}", dir.path().display());
        (dir, url, oid)
    }

    /// A serving copy (empty bare repository) and a scratch path next to it.
    fn serving() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("serving.git");
        run(dir.path(), &["init", "-q", "--bare", "serving.git"]);
        let scratch = dir.path().join("scratch.git");
        (dir, bare.join("objects"), scratch)
    }

    #[test]
    fn ref_patterns_agree_with_git() {
        let corpus = [
            "refs/heads/main",
            "refs/heads/*",
            "refs/heads/feat-*",
            "refs/*/main",
            "refs/tags/v1.2.0",
            "refs/heads/a..b",
            "refs/heads/a b",
            "refs/heads/a:b",
            "refs/heads/a?b",
            "refs/heads/a[b",
            "refs/heads/a~b",
            "refs/heads/a\\b",
            "refs/heads/x.lock",
            "refs/heads/x.lock/y",
            "refs/heads/.x",
            "refs/heads/x/.y",
            "refs/heads/",
            "refs/heads//x",
            "refs/heads/a@{1}",
            "refs/heads/a@b",
            "refs/heads/x.",
            "refs/heads/\u{1}",
            "refs/heads/ünï",
        ];
        for name in corpus {
            let git = std::process::Command::new("git")
                .args(["check-ref-format", "--refspec-pattern", name])
                .status()
                .unwrap()
                .success();
            let ours = floe_config::refpattern::check_refspec_pattern(name).is_ok();
            assert_eq!(ours, git, "{name:?}: ours {ours}, git {git}");
        }
    }

    #[tokio::test]
    async fn probe_reports_matching_tips_and_advertised_any() {
        let (up, url, main) = upstream();
        run(up.path(), &["tag", "v1"]);
        let p = probe(&url, None, &pats(&["refs/heads/*"])).await.unwrap();
        assert!(p.advertised_any);
        assert_eq!(
            p.tips,
            HashMap::from([("refs/heads/main".to_string(), main)])
        );
        // Nothing matches, but upstream advertised something: not "empty".
        let p = probe(&url, None, &pats(&["refs/heads/nope"]))
            .await
            .unwrap();
        assert!(p.advertised_any && p.tips.is_empty());
        // An upstream with no refs at all.
        let empty = tempfile::tempdir().unwrap();
        run(empty.path(), &["init", "-q", "--bare"]);
        let p = probe(
            &format!("file://{}", empty.path().display()),
            None,
            &pats(&["refs/heads/*"]),
        )
        .await
        .unwrap();
        assert!(!p.advertised_any && p.tips.is_empty());
    }

    #[tokio::test]
    async fn fetch_with_globs_reports_new_and_pruned_refs() {
        let (up, url, main) = upstream();
        run(up.path(), &["branch", "feat/x"]);
        let (_s, objects, scratch) = serving();
        let pat = pats(&["refs/heads/*"]);
        let p = probe(&url, None, &pat).await.unwrap();
        let d = fetch_refs(&url, None, &objects, &HashMap::new(), &pat, &p, &scratch)
            .await
            .unwrap();
        assert_eq!(d.tips.len(), 2, "{:?}", d.tips);
        assert!(d.pack.is_some());
        // Install the objects in the serving copy as ingest would (alternates
        // make the scratch's pack visible only to the scratch).
        let pack_dir = objects.join("pack");
        for e in std::fs::read_dir(scratch.join("objects/pack")).unwrap() {
            let e = e.unwrap();
            std::fs::copy(e.path(), pack_dir.join(e.file_name())).unwrap();
        }
        d.discard_pack().await;
        let have = d.tips.clone();

        // Upstream: feat/x deleted, feat/y created.
        run(up.path(), &["branch", "-D", "feat/x"]);
        run(up.path(), &["branch", "feat/y"]);
        let p = probe(&url, None, &pat).await.unwrap();
        let d = fetch_refs(&url, None, &objects, &have, &pat, &p, &scratch)
            .await
            .unwrap();
        assert_eq!(
            d.tips,
            HashMap::from([
                ("refs/heads/main".to_string(), main.clone()),
                ("refs/heads/feat/y".to_string(), main),
            ])
        );
        assert_eq!(
            d.tips, p.tips,
            "after --prune the scratch is the probe's set"
        );
    }

    #[tokio::test]
    async fn fetch_skips_exact_refs_the_probe_did_not_see() {
        let (_up, url, main) = upstream();
        let (_s, objects, scratch) = serving();
        let pat = pats(&["refs/heads/main", "refs/heads/gone"]);
        let p = probe(&url, None, &pat).await.unwrap();
        // We "had" gone: it must come back absent, not fail the fetch.
        let have = HashMap::new();
        let d = fetch_refs(&url, None, &objects, &have, &pat, &p, &scratch)
            .await
            .unwrap();
        assert_eq!(
            d.tips,
            HashMap::from([("refs/heads/main".to_string(), main)])
        );
        // Only the missing exact ref: nothing fetched, nothing left.
        let only = pats(&["refs/heads/gone"]);
        let p = probe(&url, None, &only).await.unwrap();
        let d = fetch_refs(&url, None, &objects, &HashMap::new(), &only, &p, &scratch)
            .await
            .unwrap();
        assert!(d.tips.is_empty(), "{:?}", d.tips);
    }

    #[tokio::test]
    async fn negative_refspec_excludes() {
        let (up, url, _main) = upstream();
        run(up.path(), &["branch", "dependabot/npm"]);
        run(
            up.path(),
            &["update-ref", "refs/archive/1/refs/heads/main", "HEAD"],
        );
        let (_s, objects, scratch) = serving();
        let pat = pats(&["refs/*", "^refs/heads/dependabot/*"]);
        let p = probe(&url, None, &pat).await.unwrap();
        let d = fetch_refs(&url, None, &objects, &HashMap::new(), &pat, &p, &scratch)
            .await
            .unwrap();
        let mut names: Vec<&String> = d.tips.keys().collect();
        names.sort();
        assert_eq!(names, vec!["refs/heads/main"], "{:?}", d.tips);
        assert_eq!(d.tips, p.tips);
    }

    #[tokio::test]
    async fn is_ancestor_errors_on_missing_object() {
        let (up, _url, c1) = upstream();
        std::fs::write(up.path().join("b"), "b").unwrap();
        run(up.path(), &["add", "."]);
        run(up.path(), &["commit", "-q", "-m", "b"]);
        let c2 = run(up.path(), &["rev-parse", "HEAD"]);
        let root = tempfile::tempdir().unwrap();
        let id = crate::RepoId::new("o", "r").unwrap();
        let local = crate::LocalRepo::init(root.path(), &id, crate::ObjectFormat::Sha1).unwrap();
        let out = local
            .git(&[
                "fetch",
                "-q",
                &up.path().display().to_string(),
                "main:refs/heads/main",
            ])
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(local.is_ancestor(&c1, &c2).await.unwrap());
        assert!(!local.is_ancestor(&c2, &c1).await.unwrap());
        let missing = "1".repeat(40);
        assert!(local.is_ancestor(&missing, &c2).await.is_err());
    }
}
