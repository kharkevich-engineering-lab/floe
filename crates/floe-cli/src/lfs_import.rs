//! Copy a local repository's Git LFS objects (`.git/lfs/objects/aa/bb/<oid>`, git-lfs's layout
//! and the bucket's, `docs/LFS.md`) into `repos/<o>/<r>/lfs/objects/`, part of `floe import`.
//!
//! Every file is hashed before it is uploaded: a local object whose content is not its name is
//! reported and never stored. Uploads are create-if-absent; an object already in the bucket at
//! the same size costs one HEAD, so a re-run copies only what is missing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures::StreamExt as _;

use floe_store::{ObjectStore, PutBody, PutMode, StoreError};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LfsImportReport {
    pub uploaded: usize,
    pub uploaded_bytes: u64,
    pub already_present: usize,
    /// `(path, why)`: local files that are not a valid object of their name.
    pub bad: Vec<(PathBuf, String)>,
}

/// Every `aa/bb/<64 hex>` file under `lfs/objects` of `git_dir`, uploaded when absent.
pub async fn import_lfs_objects(
    store: &floe_store::Prefixed,
    git_dir: &Path,
    parallelism: usize,
) -> Result<LfsImportReport> {
    let root = git_dir.join("lfs").join("objects");
    let mut report = LfsImportReport::default();
    if !root.is_dir() {
        return Ok(report);
    }
    let files = tokio::task::spawn_blocking({
        let root = root.clone();
        move || list_objects(&root)
    })
    .await
    .context("listing lfs objects")??;
    let results: Vec<(PathBuf, Result<Outcome>)> = futures::stream::iter(files)
        .map(|(oid, path)| async move {
            let r = import_one(store, &oid, &path).await;
            (path, r)
        })
        .buffer_unordered(parallelism.max(1))
        .collect()
        .await;
    for (path, r) in results {
        match r {
            Ok(Outcome::Uploaded(n)) => {
                report.uploaded += 1;
                report.uploaded_bytes += n;
            }
            Ok(Outcome::Present) => report.already_present += 1,
            Ok(Outcome::Bad(why)) => report.bad.push((path, why)),
            Err(e) => return Err(e.context(format!("uploading {}", path.display()))),
        }
    }
    report.bad.sort();
    Ok(report)
}

enum Outcome {
    Uploaded(u64),
    Present,
    Bad(String),
}

fn list_objects(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for aa in std::fs::read_dir(root)? {
        let aa = aa?.path();
        if !aa.is_dir() {
            continue;
        }
        for bb in std::fs::read_dir(&aa)? {
            let bb = bb?.path();
            if !bb.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(&bb)? {
                let f = f?.path();
                let Some(name) = f.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if f.is_file()
                    && name.len() == 64
                    && name.bytes().all(|b| b.is_ascii_hexdigit())
                    && f.parent() == Some(bb.as_path())
                {
                    out.push((name.to_ascii_lowercase(), f));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

async fn import_one(store: &floe_store::Prefixed, oid: &str, path: &Path) -> Result<Outcome> {
    let (digest, size) = tokio::task::spawn_blocking({
        let path = path.to_path_buf();
        move || sha256_file(&path)
    })
    .await
    .context("hashing")??;
    if digest != oid {
        return Ok(Outcome::Bad(format!("content hashes to {digest}")));
    }
    let key = floe_proto::keys::lfs_key(oid);
    if let Some(m) = store.head(&key).await?
        && m.size == size
    {
        return Ok(Outcome::Present);
    }
    match store
        .put(
            &key,
            PutBody::File(path.to_path_buf()),
            PutMode::Create.into(),
        )
        .await
    {
        Ok(_) => Ok(Outcome::Uploaded(size)),
        // Somebody (a push, a previous run) wrote it between the HEAD and the PUT: content
        // addressed, so the same bytes — unless a broken write left another size.
        Err(StoreError::PreconditionFailed { .. }) => match store.head(&key).await? {
            Some(m) if m.size == size => Ok(Outcome::Present),
            Some(m) => Ok(Outcome::Bad(format!(
                "the bucket already holds {} bytes under this oid (expected {size}); not overwritten",
                m.size
            ))),
            None => anyhow::bail!("create of {key} refused, yet nothing is there"),
        },
        Err(e) => Err(e.into()),
    }
}

fn sha256_file(path: &Path) -> Result<(String, u64)> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        n += k as u64;
        hasher.update(buf.get(..k).unwrap_or_default());
    }
    Ok((hex::encode(hasher.finalize()), n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_local(git_dir: &Path, content: &[u8]) -> String {
        use sha2::Digest as _;
        let oid = hex::encode(sha2::Sha256::digest(content));
        let p = git_dir
            .join("lfs/objects")
            .join(crate::verify::lfs_rel_path(&oid));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
        oid
    }

    #[tokio::test]
    async fn uploads_verified_objects_once_and_refuses_corrupt_ones() {
        let mem: floe_store::DynStore = floe_store::memory::MemoryStore::shared();
        let store = floe_store::Prefixed::new(mem, "repos/t/lfs/");
        let dir = tempfile::tempdir().unwrap();
        let a = put_local(dir.path(), b"first object\n");
        let b = put_local(dir.path(), &vec![7u8; 3 << 20]);
        // A file whose content is not its name.
        let bad = put_local(dir.path(), b"original");
        std::fs::write(
            dir.path()
                .join("lfs/objects")
                .join(crate::verify::lfs_rel_path(&bad)),
            b"bit rot",
        )
        .unwrap();
        // Stray files git-lfs leaves around are ignored.
        std::fs::create_dir_all(dir.path().join("lfs/tmp")).unwrap();
        std::fs::write(dir.path().join("lfs/tmp/partial"), b"x").unwrap();

        let r = import_lfs_objects(&store, dir.path(), 4).await.unwrap();
        assert_eq!(r.uploaded, 2);
        assert_eq!(r.uploaded_bytes, 13 + (3 << 20));
        assert_eq!(r.already_present, 0);
        assert_eq!(r.bad.len(), 1, "{:?}", r.bad);
        for oid in [&a, &b] {
            let m = store
                .head(&floe_proto::keys::lfs_key(oid))
                .await
                .unwrap()
                .unwrap();
            assert!(m.size > 0);
        }
        assert!(
            store
                .head(&floe_proto::keys::lfs_key(&bad))
                .await
                .unwrap()
                .is_none()
        );

        // A second run uploads nothing.
        let again = import_lfs_objects(&store, dir.path(), 4).await.unwrap();
        assert_eq!(again.uploaded, 0);
        assert_eq!(again.already_present, 2);
    }
}
