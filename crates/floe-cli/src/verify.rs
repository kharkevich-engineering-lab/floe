//! `floe verify OWNER/NAME [--against GITDIR]` — prove that a repository can be rebuilt from the
//! bucket alone and, with `--against`, that it is the source repository it was imported from.
//!
//! Everything is read through a fresh, empty cache under a scratch directory, so no local
//! serving copy can stand in for a missing object (docs/MIGRATION.md):
//! 1. **cold refs**: open the repository the way a new instance does (checkpoint + log tail);
//! 2. **rewind**: rebuild it independently at its head seq from the bucket (`wal materialize`),
//!    every live pack downloaded, and require the two ref sets to agree;
//! 3. **objects**: `git fsck --full` on the rebuilt copy — every object reachable from every ref
//!    is present and hashes to its id (`--quick`: connectivity only, no content hashing);
//! 4. **source**: the source's refs (the same filter `floe import` applied) equal floe's, and
//!    HEAD names the same branch. With 3 that is the whole proof: a commit id is the hash of its
//!    entire history, so equal tips over a complete, verified closure are identical histories;
//! 5. **LFS**: every Git LFS pointer reachable from a ref has an object in the bucket of the
//!    pointer's size (`--lfs-content`: downloaded and sha256-checked).
//!
//! Exit 0 = verified, 1 = a difference or a failure (each one printed).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use futures::StreamExt as _;

use floe_config::Config;
use floe_store::{GetOptions, GetResult, ObjectStore, open_store};
use floe_wal::Registry;

use crate::cli::parse_repo_id;
use crate::import::{RefFilter, resolve_git_dir};

pub struct VerifyOptions {
    pub repo: String,
    pub against: Option<PathBuf>,
    pub refs: Vec<String>,
    pub quick: bool,
    pub lfs: LfsCheck,
    pub work_dir: Option<PathBuf>,
    pub keep: bool,
}

/// How far LFS objects are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfsCheck {
    Off,
    /// In the bucket at the pointer's size (one HEAD each).
    Presence,
    /// Downloaded and sha256-checked.
    Content,
}

/// What a run found. Empty `problems` = verified.
#[derive(Debug, Default)]
pub struct VerifyReport {
    pub head_seq: u64,
    pub refs: usize,
    pub source_refs_not_imported: usize,
    pub lfs_pointers: usize,
    pub problems: Vec<String>,
}

pub async fn run(opts: VerifyOptions, cfg: &Arc<Config>) -> Result<()> {
    let store = open_store(cfg).await?;
    let report = verify(store, &opts, cfg).await?;
    for p in &report.problems {
        println!("FAIL  {p}");
    }
    if report.problems.is_empty() {
        println!(
            "VERIFIED {} at seq {}: {} refs, {} LFS objects{}",
            opts.repo,
            report.head_seq,
            report.refs,
            report.lfs_pointers,
            if opts.against.is_some() {
                " — identical to the source"
            } else {
                ""
            }
        );
        Ok(())
    } else {
        bail!(
            "{} is NOT verified: {} problem(s)",
            opts.repo,
            report.problems.len()
        )
    }
}

/// The checks behind `floe verify`, against an open store (`store` is the bucket, never a cache).
pub async fn verify(
    store: floe_store::DynStore,
    opts: &VerifyOptions,
    cfg: &Arc<Config>,
) -> Result<VerifyReport> {
    let (owner, name) = parse_repo_id(&opts.repo)?;
    let id = floe_git::RepoId::new(owner, name)?;
    let mut report = VerifyReport::default();

    let base = opts
        .work_dir
        .clone()
        .unwrap_or_else(|| cfg.cache.dir.clone());
    std::fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    let scratch = tempfile::Builder::new()
        .prefix("floe-verify-")
        .tempdir_in(&base)
        .with_context(|| format!("scratch dir under {}", base.display()))?;
    let scratch_path = scratch.path().to_path_buf();
    // A cache nobody has used: whatever this run sees came from the bucket.
    let mut cold_cfg = (**cfg).clone();
    cold_cfg.cache.dir = scratch_path.join("cache");
    cold_cfg.cache.prewarm.clear();
    let cold_cfg = Arc::new(cold_cfg);
    let registry = Registry::new(store, cold_cfg.clone());

    // 1. Cold refs, the serving path.
    let t = Instant::now();
    let handle = registry
        .open(&id)
        .await
        .with_context(|| format!("cold open of {id}"))?;
    let manifest = handle.manifest();
    report.head_seq = manifest.head_seq;
    if manifest.head_seq == 0 {
        // Created, never pushed: no refs and no objects is the whole state. It is the
        // source's state too only if the source has no refs either.
        println!("cold refs: {id} is empty (created, nothing published)");
        if let Some(src) = &opts.against {
            let src = resolve_git_dir(src)?;
            let (source_all, source_head) = git_refs(&src)?;
            let filter = RefFilter::new(opts.refs.clone());
            for (name, oid) in source_all
                .iter()
                .filter(|(n, _)| filter.keep(n, &source_head))
            {
                report.problems.push(format!(
                    "ref {name} ({oid}) is in the source, floe is empty"
                ));
            }
        }
        return Ok(report);
    }
    let served = handle.local().refs()?;
    let served_refs: BTreeMap<String, String> = served
        .refs
        .iter()
        .map(|r| (r.name.clone(), r.oid.clone()))
        .collect();
    let served_head = served.head_target.clone();
    report.refs = served_refs.len();
    println!(
        "cold refs: {} refs at seq {} (checkpoint {}), HEAD -> {} [{:.1}s]",
        served_refs.len(),
        manifest.head_seq,
        manifest.checkpoint.as_ref().map_or(0, |c| c.seq),
        if served_head.is_empty() {
            "(none)"
        } else {
            &served_head
        },
        t.elapsed().as_secs_f64()
    );

    // 2. Independent rebuild from the bucket at the same seq.
    let t = Instant::now();
    let out = scratch_path.join("rewind");
    let pack_bytes: u64 = manifest.packs.iter().map(|p| p.pack_size).sum();
    println!(
        "rewind: downloading {} pack(s), {} …",
        manifest.packs.len(),
        human_bytes(pack_bytes)
    );
    crate::wal_cmd::materialize_at(&registry, &id, manifest.head_seq, &out)
        .await
        .context("rebuilding the repository from the bucket")?;
    let git_dir = id.local_dir(&out);
    let (rebuilt_refs, rebuilt_head) = git_refs(&git_dir)?;
    println!(
        "rewind: rebuilt {} refs [{:.1}s]",
        rebuilt_refs.len(),
        t.elapsed().as_secs_f64()
    );
    diff_refs(
        "cold refs",
        &served_refs,
        "the rebuilt copy",
        &rebuilt_refs,
        &mut report.problems,
    );
    if served_head != rebuilt_head {
        report.problems.push(format!(
            "HEAD: cold refs say {served_head:?}, the rebuilt copy {rebuilt_head:?}"
        ));
    }

    // 3. Every object reachable from every ref, present and hashing to its id.
    let t = Instant::now();
    let mut fsck = vec!["fsck", "--no-dangling", "--no-progress"];
    fsck.push(if opts.quick {
        "--connectivity-only"
    } else {
        "--full"
    });
    let out_fsck = Command::new("git")
        .args(&fsck)
        .current_dir(&git_dir)
        .output()
        .context("running git fsck")?;
    let stderr = String::from_utf8_lossy(&out_fsck.stderr);
    let stdout = String::from_utf8_lossy(&out_fsck.stdout);
    if out_fsck.status.success() {
        println!(
            "objects: git {} clean [{:.1}s]",
            fsck.join(" "),
            t.elapsed().as_secs_f64()
        );
    } else {
        let lines: Vec<&str> = stdout.lines().chain(stderr.lines()).collect();
        for l in lines.iter().take(20) {
            report.problems.push(format!("fsck: {l}"));
        }
        if lines.len() > 20 {
            report
                .problems
                .push(format!("fsck: … {} more line(s)", lines.len() - 20));
        }
        if lines.is_empty() {
            report
                .problems
                .push(format!("git fsck failed ({})", out_fsck.status));
        }
    }

    // 4. The source.
    let source_dir = match &opts.against {
        Some(src) => {
            let src = resolve_git_dir(src)?;
            let (source_all, source_head) = git_refs(&src)?;
            let filter = RefFilter::new(opts.refs.clone());
            let source: BTreeMap<String, String> = source_all
                .iter()
                .filter(|(name, _)| filter.keep(name, &source_head))
                .map(|(n, o)| (n.clone(), o.clone()))
                .collect();
            report.source_refs_not_imported = source_all.len() - source.len();
            diff_refs(
                "the source",
                &source,
                "floe",
                &rebuilt_refs,
                &mut report.problems,
            );
            if !source_head.is_empty() && source_head != rebuilt_head {
                report.problems.push(format!(
                    "HEAD: the source names {source_head}, floe {rebuilt_head:?}"
                ));
            }
            println!(
                "source: {} refs compared ({} outside the ref filter, not imported){}",
                source.len(),
                report.source_refs_not_imported,
                if report.source_refs_not_imported > 0 {
                    " — pass the same --refs you imported with to include them"
                } else {
                    ""
                }
            );
            Some(src)
        }
        None => None,
    };

    // 5. LFS objects behind every reachable pointer (a history fsck found holes in cannot be
    // walked for them).
    let objects_ok = out_fsck.status.success();
    if opts.lfs != LfsCheck::Off && !objects_ok {
        println!("lfs: not checked — the history is incomplete (see the fsck findings)");
    }
    if opts.lfs != LfsCheck::Off && objects_ok {
        let t = Instant::now();
        let pointers = match lfs_pointers(&git_dir) {
            Ok(p) => p,
            Err(e) => {
                report
                    .problems
                    .push(format!("lfs: listing pointers failed: {e:#}"));
                Vec::new()
            }
        };
        report.lfs_pointers = pointers.len();
        if !pointers.is_empty() {
            let missing = check_lfs(
                handle.store(),
                &pointers,
                opts.lfs == LfsCheck::Content,
                source_dir.as_deref(),
                &mut report.problems,
            )
            .await?;
            println!(
                "lfs: {} object(s) behind reachable pointers, {} {} [{:.1}s]",
                pointers.len(),
                if opts.lfs == LfsCheck::Content {
                    "downloaded and sha256-checked"
                } else {
                    "present at the pointer's size"
                },
                if missing == 0 {
                    "— all good".to_string()
                } else {
                    format!("— {missing} missing or wrong")
                },
                t.elapsed().as_secs_f64()
            );
        }
    }

    if opts.keep {
        let kept = scratch.keep();
        println!(
            "kept the rebuilt copy at {}",
            id.local_dir(&kept.join("rewind")).display()
        );
    }
    Ok(report)
}

/// `git for-each-ref` + HEAD's symbolic target of a git dir.
fn git_refs(git_dir: &Path) -> Result<(BTreeMap<String, String>, String)> {
    let out = Command::new("git")
        .args(["for-each-ref", "--format=%(objectname) %(refname)"])
        .current_dir(git_dir)
        .output()
        .context("git for-each-ref")?;
    anyhow::ensure!(
        out.status.success(),
        "git for-each-ref in {}: {}",
        git_dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut refs = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((oid, name)) = line.trim().split_once(' ') {
            refs.insert(name.to_string(), oid.to_string());
        }
    }
    let head = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(git_dir)
        .output()
        .context("git symbolic-ref HEAD")?;
    let head = if head.status.success() {
        String::from_utf8_lossy(&head.stdout).trim().to_string()
    } else {
        String::new()
    };
    Ok((refs, head))
}

/// Every ref that is missing, extra or different between `a` and `b` (the first 50 of each kind).
fn diff_refs(
    a_name: &str,
    a: &BTreeMap<String, String>,
    b_name: &str,
    b: &BTreeMap<String, String>,
    problems: &mut Vec<String>,
) {
    let names: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let mut shown = 0usize;
    let mut total = 0usize;
    for n in names {
        let line = match (a.get(n), b.get(n)) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => format!("ref {n}: {a_name} {x}, {b_name} {y}"),
            (Some(x), None) => format!("ref {n} ({x}) is in {a_name}, not in {b_name}"),
            (None, Some(y)) => format!("ref {n} ({y}) is in {b_name}, not in {a_name}"),
            (None, None) => continue,
        };
        total += 1;
        if shown < 50 {
            problems.push(line);
            shown += 1;
        }
    }
    if total > shown {
        problems.push(format!(
            "… {} more ref difference(s) between {a_name} and {b_name}",
            total - shown
        ));
    }
}

/// A parsed Git LFS pointer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LfsPointer {
    pub oid: String,
    pub size: u64,
}

/// Parse a blob as a Git LFS pointer (spec v1): `version …\noid sha256:<64 hex>\nsize <n>\n`.
pub fn parse_lfs_pointer(blob: &[u8]) -> Option<LfsPointer> {
    if blob.len() > 1024 {
        return None;
    }
    let text = std::str::from_utf8(blob).ok()?;
    let mut lines = text.lines();
    if !lines
        .next()?
        .starts_with("version https://git-lfs.github.com/spec/")
    {
        return None;
    }
    let (mut oid, mut size) = (None, None);
    for l in lines {
        if let Some(o) = l.strip_prefix("oid sha256:") {
            oid = Some(o.trim().to_string());
        } else if let Some(s) = l.strip_prefix("size ") {
            size = s.trim().parse::<u64>().ok();
        }
    }
    let oid = oid.filter(|o| o.len() == 64 && o.bytes().all(|b| b.is_ascii_hexdigit()))?;
    Some(LfsPointer {
        oid: oid.to_ascii_lowercase(),
        size: size?,
    })
}

/// Every LFS pointer in a blob reachable from any ref (blobs ≤ 1 KiB, parsed).
fn lfs_pointers(git_dir: &Path) -> Result<Vec<LfsPointer>> {
    // Small blobs reachable from every ref: `--filter=blob:limit` leaves out every blob larger
    // than a pointer can be, so the walk never reads big content.
    let list = Command::new("git")
        .args([
            "rev-list",
            "--objects",
            "--all",
            "--no-object-names",
            "--filter=blob:limit=1025",
            "--filter-provided-objects",
        ])
        .current_dir(git_dir)
        .output()
        .context("git rev-list --objects")?;
    anyhow::ensure!(
        list.status.success(),
        "git rev-list --objects: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let mut cat = Command::new("git")
        .args([
            "cat-file",
            "--batch=%(objecttype) %(objectname) %(objectsize)",
        ])
        .current_dir(git_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("git cat-file --batch")?;
    let mut stdin = cat.stdin.take().context("cat-file stdin")?;
    let ids = list.stdout;
    let writer = std::thread::spawn(move || stdin.write_all(&ids));
    let mut reader = std::io::BufReader::new(cat.stdout.take().context("cat-file stdout")?);
    let mut found = BTreeSet::new();
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let mut parts = header.split_whitespace();
        let (Some(kind), Some(_), Some(size)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("unexpected cat-file header {header:?}");
        };
        let size: usize = size.parse().context("cat-file size")?;
        let mut body = vec![0u8; size + 1];
        std::io::Read::read_exact(&mut reader, &mut body)?;
        body.truncate(size);
        if kind == "blob"
            && let Some(p) = parse_lfs_pointer(&body)
        {
            found.insert(p);
        }
    }
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("cat-file writer panicked"))??;
    let status = cat.wait()?;
    anyhow::ensure!(status.success(), "git cat-file --batch failed ({status})");
    Ok(found.into_iter().collect())
}

/// HEAD (or download and hash) every pointer's object; returns how many failed.
async fn check_lfs(
    store: &floe_store::Prefixed,
    pointers: &[LfsPointer],
    content: bool,
    source: Option<&Path>,
    problems: &mut Vec<String>,
) -> Result<usize> {
    let results: Vec<(LfsPointer, Result<Option<String>>)> =
        futures::stream::iter(pointers.iter().cloned())
            .map(|p| async move {
                let r = check_one_lfs(store, &p, content).await;
                (p, r)
            })
            .buffer_unordered(16)
            .collect()
            .await;
    let mut bad = 0usize;
    for (p, r) in results {
        let problem = match r {
            Ok(None) => continue,
            Ok(Some(why)) => why,
            Err(e) => format!("error: {e:#}"),
        };
        bad += 1;
        if bad <= 50 {
            let in_source = source
                .map(|s| s.join("lfs/objects").join(lfs_rel_path(&p.oid)))
                .filter(|f| f.is_file());
            problems.push(format!(
                "lfs {} ({} bytes): {problem}{}",
                p.oid,
                p.size,
                if in_source.is_some() {
                    " — the source has it: `floe import` copies .git/lfs/objects"
                } else {
                    ""
                }
            ));
        }
    }
    if bad > 50 {
        problems.push(format!("… {} more LFS problem(s)", bad - 50));
    }
    Ok(bad)
}

async fn check_one_lfs(
    store: &floe_store::Prefixed,
    p: &LfsPointer,
    content: bool,
) -> Result<Option<String>> {
    use sha2::Digest as _;
    let key = floe_proto::keys::lfs_key(&p.oid);
    if !content {
        return Ok(match store.head(&key).await? {
            None => Some("not in the bucket".into()),
            Some(m) if m.size != p.size => Some(format!("in the bucket with {} bytes", m.size)),
            Some(_) => None,
        });
    }
    let mut body = match store.get(&key, GetOptions::default()).await {
        Ok(GetResult::Object { body, .. }) => body,
        Ok(GetResult::NotModified { .. }) => bail!("unexpected 304 for {key}"),
        Err(e) if e.is_not_found() => return Ok(Some("not in the bucket".into())),
        Err(e) => return Err(e.into()),
    };
    let mut hasher = sha2::Sha256::new();
    let mut n = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        n += chunk.len() as u64;
        hasher.update(&chunk);
    }
    let got = hex::encode(hasher.finalize());
    Ok(if n != p.size {
        Some(format!("downloaded {n} bytes"))
    } else if got != p.oid {
        Some(format!("content hashes to {got}"))
    } else {
        None
    })
}

/// git-lfs's on-disk layout, which is also the bucket's: `aa/bb/<oid>`.
pub fn lfs_rel_path(oid: &str) -> PathBuf {
    let (aa, bb) = (oid.get(..2).unwrap_or(""), oid.get(2..4).unwrap_or(""));
    Path::new(aa).join(bb).join(oid)
}

#[allow(clippy::cast_precision_loss)] // display only
fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < U.len() {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U.get(i).copied().unwrap_or("B"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// An archive repository with what a real one has: two branches, an annotated and a
    /// lightweight tag, a note (outside the default ref filter) and one LFS-tracked file whose
    /// object sits in `.git/lfs/objects`.
    fn archive_repo() -> (tempfile::TempDir, String) {
        use sha2::Digest as _;
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path();
        git(w, &["init", "-q", "-b", "main"]);
        git(w, &["config", "user.email", "t@t"]);
        git(w, &["config", "user.name", "t"]);
        git(w, &["config", "commit.gpgsign", "false"]);
        for i in 0..3 {
            std::fs::write(w.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
            git(w, &["add", "."]);
            git(w, &["commit", "-q", "-m", &format!("c{i}")]);
        }
        let lfs_content = vec![42u8; 70_000];
        let oid = hex::encode(sha2::Sha256::digest(&lfs_content));
        let obj = w.join(".git/lfs/objects").join(lfs_rel_path(&oid));
        std::fs::create_dir_all(obj.parent().unwrap()).unwrap();
        std::fs::write(&obj, &lfs_content).unwrap();
        std::fs::write(
            w.join("big.bin"),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
                lfs_content.len()
            ),
        )
        .unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-q", "-m", "lfs"]);
        git(w, &["tag", "-a", "v1", "-m", "v1"]);
        git(w, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(w.join("feature.txt"), "x\n").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-q", "-m", "feature"]);
        git(w, &["tag", "light"]);
        git(w, &["notes", "add", "-m", "a note"]);
        git(w, &["checkout", "-q", "main"]);
        (dir, oid)
    }

    fn config(cache: &Path) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.cache.dir = cache.to_path_buf();
        cfg.store.bucket = "test".into();
        cfg.wal.freshness_ttl = std::time::Duration::ZERO;
        Arc::new(cfg)
    }

    fn opts(against: &Path, lfs: LfsCheck) -> VerifyOptions {
        VerifyOptions {
            repo: "arch/one".into(),
            against: Some(against.to_path_buf()),
            refs: vec![],
            quick: false,
            lfs,
            work_dir: None,
            keep: false,
        }
    }

    /// Import an archive repository, then prove it from the bucket alone; and every way it can
    /// differ — a missing LFS object, a source that moved, a corrupt pack — is caught.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_imported_archive_verifies_and_every_difference_is_caught() {
        let (src, lfs_oid) = archive_repo();
        let store: floe_store::DynStore = floe_store::memory::MemoryStore::shared();
        let cache = tempfile::tempdir().unwrap();
        let cfg = config(cache.path());
        crate::import::run_on(
            store.clone(),
            src.path().to_path_buf(),
            "arch/one".into(),
            false,
            vec![],
            &cfg,
        )
        .await
        .unwrap();
        crate::import::import_lfs_on(store.clone(), src.path(), "arch/one", &cfg)
            .await
            .unwrap();

        // Verified, from a cache nobody used, LFS content included.
        let r = verify(store.clone(), &opts(src.path(), LfsCheck::Content), &cfg)
            .await
            .unwrap();
        assert!(r.problems.is_empty(), "{:#?}", r.problems);
        assert_eq!(r.refs, 4, "main, feature, v1, light");
        assert_eq!(r.source_refs_not_imported, 1, "refs/notes/commits");
        assert_eq!(r.lfs_pointers, 1);

        // An LFS object missing from the bucket is named, with the fix.
        let lfs_key = format!("repos/arch/one/{}", floe_proto::keys::lfs_key(&lfs_oid));
        store.delete(&lfs_key, None).await.unwrap();
        let r = verify(store.clone(), &opts(src.path(), LfsCheck::Presence), &cfg)
            .await
            .unwrap();
        assert_eq!(r.problems.len(), 1, "{:#?}", r.problems);
        assert!(r.problems[0].contains(&lfs_oid) && r.problems[0].contains("the source has it"));
        // Re-running the LFS import copies exactly that object back.
        crate::import::import_lfs_on(store.clone(), src.path(), "arch/one", &cfg)
            .await
            .unwrap();
        let r = verify(store.clone(), &opts(src.path(), LfsCheck::Presence), &cfg)
            .await
            .unwrap();
        assert!(r.problems.is_empty(), "{:#?}", r.problems);

        // A source that moved after the import is not "the same repository".
        std::fs::write(src.path().join("late.txt"), "late\n").unwrap();
        git(src.path(), &["add", "."]);
        git(src.path(), &["commit", "-q", "-m", "late"]);
        let r = verify(store.clone(), &opts(src.path(), LfsCheck::Off), &cfg)
            .await
            .unwrap();
        assert!(
            r.problems
                .iter()
                .any(|p| p.starts_with("ref refs/heads/main:")),
            "{:#?}",
            r.problems
        );
        git(src.path(), &["reset", "-q", "--hard", "HEAD~1"]);

        // Flip one byte in the middle of every live pack in the bucket: never "verified".
        let mut keys = Vec::new();
        let mut list = store.list("repos/arch/one/wal/", None);
        while let Some(m) = list.next().await {
            let m = m.unwrap();
            if Path::new(&m.key).extension().is_some_and(|e| e == "pack") {
                keys.push(m.key);
            }
        }
        assert!(!keys.is_empty());
        for key in keys {
            let (_, b) = floe_store::ObjectStoreExt::get_bytes(&*store, &key)
                .await
                .unwrap()
                .unwrap();
            let mut v = b.to_vec();
            let mid = v.len() / 2;
            v[mid] ^= 0xff;
            store
                .put(&key, v.into(), floe_store::PutMode::Overwrite.into())
                .await
                .unwrap();
        }
        match verify(store.clone(), &opts(src.path(), LfsCheck::Off), &cfg).await {
            Ok(r) => assert!(!r.problems.is_empty(), "a corrupt pack verified"),
            Err(e) => eprintln!("corrupt pack refused: {e:#}"),
        }
    }

    /// A source whose history is incomplete publishes nothing: a shallow clone is refused up
    /// front (it used to be published — `pack-objects` honours the graft — and then fail its
    /// post-import repack), and a repository missing an object deep in its history fails before
    /// the publish.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_incomplete_source_publishes_nothing() {
        let (src, _) = archive_repo();
        let store: floe_store::DynStore = floe_store::memory::MemoryStore::shared();
        let cache = tempfile::tempdir().unwrap();
        let cfg = config(cache.path());

        let shallow = tempfile::tempdir().unwrap();
        let url = format!("file://{}", src.path().display());
        let to = shallow.path().join("s");
        git(
            shallow.path(),
            &["clone", "-q", "--depth", "2", &url, to.to_str().unwrap()],
        );
        let err = crate::import::run_on(
            store.clone(),
            to,
            "arch/shallow".into(),
            false,
            vec![],
            &cfg,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("shallow clone"), "{err:#}");

        // Drop the first commit's tree from an otherwise complete repository.
        let broken = tempfile::tempdir().unwrap();
        let b = broken.path().join("b");
        git(
            broken.path(),
            &["clone", "-q", "--bare", &url, b.to_str().unwrap()],
        );
        let root = git(&b, &["rev-list", "--max-parents=0", "HEAD"]);
        let tree = git(&b, &["rev-parse", &format!("{root}^{{tree}}")]);
        let keep = git(&b, &["rev-list", "--objects", "--all"]);
        let keep: String = keep
            .lines()
            .filter_map(|l| l.split(' ').next())
            .filter(|o| *o != tree)
            .flat_map(|o| [o, "\n"])
            .collect();
        let mut child = Command::new("git")
            .current_dir(&b)
            .args(["pack-objects", "-q", "objects/pack/pack"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(keep.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        for e in std::fs::read_dir(b.join("objects/pack")).unwrap() {
            let p = e.unwrap().path();
            let newest = std::fs::read_dir(b.join("objects/pack"))
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "pack"))
                .max_by_key(|p| std::fs::metadata(p).unwrap().modified().unwrap())
                .unwrap();
            if p.file_stem() != newest.file_stem() {
                std::fs::remove_file(p).unwrap();
            }
        }
        let err =
            crate::import::run_on(store.clone(), b, "arch/broken".into(), false, vec![], &cfg)
                .await
                .unwrap_err();
        eprintln!("broken source refused: {err:#}");
        let (_, m) =
            floe_store::ObjectStoreExt::get_bytes(&*store, "repos/arch/broken/manifest.pb")
                .await
                .unwrap()
                .unwrap();
        let m =
            <floe_proto::v1::Manifest as floe_proto::prost::Message>::decode(m.as_ref()).unwrap();
        assert_eq!(m.head_seq, 0, "an incomplete import published refs");
    }

    /// A repository created and never pushed is verified (empty), unless its source has refs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_created_never_pushed_repository_is_verified_empty() {
        let store: floe_store::DynStore = floe_store::memory::MemoryStore::shared();
        let cache = tempfile::tempdir().unwrap();
        let cfg = config(cache.path());
        let registry = Registry::new(store.clone(), cfg.clone());
        let id = floe_git::RepoId::new("arch", "one").unwrap();
        registry
            .create(&id, floe_git::ObjectFormat::Sha1)
            .await
            .unwrap();
        let mut o = opts(Path::new("/nonexistent"), LfsCheck::Off);
        o.against = None;
        let r = verify(store.clone(), &o, &cfg).await.unwrap();
        assert!(r.problems.is_empty(), "{:#?}", r.problems);
        let (src, _) = archive_repo();
        let r = verify(store.clone(), &opts(src.path(), LfsCheck::Off), &cfg)
            .await
            .unwrap();
        assert_eq!(r.problems.len(), 4, "{:#?}", r.problems);
    }

    #[test]
    fn lfs_pointers_parse_and_everything_else_does_not() {
        let oid = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let p =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 12345\n");
        assert_eq!(
            parse_lfs_pointer(p.as_bytes()),
            Some(LfsPointer {
                oid: oid.into(),
                size: 12345
            })
        );
        assert_eq!(parse_lfs_pointer(b"hello\n"), None);
        let short = "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n";
        assert_eq!(parse_lfs_pointer(short.as_bytes()), None);
        let no_size = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\n");
        assert_eq!(parse_lfs_pointer(no_size.as_bytes()), None);
        assert_eq!(lfs_rel_path(oid), Path::new("4d").join("7a").join(oid));
    }
}
