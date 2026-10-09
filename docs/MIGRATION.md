# Moving an archive of repositories into floe — and proving it

Context: **runbook** for anyone moving existing git repositories (a directory of clones, bare mirrors, LFS) into
a floe bucket and retiring the originals. It covers what "verified" means, the rehearsal to run before trusting
floe with the only copy, the migration itself, and the recovery drills. The object store's own durability is
out of scope: versioning, replication and backups of the bucket are the store's job. Everything here is about
floe, given a bucket that keeps what it acknowledged. The guarantees it relies on are `docs/INTEGRITY.md` §6.

## 1. What "verified" means
`floe verify <owner>/<name> --against <source>` reads **only the bucket**: it opens the repository through a fresh,
empty cache in a scratch directory, so no local copy can stand in for a missing object. Then:

| Step | Checks | Catches |
|---|---|---|
| cold refs | the serving path: checkpoint `refs.pb` + every log segment of the tail | a missing snapshot, a truncated segment (fail loudly, D67) |
| rewind | an independent rebuild at the head seq (`floe wal materialize`), every live pack downloaded; its refs must equal the cold refs | a pack, index or log object that is missing |
| objects | `git fsck --full` on the rebuilt copy (`--quick`: `--connectivity-only`) | an object missing from the closure of any ref, or bytes that don't hash to their id |
| source | the source's refs, filtered as `floe import` filtered them, equal floe's; HEAD names the same branch | a ref not imported, a source that moved after the import |
| LFS | every Git LFS pointer reachable from a ref has its object in the bucket at the pointer's size (`--lfs-content`: downloaded and sha256-checked) | LFS objects never copied, or truncated |

Why this is a proof and not a sample: a commit id is the hash of its tree and parents, recursively. So **equal tip
ids over a closure that `fsck --full` found complete and correctly hashed are identical histories**, object for
object. Exit 0 prints `VERIFIED … — identical to the source`; anything else prints one `FAIL` line per difference
and exits 1.

What is **not** carried over (check that you don't need it before deleting a source):
- Refs outside the import's ref filter. The default is `refs/heads/*`, `refs/tags/*` and HEAD's target, so
  `refs/notes/*`, `refs/remotes/*`, `refs/stash`, `refs/replace/*` and `refs/pull/*` are not imported. `verify`
  prints how many there were. If you need them, pass the same `--refs` globs to both `import` and `verify`.
- Reflogs, hooks, `config`, `description`, `info/exclude`, and a working tree's uncommitted or untracked files.
  Commit or stash anything you want to keep; `git status` in each working tree first.
- Submodules (`.git/modules/…`) and repositories nested inside another one's directory. Migrate each as its
  own repository.
- Shallow clones are refused (`fetch --unshallow` first, or migrate their origin). An incomplete history is
  never published.

## 2. Before you trust it: the rehearsal (an afternoon)
Run all of it against a **throwaway bucket** first, with a copy of the archive or the archive read-only. Each step
has an exit code; none needs you to read logs to know the answer.

1. **The suites** on the build you will use: `just test`, `just sim`, `just e2e`. The sim includes the
   lost-response scenarios (`liveness_after_a_lost_cas_response`,
   `a_retried_cas_that_answers_412_on_its_own_write_keeps_the_segment`). `cargo test -p floe-cli --lib verify`
   imports a synthetic archive repository (two branches, both kinds of tag, a note, an LFS file), proves it, then
   checks that a deleted LFS object, a moved source, a flipped byte in a pack and a shallow or incomplete source
   are each caught.
2. **Prepare the sources**: in each LFS repository, `git lfs fetch --all` and `git lfs fsck`. The import copies
   what is in `.git/lfs/objects`, and `verify` names every pointer whose object never arrived. In every working
   tree, `git status` (see §1).
3. **Dry run**: `scripts/migrate-archive.sh --dry-run ARCHIVE` lists every repository it found, the floe name it
   will get, its size and the import mode. Name clashes (two `foo` directories) are reported, not merged.
4. **Migrate into the throwaway bucket**: `scripts/migrate-archive.sh --lfs-content ARCHIVE` (§3). Every row of
   `ARCHIVE/.floe-migrate/report.tsv` should be `ok`.
5. **Cold serve**: delete the server's `cache.dir`, start `floe-server`, then for each repository
   `git clone --mirror -c transfer.bundleURI=false <url>` and compare
   `git for-each-ref refs/heads refs/tags | sort | shasum` with the source, plus `git fsck --full` on the clone.
   This exercises the serving path (smart HTTP, upload-pack, LFS batch) that `verify` does not.
6. **Recovery drill** on one repository: push a commit, force-push it away, delete a branch. Run a base rebuild
   (`floe compact <o>/<r> --base --once`), wipe the cache, then:
   - `floe verify <o>/<r>` passes;
   - `floe wal ls` / `floe wal show` locate the seq before the force-push, and
     `floe wal materialize <o>/<r> --at-seq <that seq> --out DIR` rebuilds it with `fsck --full` clean;
   - the force-pushed commit is still in the new base (`git cat-file -t <sha>` on it). Full repacks keep
     unreachable objects, so the rewind needs nothing else.
7. **Re-run** the script: everything is skipped (`already done`). Delete one `<name>.ok` and re-run: that
   repository is imported again and must still verify. Re-imports are idempotent: LFS objects already in the
   bucket cost one HEAD each, and `--direct` skips uploads whose checksum is present.

The rehearsal that shaped this runbook (2026-10-08, rustfs, the debug build) went as follows:
- A 2,000-commit / 71-ref repository through `--direct`, its bare mirror, an LFS repository (3 objects) and an
  empty repository all verified.
- A source with a corrupted pack was refused at the source fsck.
- A shallow clone was published by the old import, broken, and is now refused. `verify` flags the copy the old
  code published with `missing commit`.
- After a cache wipe, every mirror clone matched its source.
- A force-pushed commit and a deleted branch survived a base rebuild, and the rewind to the earlier seq was
  fsck-clean.

## 3. Migrating
```sh
export FLOE_CONFIG=/path/to/floe.toml                 # the real bucket now
scripts/migrate-archive.sh --owner archive --lfs-content ~/archive
```
For each repository, in turn:
1. `git fsck --full` on the source. A corrupt source is reported as `source-corrupt` and skipped: repair it, or
   accept the loss knowingly, first.
2. `floe import`:
   - At most `--big` (default 5 GiB of objects): the default import, which streams `pack-objects --all` through
     `index-pack --fsck-objects`, checks the closure of every ref before publishing, and builds a bitmap'd base.
   - Above `--big`: `git pack-objects --all --write-bitmap-index` into one pack, then `floe import --direct`,
     which verifies the closure before any upload, is resumable after any interruption, and is idempotent
     (`docs/INTEGRITY.md` §5).
   - Either way the LFS objects go too (`--lfs`, default on): each file is sha256-checked against its name before
     upload, and written create-if-absent.
3. `floe verify --against` the source from a cold cache (§1).

The result is in `ARCHIVE/.floe-migrate/report.tsv` (`repo, source, bytes, mode, result, seconds`), with one
log per repository next to it. A verified repository gets `<name>.ok` and is skipped on the next run, so after a
failure, fix the cause and re-run the same command.

**Disk.** `verify` rebuilds the whole pack set in its scratch directory (default `cache.dir`; `--work-dir`
elsewhere), and the default import needs the repository plus its repack in `cache.dir`. For the big
repositories, run on a machine with an SSD and at least 2.5× the repository's size free. Don't run on a 20 GiB
tmpfs serving host (`AGENTS.md` §1.1).

Single repositories, by hand:
```sh
floe import --from ~/archive/foo foo-owner/foo
floe verify foo-owner/foo --against ~/archive/foo --lfs-content
```

## 4. After the migration: when the originals can go
1. Every row of the report is `ok`.
2. Wait for the first weekly base rebuild of each big repository (or run `floe compact <o>/<r> --base --once`
   on the SSD host), then run `floe verify` again. The rebuild is the one step that rewrites packs after the
   import.
3. Your bucket's own protection is in place (versioning, replication, a copy). floe never deletes pack bytes,
   but it cannot protect against the bucket losing them.
4. Keep the originals read-only for a while longer if you can. Any doubt is answered by
   `floe verify --against` again.

Periodic assurance afterwards:
- The maintainer's `fsck` unit audits connectivity every `maintenance.fsck_interval` (7 d) on a host that holds
  the whole pack set, and writes `fsck.pb` (`floe_repo_missing_objects{repo}`).
- `floe verify <o>/<r>` without `--against`, for example monthly from cron on an SSD machine, is the
  content-hashing, bucket-only check.

## 5. Recovery
| Situation | What to do |
|---|---|
| Every instance and its cache wiped | Nothing: the next request rebuilds from the bucket (refs in < 1 s, packs on demand). `floe verify` proves it. |
| A bad push, a force-push or a deleted branch | `floe wal ls <o>/<r>`, then `floe wal show <o>/<r> <seq>` (old and new oid per ref), then push the old oid back, or `floe wal materialize --at-seq <seq> --out DIR` for the whole state at that point. Superseded packs are never deleted. |
| `verify`, or a sync, says `incomplete log segment` / `checkpoint ref snapshot … is missing` | The bucket lost or truncated a WAL object. floe refuses to serve less than the log. Restore that object from the bucket's versioning, or re-import the repository from its source with `--replace`. |
| `verify` reports `missing <object>` | `docs/INTEGRITY.md` §3–4: the `repair` unit refetches from `upstream.git`; by hand, `floe wal add-pack` a pack of those objects built from the source. |
| `verify` reports an LFS object `not in the bucket` and "the source has it" | Re-run `floe import` (only the missing objects are uploaded), or `git lfs push --all` to floe. |
