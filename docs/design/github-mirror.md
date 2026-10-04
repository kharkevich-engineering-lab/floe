# GitHub mirror MVP: follow globs, archive-on-rewrite, `floe-mirror`, `floe-catalog`

Context: **the design of record for the GitHub mirror MVP**, written before any code so that four engineers can
build the four work packages (§A–§D) in parallel without talking to each other. Read `GOAL.md`, `AGENTS.md`
(§2.2 the write path, §3 principles, D24/D28/D30/D32/D33/D46/D47) and `docs/EVENTS.md` first. Every interface that
crosses a work package is pinned here (§0.3); everything else is the implementer's call within the house rules
(`[workspace.lints]`, no `unwrap`/`expect`/`panic` in production code, clippy pedantic, tests next to the code).
Where this document and the code disagree after landing, the code wins and this file gets a dated note.

Status: **proposed** (2026-10-04; revised the same day after review). Decision numbers D48–D51 are reserved
for it (§D.6). One item needs the owner's sign-off before §B starts: the name mapping
`<prefix>-<owner>/<repo>` (§B.5, R1). Every other contract in §0.3 is frozen.

---

## 0. Summary

### 0.1 What we are building

| # | Requirement | Where |
|---|---|---|
| 1 | Monitor and mirror repositories from GitHub (public and private) automatically; GitLab/Gitea later | §B `floe-mirror` (`Source` trait, GitHub implementation only) |
| 2 | The bucket (RustFS, S3) is the only backend; S3 Tables (Iceberg REST) holds **metadata/audit tables only** | §B state in the bucket; §C `floe-catalog`, feature-gated, best effort |
| 3 | Own repositories keep working exactly as today | §A is opt-in per pattern; §B touches only repositories it created |
| 4 | Push to GitHub: not in MVP, a seam only | §E |
| 5 | Multi-user / GitHub-compatible workflows: not in MVP | out of scope |

The MVP is three changes and some wiring:

- **§A Follow globs + archive-on-rewrite** (`floe-config`, `floe-git::follow`, `floe-server::follow`).
  `[upstream] follow` accepts `refs/heads/*`-style patterns. When upstream force-pushes or deletes a followed
  ref, the old tip is kept at `refs/archive/<unix-ts>/<original-ref>` **in the same WAL entry** that applies the
  new state. Nothing is ever lost. `on_rewrite = "refuse"` keeps today's behaviour.
- **§B `floe-mirror`**: discovers GitHub repositories (users, orgs, stars, explicit list; include/exclude globs),
  creates a floe repository per GitHub repository with `[upstream]` settings, and reconciles them periodically.
  It runs in-process on a `maintain` host under one bucket lease and keeps its state in the bucket. It does no
  git transfer itself: **follow (§A) moves the bytes**. The mirror only decides *which* repositories follow
  *what*.
- **§C `floe-catalog`**: an Iceberg writer for four audit tables (`repo_inventory`, `sync_runs`, `ref_events`,
  `force_push_log`). It is fed by its own WAL tail on the events host (the bridge's cursor machinery, but its
  own loop, so it never slows the webhook) and by lossy telemetry from the mirror and follow loops. It sits
  behind the `catalog` cargo feature, and a catalog outage only adds catalog lag.
- **§D Wiring**: CLI, server loops, config, compose, README, AGENTS.md decisions.

### 0.2 Why this shape (principles check)

- **I (no state outside the bucket)**: mirror state is `mirror/github/state.json` (CAS); the HTTP ETag cache is
  a disposable bucket object; Iceberg tables are *derived* audit copies, never a source of truth (wipe the
  catalog and only history in the warehouse is lost; the WAL still has every ref event).
- **II (manifest CAS is the only commit point)**: follow still publishes one PUSH entry per round; the archive
  ref and the rewrite are in **one** `RefTransaction`. The mirror creates repos by the existing manifest CAS
  create and publishes settings via the existing SETTINGS entry. No new commit points. One new bucket
  object is overwritten without CAS, the disposable `mirror/github/http-cache.json`; it joins principle II's
  `PutMode::Overwrite` list (§D.6). Every other new object (state, leases, cursors, `catalog/*.json`) is CAS'd.
- **Scope (X, `GOAL.md` §4)**: upstream mirroring and derived audit tables are not in GOAL §4 today. §D.6
  amends it in the same change, so the question "which line of GOAL §4 is this for?" has an answer.
- **III (side effects are WAL readers)**: the catalog consumes the WAL through the bridge's cursor machinery
  (D32/D46), in its own loop. The push-back seam (§E) is a WAL reader. Neither `follow.rs` nor the mirror makes an HTTP call
  *as a step of a write*; the only thing they hand the catalog is a lossy `try_send` of telemetry *after* the
  write finished (§C.6), the same as a metric.
- **X (keep floe small)**: no new git transport (follow reuses `git fetch`), no new auth path, no database. The
  heavy dependency (iceberg + arrow + parquet) is behind a feature that is off by default.

### 0.3 Cross-package contracts (frozen by this document)

| Contract | Producer | Consumer | Section |
|---|---|---|---|
| `[upstream]` keys `follow` (patterns), `on_rewrite`, `head`, `follow_interval`, `source`; host-only `token_env_by_host` | §A (floe-config) | §B writes them into settings | §A.1 |
| `floe_config::refpattern::RefPatterns` (parse/matches/refspecs; in floe-config because floe-git already depends on it) | §A | floe-config validation, `floe_git::follow`, §B | §A.2 |
| `LocalRepo::is_ancestor` returns `Err` on any exit code other than 0/1 | §A (floe-git) | follow classification | §A.4 |
| Archive ref name `refs/archive/<unix-ts>/<original-ref>` | §A | §C `force_push_log`, humans | §A.3 |
| Log entry meta `follow.archived` (format below) | §A | §C | §A.5 |
| `floe_catalog::{Recorder, SyncRun, InventoryRecord, parse_follow_archived}` (always compiled, no iceberg) | §C | §A (follow), §B (mirror), the server's catalog tail | §C.4, §C.6 |
| `floe_mirror::{Source, RemoteRepo, Target, HttpCache}` | §B | §D (server/CLI wiring) | §B.3, §B.7 |
| `RepoHandle::publish_settings_if(toml, author, message, expected_revision)` + `WalError::SettingsConflict { expected, actual }` | §B (small floe-wal addition) | §B | §B.7 |
| `Registry::create` returns `WalError::AlreadyExists` for an already-cached handle too | §B (floe-wal) | §B `WalTarget::create`, `admin.rs` | §B.7 |
| `floe_store::coord::cas_update_json`; `LeaseGuard::released_flag() -> Arc<AtomicBool>` | §B (floe-store) | §B, later the bridge | §B.6, §B.8 |

Build order: §A and §C core types first (one small PR each). §B and the §C writer follow in parallel, and §D
wires everything last. A package may stub another package's side of a contract in its tests.

---

## A. Follow globs + archive on rewrite/delete (D48)

### A.1 Config schema (`crates/floe-config/src/lib.rs`, `UpstreamConfig`)

Old documents still parse unchanged: `follow = ["refs/heads/main"]` is a valid pattern list. Two things change
in behaviour, and both are deliberate:

- The **default** `on_rewrite = "archive"`, so a rewound upstream on an existing `follow` is now archived and
  applied instead of refused. Set `on_rewrite = "refuse"` to keep the old behaviour. AGENTS.md's banner says
  we owe no compatibility here, and the user asked for archive as the default.
- A followed ref that disappears upstream is archived and deleted. Today it is left alone.

```rust
pub struct UpstreamConfig {
    pub git: Option<String>,
    pub lfs: Option<String>,
    pub token_env: Option<String>,                 // host-only (unchanged)
    /// Ref patterns kept equal to upstream's (§A.2). Empty = follow off; this is checked before, and
    /// instead of, `RefPatterns::parse`, so `follow = []` is always valid (the mirror's freeze, §B.9).
    pub follow: Vec<String>,
    /// What follow does when upstream rewrites (non-fast-forward, or any tag move) or deletes a followed ref.
    pub on_rewrite: OnRewrite,                     // default Archive
    /// Desired symbolic target of HEAD (`refs/heads/main`); follow retargets HEAD when the WAL's differs and
    /// the target exists. Unset = follow never touches HEAD (today's behaviour). Written by the mirror from
    /// GitHub's `default_branch`.
    pub head: Option<String>,
    /// Per-repository minimum pause between follow rounds (the loop still ticks every
    /// `maintenance.follow_interval`; a repository whose last round is younger than this is skipped).
    /// Unset = every tick. The mirror writes a long backstop (`10m`) and nudges on change (§B.8).
    #[serde(default, with = "humantime_serde::option")]
    pub follow_interval: Option<Duration>,
    /// Opaque provenance label (`github:123456789`): who manages this `[upstream]`. Follow ignores it except
    /// for logs and `sync_runs.source`; the mirror uses it to recognise its own repositories (§B.7).
    pub source: Option<String>,
    /// Host-only: env var name per upstream host (`"github.com" = "FLOE_GITHUB_TOKEN"`). Resolution for a
    /// repository: `token_env_by_host[host(upstream.git)]`, else `token_env`, else unauthenticated.
    /// Refused in settings and stripped from public effective config, exactly like `token_env`.
    pub token_env_by_host: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnRewrite {
    /// Keep the old tip under `refs/archive/<unix-ts>/<ref>`, then apply upstream's state. Nothing is lost.
    #[default]
    Archive,
    /// D33 behaviour: refuse non-fast-forwards and leave refs deleted upstream as they are; logged every round.
    Refuse,
}
```

Changes in the same file:

- `with_settings`: also refuse `upstream.token_env_by_host` in a settings document (same message shape as
  `token_env`). Validate `follow`: an empty list is follow off and needs nothing more; a non-empty list goes
  through `floe_config::refpattern::RefPatterns::parse` (§A.2). Validate `head` (must start with
  `refs/heads/`). An invalid value is a 400 at `PUT …/api/settings` and nothing is published. This replaces
  today's inline check (`r.starts_with("refs/") && !r.contains('*')`, lib.rs ~1485).
- Redaction: `Config::public_settings_toml` (lib.rs ~890) removes `token_env_by_host` next to `token_env`, and
  the flatten filter in `crates/floe-server/src/settings.rs` (~299, today `k.ends_with("token_env")`) becomes
  `k.contains("token_env")`, so flattened keys like `upstream.token_env_by_host.github.com` never reach the
  `fields` of `/api/settings/describe`. Test next to the existing `token_env` redaction test (lib.rs ~1957).
- `Config::validate`: the same pattern validation for host-level `[upstream]`. `token_env_by_host` keys are
  bare hostnames (no scheme, no path), and values are non-empty.
- **Mirror token, scoped** (in `Config::load`, not `validate`): set `github_mirror.use_token` (not TOML)
  when **either** `github_mirror.enabled` **or** the variable `github_mirror.token_env` names is set in this
  process's environment. The second clause covers serving-only hosts (LFS read-through runs in the serving
  request path, not only on the maintainer), which do not run the mirror but share the fleet's `floe.toml`
  (D45) and environment. `upstream_token_env` then resolves `token_env_by_host`, else — **only for a
  mirror-managed repository** (`upstream.source = "github:…"`) on `host(github_mirror.git_url)` — the
  mirror's `token_env`, else `upstream.token_env`. Own repositories and every other upstream resolve exactly
  as before (requirement 3); nothing is inserted into `token_env_by_host`.
- `floe.example.toml` documents every new key (§D.3).

Token resolution moves to one function, `floe_config::Config::upstream_token_env(&self, url: &str) ->
Option<&str>`. Every call site that reads `cfg.upstream.token_env` today migrates to it, in the same change:
`crates/floe-server/src/follow.rs` `token_for` (~490), `crates/floe-server/src/lfs.rs` (~109 and ~284, LFS
read-through), and `crates/floe-server/src/ops.rs` (~330, the `repair` op). `grep -n 'upstream.token_env'
crates/` must come back empty afterwards, except for the function itself.

### A.2 Pattern syntax and matching (new `crates/floe-config/src/refpattern.rs`, `RefPatterns`)

`RefPatterns` lives in floe-config, not floe-git: floe-git already depends on floe-config, and floe-config's
validation needs the parser, so putting it in floe-git would be a crate cycle. It is pure string code with no
git dependency. `floe_git::follow` uses it from there.

The patterns are passed to `git fetch` verbatim as refspec sources, so they use **git refspec glob
semantics**, not POLICY.md's doublestar dialect. A pattern has one consumer, and that consumer is git.

| Form | Meaning | Example |
|---|---|---|
| exact | one ref | `refs/heads/main` |
| glob | exactly one `*`, matching **any** characters **including `/`** (git refspec rule) | `refs/heads/*` matches `refs/heads/feat/x` |
| negative | leading `^`, exact or glob; excludes (git ≥ 2.29 negative refspec) | `^refs/heads/dependabot/*` |

Rules (`RefPatterns::parse(&[String]) -> Result<RefPatterns, RefPatternError>`; it is only called on a
non-empty list, since an empty `follow` means follow off, §A.1):

1. Every entry starts with `refs/` (after an optional `^`) and has at most one `*`. Each entry passes git's
   `check-ref-format --refspec-pattern` rules: no `..`, `@{`, `\`, control characters, space, `~^:?[`,
   trailing `/` or `.lock`. Implement the checks in Rust (cheap, no subprocess) and test them against git in
   one test.
2. A non-empty list has at least one positive entry (`["^refs/heads/x"]` alone is an error).
3. **Reserved namespaces are never followed**: `refs/archive/` and `refs/follow/`. An entry that starts with
   either is a parse error. `RefPatterns::matches` also returns false for any name under them, even when a
   positive pattern like `refs/*` would match. The fetch always appends `^refs/archive/*` and `^refs/follow/*`,
   so an upstream that is itself a floe never feeds its archive into ours.
4. `matches(name)` = (any positive matches) ∧ ¬(any negative matches) ∧ ¬reserved. Deterministic, no regex.
5. `refspecs()` → positive: `+<src>:refs/follow/<src minus "refs/">` (the `*` carries through), negative:
   `^<src>`. `is_exact()` reports entries without `*` (needed for the missing-ref fallback below).

New `floe_git::follow::probe(upstream, token, patterns) -> Result<Probe, GitError>`: one `git ls-remote
<upstream>` (no patterns on the command line: `ls-remote`'s own patterns are tail matches, not refspecs), whose
output floe filters with `RefPatterns::matches`. `Probe { tips: HashMap<String, String>, advertised_any: bool }`;
`advertised_any` is whether upstream advertised **any** ref at all, before filtering. It costs the same single
`ls-refs` round trip the fetch's advertisement costs today, and it needs no objects and no scratch.

`fetch_refs` changes signature: `refs: &[String]` becomes `patterns: &RefPatterns`, and `have` becomes "every
WAL ref matching `patterns`". It is only called when the probe saw a difference (§A.4). Behaviour changes:

- **Scratch reset**: before the fetch, the scratch's `refs/follow/*` is made **exactly** `have` (delete every
  `refs/follow/*` not in `have`; `for-each-ref` + one `update-ref --stdin`). Today only the listed refs are
  reset. With globs the set changes from round to round.
- **`--prune`** is added to the fetch so a ref upstream no longer advertises disappears from `refs/follow/*`.
  Then `read_scratch().tips` is exactly upstream's matching set, and a deletion is `have.keys() − tips.keys()`.
- **Exact ref missing upstream**: git fails the whole fetch with `couldn't find remote ref <r>`. The probe
  already lists what upstream has, so exact entries absent from `probe.tips` are left out of the refspecs
  before the fetch and reported as absent (deletions). There is no retry path. If upstream deletes an exact
  ref between the probe and the fetch, the fetch fails, and the next round re-probes.
- The token still travels through the one-shot credential helper (never argv), for the probe as for the fetch.

### A.3 Archive ref naming

`refs/archive/<unix-ts>/<original-ref>`, where `<original-ref>` is the **full** ref name and `<unix-ts>` is
taken once per op and shared by every archive in that op: `ts = max(now_utc_secs, newest_archive_ts + 1)`,
where `newest_archive_ts` is the largest `<unix-ts>` among the repository's current `refs/archive/*` refs in
the WAL snapshot the op planned from (0 when none). The timestamp is therefore strictly increasing per
repository, and no archive name the op picks can exist in the state it planned against. Examples:

```
refs/archive/1791072000/refs/heads/main          # main was force-pushed upstream
refs/archive/1791072000/refs/tags/v1.2.0          # tag v1.2.0 was moved upstream
refs/archive/1791075600/refs/heads/feat/old-api  # branch deleted upstream
```

- The archive ref is created with `old_oid = ""` (must not exist), so it can never overwrite an earlier
  archive. **A follow round is one atomic `RefTransaction`**: `publish.rs` verifies every update
  (`verify_txn`) and appends the entry only when all of them pass (`all_ok`); WAL application is always atomic
  (wal.proto). A name that does exist anyway (another writer published between the op's snapshot and its CAS:
  a second maintainer, R16, or a human push of that exact name) therefore rejects the **whole round**, with
  every other ref's update. Nothing is published, the op reports `refused` with the conflicting ref, and the
  next round re-plans from the new snapshot with a new timestamp. The same holds for any other ref that moved
  under the round.
- Archive refs are ordinary refs: they keep objects reachable for `fsck`/`repair`/compaction, they are
  advertised by `ls-refs`, and `git fetch origin 'refs/archive/*:refs/archive/*'` retrieves them. Follow never
  deletes or moves them.
- The mirror's read-only policy (§B.7) protects them from pushes. For own repositories, POLICY.md gains an
  example rule `archive-immutable` (`refs/archive/**`, restricts `update`, `delete`). *Superseded, see §A.7:
  the rule is built in.*

### A.4 Where the change goes in `follow.rs`

Split `crates/floe-server/src/follow.rs` into `follow/mod.rs` (loop, op, statuses: today's code) and
`follow/plan.rs` (pure, synchronous, unit-tested):

```rust
// follow/plan.rs
pub(crate) struct Observed<'a> {
    pub have: &'a HashMap<String, String>,   // WAL refs matching the patterns
    pub tips: &'a HashMap<String, String>,   // upstream refs matching the patterns (probe, then post --prune)
    pub advertised_any: bool,                // upstream advertised at least one ref of any name (Probe)
}
pub(crate) enum Change { Create { name, new }, Update { name, old, new }, Delete { name, old } }
/// Diff; never emits a no-op. When `advertised_any` is false (upstream advertised **no refs at all**), it
/// yields no Delete: the empty-advertisement guard. An upstream that suddenly has nothing is a
/// misconfiguration, an outage or a wiped repository, not a mass delete. The guard is about the whole
/// advertisement, not the matching set: `follow = ["refs/heads/main"]` with `main` deleted upstream, or
/// `["refs/tags/*"]` with the last tag deleted, is an ordinary deletion and is archived and applied.
pub(crate) fn diff(o: Observed<'_>) -> (Vec<Change>, Option<String /* guard notice */>);

pub(crate) enum Kind { FastForward, Rewrite }   // decided by the op (async ancestry), fed back in
pub(crate) struct Plan {
    pub updates: Vec<RefUpdate>,          // the one RefTransaction (archive creates + applies + HEAD)
    pub archived: Vec<Archived>,          // for meta + logs + metrics
    pub refused: Vec<String>,             // human lines (policy = refuse, guard)
}
pub(crate) struct Archived { pub archive_ref: String, pub original: String, pub old: String, pub new: Option<String> }
pub(crate) fn build(changes: Vec<(Change, Kind)>, policy: OnRewrite, ts: u64, head: Option<HeadMove>) -> Plan;
```

Classification is done in the op **after `ingest_pack`** and before `build`, under the existing read guard,
as `follow.rs` already orders it today (`is_ancestor` runs only once the fetched objects are in the serving
copy). Before ingest, the new commit is not in the serving copy, and an ancestry check there would misreport
every fast-forward.

| Change | Kind |
|---|---|
| Create | FastForward (nothing to lose) |
| Update under `refs/tags/` | **Rewrite**, always (POLICY.md: "a tag retarget is `force-push`") |
| Update elsewhere | `is_ancestor(old, new)`: `Ok(true)` ⇒ FastForward; `Ok(false)` ⇒ Rewrite; `Err` ⇒ the round fails (nothing published, `failed` outcome) |
| Delete | Rewrite |

`LocalRepo::is_ancestor` (floe-git lib.rs ~1906) changes in the same PR: today it is
`Ok(out.status.success())`, so `merge-base --is-ancestor` exiting 128 (a missing object, or `old`/`new` not a
commit) reads as "not an ancestor". New contract: exit 0 ⇒ `Ok(true)`, exit 1 ⇒ `Ok(false)`, anything else ⇒
`Err(GitError)` with stderr. A missing object must fail the round, never archive. A branch pointed at a
non-commit object upstream (rare, legal) then fails every round with a clear message instead of archiving every
round; R17 covers it.

`build` with `Archive`: FastForward ⇒ one `RefUpdate{name, old, new}`. A rewritten update ⇒ two updates,
`RefUpdate{archive_ref, "", old}` + `RefUpdate{name, old, new}`. A delete ⇒ `RefUpdate{archive_ref, "", old}` +
`RefUpdate{name, old, ""}` (an empty new oid is a delete in `apply_txn_to_map`/`apply_ref_txn`). With
`Refuse`, a rewrite or delete becomes a `refused` line (today's text for rewinds, plus `"<ref>: deleted
upstream; left as is"`).

`HeadMove`: when `upstream.head` is set, differs from the WAL's HEAD target, and the target is in the
post-plan ref set, add `RefUpdate{name: "HEAD", new_symbolic_target: head}`. `publish.rs` already handles
symbolic updates.

Changes in `follow/mod.rs`:

- `run_pass` becomes **refs-first**: `sync_refs()` only (the settings ride on the manifest), skip when
  `follow` is empty, `RefPatterns::parse` (a parse error is a `warn!` and skip; it cannot normally happen,
  because settings are validated at publish). Skip the repository when `upstream.follow_interval` is set and
  the last round's `Instant` (new field on `FollowStatus`, not serialized) is younger. Then `probe` and
  `diff(have, probe.tips, probe.advertised_any)`. Nothing differs and HEAD needs no move ⇒ `in-sync`, done:
  **no `sync()`, no `packs_fit()`, no fetch, no scratch**. Today every round does a Serve-level `sync()`
  first, which re-downloads the pack set of every repository the LRU evicted; at mirror scale (thousands of
  small repositories on a 20 GiB budget) that is a constant re-materialization for rounds that find nothing.
  Only when something differs does the loop check `packs_fit()` and start the op.
- **The loop no longer fetches** (`prefetched=1` and the loop-side `fetch_refs` call go away). It starts the
  op through the same `(repo, "follow")` task lock a nudge (§B.8) or a manual op uses, so exactly one fetch
  touches `cache.dir/follow/<o>/<n>.git` at a time. Today the loop's prefetch runs outside any task lock and
  can race a manual op on the scratch (`fetch_refs` deletes `objects/pack/*` and rewrites `refs/follow/*`);
  mirror nudges would make that race routine. The scratch is touched only inside the `follow` op.
- `current(handle, refs)` → `current_matching(handle, &patterns)`: every snapshot ref with
  `patterns.matches(name)`.
- `op`, in this order: `sync()` (Serve) under the read guard → `current_matching` → `fetch_refs` (exact refs
  absent from a fresh probe dropped, §A.2) → `diff` → `ingest_pack` (as today) → classify (`is_ancestor` on the
  serving copy, which now holds the fetched objects) → `ts` (§A.3) → `build` → connectivity over the
  non-delete new oids (archive refs point at objects already held) → `fill_peeled` → one `publish_push`.
- The round is atomic (§A.3): `publish_push` either appends every update in the plan or none. `per_ref` is
  still reported as today, so a rejected round names the ref that moved. There is no partial outcome to
  handle: an archive ref cannot fail while its paired apply succeeds. A rejected round records `refused`, and
  the next round re-plans. With wide globs, one contended ref holds back every ref of that repository until the
  contention ends; that is the price of "the archive and the rewrite are one entry", and the contention only
  exists with two writers (R16).
- `FollowStatus` gains `archived: Vec<String>` (human lines), and `outcome` gains `"archived"` (published with
  at least one archive). The Settings tab shows it as it shows the other outcomes.
- `token_for` uses `cfg.upstream_token_env(upstream)` (§A.1).
- After the op returns (in `run_pass`, never inside `op`), record one `SyncRun` (§C.6) with
  `state.recorder.record_sync_run(..)`. Non-blocking, lossy.
- Metrics: `floe_follow_archived_total{repo, kind="rewrite"|"delete"}`, and
  `floe_follow_rounds_total{outcome="archived"}`.

### A.5 WAL and event implications

- **One PUSH entry per round, unchanged**: `principal = upstream`, `upstream = <url>`, `agent = floe follow`.
  New meta key, present only when something was archived:

  ```
  meta["follow.archived"] = "<archive_ref> <original_ref> <old_oid> <new_oid|->\n…"   (one line per archive, sorted)
  ```

  `LogEntry.meta` is already a free `map<string,string>`, so the proto does not change (D4 append-only holds).
  `-` marks a delete.
- **Events (docs/EVENTS.md)**: no schema change (`schema_version` stays 1). A rewrite appears as **two events
  with the same `_floe.seq`**, a `create` of `refs/archive/<ts>/<ref>` and an `update` (or `delete`) of `<ref>`,
  both with `pusher = "upstream"`. EVENTS.md gains one paragraph saying so and stating that `refs/archive/` is
  reserved for this.
- Bundles: `refs/archive/*` is never in `main_only` bundles. Full bundles that take every ref take them too,
  which is fine.
- `docs/ROUNDTRIPS.md`: a publishing round is unchanged on the bucket side (one log PUT + one manifest CAS).
  A round that finds nothing gets **cheaper**: refs-level sync only, no Serve-level `sync()` (§A.4). Upstream
  side: one `ls-refs` (the probe) per round as today, plus the fetch's own advertisement when something moved
  (§D.7).

### A.6 Tests (§A)

`floe-config` unit tests (`refpattern.rs`): `ref_patterns_parse_and_match` (table test: exact/glob/negative,
`refs/*` never matches `refs/archive/x`, invalid forms rejected, negative-only list rejected);
`with_settings_accepts_empty_follow` (`follow = []` publishes, the freeze of §B.9); redaction of
`token_env_by_host` in `public_settings_toml`.

`floe-git` unit tests (`follow.rs` `#[cfg(test)]`, real git, `file://` upstream in a tempdir):

- `ref_patterns_agree_with_git` (the parser against `git check-ref-format --refspec-pattern` on a corpus).
- `probe_reports_matching_tips_and_advertised_any` (an upstream with no refs ⇒ `advertised_any = false`).
- `fetch_with_globs_reports_new_and_pruned_refs` (branch created and branch deleted upstream between two
  rounds → `tips` reflects both).
- `fetch_skips_exact_refs_the_probe_did_not_see`.
- `negative_refspec_excludes`.
- `is_ancestor_errors_on_missing_object` (exit 128 ⇒ `Err`, not `Ok(false)`).

`floe-server` unit tests (`follow/plan.rs`): `diff` cases, the empty-advertisement guard (only when
`advertised_any = false`), `single_exact_ref_deleted_upstream_is_a_delete`, `last_tag_deleted_is_a_delete`,
`build` with both policies, tag-move-is-rewrite, archive-and-delete pairing, archive `ts` above the newest
existing archive, HEAD move only when its target exists, ordering determinism of `follow.archived`.

`crates/floe-server/tests/follow.rs` (extends the existing two-instance test, which uses a second floe as the
upstream):

1. `follows_globs_creating_branches_and_tags`.
2. `upstream_force_push_is_archived_then_applied`: one PUSH entry, `refs/archive/<ts>/refs/heads/main = old`,
   `main = new`, `meta["follow.archived"]` correct, `old` still reachable (`git cat-file -e`).
3. `upstream_delete_is_archived_then_deleted`.
4. `on_rewrite_refuse_keeps_d33_behaviour` (the existing assertion moves here).
5. `empty_advertisement_never_deletes` (upstream with zero refs).
6. `moved_tag_is_archived`.
7. `own_repo_without_follow_is_untouched` (requirement 3).
8. `fast_forward_is_not_archived` (classification after ingest).
9. `round_with_a_moved_ref_publishes_nothing` (a concurrent writer moves one followed ref between plan and
   publish ⇒ no entry, `refused`, next round converges).
10. `in_sync_round_does_no_serve_sync` (assert on store op counts: no pack GETs after eviction).
11. `nudge_during_loop_round_joins_one_follow_task`.

`tests/events.rs`: one golden test that a rewrite yields `create(refs/archive/…)` + `update` with the same seq.

### A.7 As landed (2026-10-04)

Where the code differs from §A.1–§A.6 (the code wins):

- `floe_git::follow::fetch_refs(upstream, token, serving_objects, have, patterns, probe, scratch)` takes the
  op's fresh `Probe` and drops the exact entries it did not see itself (`RefPatterns::refspecs(skip)`), instead of
  the caller filtering. `RefPatterns::exact()` replaces `is_exact()`. `refspecs()` returns nothing when no
  positive entry is left, and the fetch is skipped.
- A round whose diff is empty but whose `have` would lose refs to the empty-advertisement guard reports
  outcome `refused` with the guard's notice (not `in-sync`), so the Settings tab shows why nothing moved.
- A rejected publish (a ref moved under the round) is an `Ok` op result with `published = 0` and the conflicting
  refs in `refused` (outcome `refused`), as D33's per-ref rejections were; a refused-only plan (policy `refuse`)
  stays an op error (`failed` in the loop's report), as before.
- Not wired yet, owned by other packages: `Recorder::record_sync_run` (§C.6, needs `floe-catalog`) and the
  derived `token_env_by_host` default from `[github_mirror]` in `Config::load` (§B.4 config). Of the §A.6
  integration tests, `tests/follow.rs` has `on_rewrite_refuse_keeps_d33_behaviour` (the old test) and
  `upstream_globs_force_push_and_delete_are_archived_then_applied` (globs + negative, fast-forward not archived,
  force-push and delete archived in one entry, `follow.archived`, reachability, in-sync round); the rest
  (empty advertisement, concurrent writer, store op counts, nudge, events golden) are open.
- Review fixes (2026-10-04): the loop syncs with `RepoHandle::sync_refs_only` (no background pack prefetch,
  so an in-sync round never re-materializes an evicted repository: `in_sync_round_does_not_rematerialize_evicted_packs`).
  With `refuse`, deletions, tag moves and a rewind already refused at the same old/new oids are reported
  `refused` by the loop without a task (`on_rewrite_refuse_keeps_d33_behaviour` covers both); any other op
  error is outcome `failed`, and an op that found nothing to do is `in-sync`. A repository whose object set
  does not fit records a `failed` round (so `follow_interval` applies). One repository's open/sync error no
  longer ends the pass. `archive_ts` ignores archive timestamps more than a day ahead of now and skips every
  timestamp already used, so a pushed `refs/archive/<u64::MAX>/x` cannot pin it (the POLICY.md example
  `archive-immutable` now restricts `create` too). The probe is bounded (`PROBE_TIMEOUT`, `kill_on_drop`,
  `GIT_HTTP_LOW_SPEED_*`). `upstream.git`/`upstream.lfs` refuse userinfo, `?` and `#`; the authority for
  `token_env_by_host` ends at the first `/`, `?` or `#`; the credential helper answers only for the
  upstream's own `host[:port]`.
- Archive protection (2026-10-04): an example rule left every repository without a policy file open to
  forged, moved or deleted archive refs. `floe_server::policy` now has a built-in `archive-immutable` rule
  (`refs/archive/**`; `create`, `update`, `delete`; no bypass), evaluated after the file's rules on every
  push. A policy file that defines a rule with that name replaces it (for example to give admins a bypass).
  Follow does not go through policy, so it still writes archives. Tests: `policy::tests::archive_*` and
  `tests/policy.rs` `archive_refs_are_immutable_without_a_policy`.

---

## B. `floe-mirror` (D49)

### B.1 Responsibilities and non-responsibilities

The mirror **decides**. Follow **moves bytes**.

- Does: discover repositories at a source; map them to floe names; create the floe repository; publish its
  `[upstream]` settings (and a read-only `policy.json`); keep those up to date as the source changes
  (rename, archive, visibility, deletion); nudge follow when the source reports a push; record inventory and
  sync telemetry.
- Does not: fetch git objects (follow does, on the maintaining host, D28); delete floe repositories (MVP
  never deletes: "nothing is ever lost"); push to GitHub (§E); store tokens.

### B.2 Module layout (`crates/floe-mirror/`)

```
crates/floe-mirror/
  Cargo.toml          # deps: floe-config, floe-store, floe-git, floe-wal, floe-proto, floe-catalog (core),
                      #       reqwest, serde, serde_json, chrono, async-trait, tokio, tracing, metrics, anyhow, thiserror
  src/lib.rs          # pub use; run_loop(); reconcile_once()
  src/source.rs       # Source trait, RemoteRepo, Discovery, Lookup, SourceError
  src/http_cache.rs   # forge-neutral conditional-GET cache (HttpCache: url → {etag, next, body})
  src/github/mod.rs   # GithubSource: Source
  src/github/http.rs  # GitHub's REST client: headers, pagination (Link), rate limits, Retry-After
  src/github/model.rs # serde structs for the REST payloads we read (only the fields we use)
  src/naming.rs       # RemoteRepo → RepoId mapping, collision suffix
  src/select.rs       # include/exclude/skip rules (pure)
  src/state.rs        # MirrorState (JSON), load/CAS helpers
  src/plan.rs         # pure: (state, discovery, existing) → Vec<Action>
  src/apply.rs        # Action → Target calls; updates state entries
  src/target.rs       # Target trait (floe side) + WalTarget (Registry-backed impl)
  src/settings.rs     # render/merge the [upstream] table (toml_edit-free: toml::Table)
  src/fake.rs         # #[cfg(any(test, feature = "testing"))] FakeSource, FakeTarget
```

It does **not** depend on `floe-server`, so the server can depend on it with no cycle. The server-specific
pieces (follow nudge, placement) come in through the `Target` trait (§B.7).

### B.3 The `Source` trait

```rust
/// A forge we can discover repositories on. GitHub now; GitLab/Gitea implement the same trait later.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    /// `github` | `gitlab` | `gitea`: the prefix of `upstream.source` (`github:<id>`) and of metrics labels.
    fn kind(&self) -> &'static str;
    /// Every repository the selection names, as visible to our credential. `complete = false` when any
    /// listing failed or was cut short (rate limit): the planner then never infers "gone" from absence.
    async fn discover(&self, sel: &Selection, cache: &mut HttpCache) -> Result<Discovery, SourceError>;
    /// One repository by its stable id (rename detection, gone/forbidden checks).
    async fn lookup(&self, id: &str, cache: &mut HttpCache) -> Result<Lookup, SourceError>;
    /// Git URL follow fetches (`upstream.git`), LFS endpoint (`upstream.lfs`), wiki git URL if any.
    fn git_url(&self, r: &RemoteRepo) -> String;
    fn lfs_url(&self, r: &RemoteRepo) -> Option<String>;
    fn wiki_url(&self, r: &RemoteRepo) -> Option<String>;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteRepo {
    pub id: String,              // stable across renames (GitHub numeric id as a string)
    pub owner: String,           // as the forge spells it
    pub name: String,
    pub private: bool,
    pub archived: bool,
    pub fork: bool,
    pub disabled: bool,
    pub default_branch: Option<String>,   // None for an empty repository
    pub pushed_at: Option<String>,        // RFC 3339; the change signal
    pub size_kb: u64,
    pub has_wiki: bool,
}
/// Interpreted per forge: `orgs` are GitLab groups or Gitea organisations; a forge without stars ignores
/// `starred`. The fields are the union, not a GitHub schema.
pub struct Selection { pub users: Vec<String>, pub orgs: Vec<String>, pub starred: Vec<String>, pub repos: Vec<String> }
pub struct Discovery { pub repos: Vec<RemoteRepo>, pub complete: bool, pub stats: ApiStats }
pub enum Lookup { Found(RemoteRepo), Gone /* 404 */, Forbidden /* 401/403 non-rate-limit */ }
pub struct ApiStats { pub requests: u32, pub not_modified: u32, pub rate_remaining: Option<u32>, pub rate_reset: Option<i64> }
#[derive(Debug, thiserror::Error)]
pub enum SourceError { Unauthorized, RateLimited { until: SystemTime }, Http(String), Decode(String) }
```

Nothing in the trait or in `plan.rs`/`apply.rs`/`state.rs` names GitHub. Everything per-forge is keyed by
`Source::kind()`: the state object `mirror/<kind>/state.json`, the cache `mirror/<kind>/http-cache.json`, the
lease `leases/mirror-<kind>.pb`, the `upstream.source` prefix and the metric label. For the MVP, `<kind>` is
`github` everywhere, so the paths below read `mirror/github/…`. Only the config section is per forge
(`[github_mirror]`; a later `[gitlab_mirror]` maps onto the same `Selection`).

### B.4 Config: `[github_mirror]` (`floe-config`, new `GithubMirrorConfig`)

`[github]` is taken by the facade (D42), hence `[github_mirror]`. This section is host-level only. It is not
a settings section (D24) because it is not per repository.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Run the mirror on this host (needs the `maintain` role; `validate` refuses otherwise). |
| `api_url` | `"https://api.github.com"` | REST base. GHES: `https://ghe.example.com/api/v3`. |
| `git_url` | `"https://github.com"` | Base for clone/LFS/wiki URLs and the `token_env_by_host` key. |
| `token_env` | `"FLOE_GITHUB_TOKEN"` | Env var holding a PAT (classic `repo` scope, or fine-grained Contents:read + Metadata:read). Never in the bucket. Empty/missing env at runtime = a failed pass with a clear error, never a crash. |
| `prefix` | `"gh"` | floe owner = `<prefix>-<github owner>` (§B.5). `[a-z0-9]{1,16}`. |
| `interval` | `"5m"` | Discovery/reconcile cadence. `0` = only `floe github sync --once` and startup. |
| `users` | `[]` | Owners whose repositories are mirrored. `"@me"` = the token's user, private included. |
| `orgs` | `[]` | Organisations (all types; private when the token can see them). |
| `starred` | `[]` | Users whose stars are mirrored (`"@me"` allowed). |
| `repos` | `[]` | Explicit `"owner/name"`. Evaluation order below. |
| `include` | `["*/*"]` | Globs over `owner/name` (case-insensitive). `*` stops at `/`, so every pattern has exactly one `/`: `*/*` (everything), `owner/*`, `*/name`. A pattern without `/` is a `validate` error, not a silent match-nothing. |
| `exclude` | `[]` | Globs, same syntax. Exclude wins over everything, explicit `repos` included. |
| `skip_archived` | `true` | Do not *start* mirroring archived repositories (already mirrored ones are kept, §B.9). |
| `skip_forks` | `true` | Same for forks. |
| `include_private` | `true` | `false` = public only. `true` is refused by `validate` on a host where floe readers are not all trusted with every mirrored repository, unless `private_visible_to_all_readers = true` (below; R2). |
| `private_visible_to_all_readers` | `false` | The operator's explicit acknowledgement that floe has no per-repository read ACL, so every principal with read on this floe can read every mirrored private repository. Required whenever `include_private = true`, in every auth mode: the bucket is the fleet's, so the mirror host's auth mode does not bound who reads it. |
| `wikis` | `false` | Also mirror `<repo>.wiki.git` as `<prefix>-<owner>/<name>.wiki` (§B.10). |
| `lfs` | `true` | Write `upstream.lfs` so LFS objects read through (`docs/LFS.md`). |
| `follow` | `["refs/heads/*", "refs/tags/*"]` | Patterns written into each repository's `upstream.follow`. |
| `on_rewrite` | `"archive"` | Written into `upstream.on_rewrite`. |
| `follow_interval` | `"10m"` | Written into `upstream.follow_interval` (backstop; pushes are nudged, §B.8). |
| `read_only` | `true` | Publish a deny-all `policy.json` at creation, so only follow (which bypasses policy, D33) moves refs. |
| `max_repo_size` | `"2GiB"` | Repositories larger than this by GitHub `size` are `too-large`: listed in state and logged with the `floe import` recipe (§B.7.1, handoff), never auto-created. Applies to explicit `repos` too. `0` = no limit. |
| `max_new_per_pass` | `20` | Bound on creations per pass (a 2,000-repository org arrives over several passes, and follow is not flooded). |
| `min_rate_remaining` | `200` | Stop a pass (incomplete) when `x-ratelimit-remaining` drops below this. |
| `gone_after` | `"24h"` | A repository must be missing/404 for this long (several passes) before it is marked `gone`. |
| `lease_ttl` | `"2m"` | TTL of `leases/mirror-github.pb`; heartbeat every `lease_ttl / 3`. |

`Config::validate`: `enabled` ⇒ `has_role(Maintain)`; `prefix` charset; globs well-formed (exactly one `/`);
`api_url`/`git_url` are http(s); `follow` is non-empty and parses (§A.2); the `include_private` rule above.
`maintenance.follow_interval == 0` (follow off on this host) gets a **warning** when the mirror is enabled:
nudges still run follow ops here, but no backstop round ever runs, and the mirror cannot tell whether another
host follows the repositories. The default (30 s) is what the mirror needs and is silent.

**Selection, in evaluation order** (`select.rs`, pure; the first rule that decides wins). A candidate carries
where it came from: `explicit` (in `repos`), or `listed` (from `users`, `orgs` or `starred`). A repository that
is both is `explicit`.

1. `exclude` matches ⇒ out.
2. `private` and not `include_private` ⇒ out (explicit too: the flag is a safety rule, not a filter).
3. Not explicit, and (`archived` ∧ `skip_archived`, or `fork` ∧ `skip_forks`) ⇒ out. This applies to starred
   repositories as to any listed one; explicit `repos` bypass the skips.
4. Not explicit, and no `include` glob matches ⇒ out. Explicit `repos` bypass `include`.
5. `max_repo_size` exceeded ⇒ `too-large` (selected, never auto-created).
6. Otherwise selected.

| candidate | exclude | private / include_private=false | archived or fork (skip=true) | include no match | result |
|---|---|---|---|---|---|
| explicit | yes | – | – | – | out |
| explicit | no | yes | – | – | out |
| explicit | no | no | yes | yes | **in** |
| listed (owner/org) | no | no | yes | – | out |
| listed (starred) | no | no | fork | – | out |
| listed (any) | no | no | no | yes | out |
| listed (any) | no | no | no | no | **in** |

`select.rs` tests this table row for row, plus the default config (`include = ["*/*"]`, `users = ["@me"]`
selects every non-archived, non-fork repository the token owns).

### B.5 Naming: GitHub → floe `RepoId`

**Pending the owner's sign-off** (R1). The requirement says `<prefix>/<owner>/<repo>`. floe identity is
exactly two segments, `<owner>/<repo>` (D5, `RepoId`), and routing is by those two segments (D26). A third
segment would change both, so this document proposes **`gh/acme/widgets` → `gh-acme/widgets`**. Only
`naming.rs` encodes it, so the rest of §B does not wait for the answer:

- `owner = format!("{prefix}-{gh_owner}")`, lowercased (GitHub names are case-insensitive, so lowercase is
  lossless for identity). GitHub owners are `[A-Za-z0-9-]{1,39}`, so a fixed prefix plus `-` is unambiguous.
- `name = gh_name` lowercased. GitHub allows a leading `.` (`.github`), and floe does not. A leading `.` maps to
  `_.` (`.github` → `_.github`).
- Wiki: `name + ".wiki"`.
- **Collision** (the mapped id already exists and is not ours, i.e. its `upstream.source` ≠ `github:<id>`, or
  two GitHub repositories map to one name): use `<name>--<id>`. If that also exists and is not ours, the state
  entry is `conflict` and the mapping is skipped with a `warn!`. The mapping is computed **once, at creation**,
  and stored in state. A rename never changes it (§B.9).
- **Length**: `RepoId` parts are 1..=100 characters (`validate_part`, floe-git lib.rs ~177). The owner is at
  most 16 + 1 + 39 = 56, always valid. A GitHub name can be 100 characters, so `<name>.wiki` (≤ 105) and
  `<name>--<id>` (≤ ~112) can overflow. Rule: when a mapped name exceeds 100, cut the base name so that the
  result with its suffix is exactly 100 (`<name[..k]>--<id>`, with `.wiki` after it for a wiki). The id makes
  the cut name unique. A name `RepoId::new` still rejects is `conflict` with the error as `last_error`.

`naming.rs` is pure, and it is table-tested, including `RepoId::new` acceptance of every output, a
100-character GitHub name, its wiki, and its collision form.

### B.6 State in the bucket

Two root-level objects (not under `repos/`, so `Registry::list` never sees them; next to `maintain/` and
`github/integrations.json`):

**`mirror/github/state.json`**: the reconciler's durable memory, CAS'd by version:

```json
{
  "version": 1,
  "generation": 42,
  "updated_at": "2026-10-04T12:00:00Z",
  "holder": "<instance id of the last writer>",
  "token_login": "alice",
  "last_pass": { "started_at": "…", "finished_at": "…", "complete": true, "created": 3, "updated": 1, "errors": 0 },
  "repos": {
    "123456789": {
      "full_name": "Acme/Widgets",
      "floe": "gh-acme/widgets",
      "wiki_floe": null,
      "status": "active",
      "private": true, "archived": false, "fork": false,
      "default_branch": "main",
      "pushed_at": "2026-10-04T11:58:01Z",
      "size_kb": 5120,
      "first_seen": "…", "last_seen": "…", "missing_since": null, "last_lookup": null,
      "settings_sha": "<sha256 of the [upstream] table the mirror last published>",
      "settings_revision": 3,
      "last_error": null
    }
  }
}
```

`status` ∈ `active` | `too-large` | `excluded` (no longer selected; frozen) | `gone` (404 for `gone_after`;
frozen) | `forbidden` | `conflict` | `detached` (a human edited `[upstream]`, §B.7) | `error`. No status is
terminal: §B.9 lists the way back out of each.

**Absence bookkeeping** (`missing_since`, `last_lookup`):

- `missing_since` is set to the pass's start on the **first complete discovery** that lacks the id, whether
  or not the id is looked up in that pass, and cleared as soon as any discovery or lookup finds it selected
  again. An incomplete discovery neither sets nor clears it.
- Lookups are bounded to 50 per pass. Candidates are the ids with `missing_since` set, ordered by
  `last_lookup` ascending (never looked up first), then by `missing_since`. So 2,000 removed stars are all
  looked up within 40 passes, round-robin, and none starves. `last_lookup` records the pass time.
- An id not looked up this pass keeps its status; `gone`/`forbidden` need a Gone/Forbidden lookup result
  **and** `missing_since` older than `gone_after`. `excluded` needs a Found lookup (§B.9).

**`mirror/github/http-cache.json`**: `{ "<url>": { "etag": "…", "next": "<rel=next url>" | null, "body":
<projected JSON> } }`, only the fields of `model.rs`. `next` is the page's `Link rel="next"` from its last 200,
so a 304 page (whose headers GitHub does not promise to repeat) can still be paginated past (§B.11). This is a
**cache**: losing it costs rate limit, nothing else (principle I). It is overwritten without CAS at the end of a
pass (`PutMode::Overwrite`, added to principle II's list, §D.6).

CAS: add `floe_store::coord::cas_update_json<T: Serialize + DeserializeOwned>(store, key, max_retries, f)`
next to `cas_update`, with the same loop shape (`get_bytes` → `f(Option<&T>)` → `put_bytes(Create |
Update(version))`, re-read on 412, jittered backoff on `Retryable`). The reconciler writes state **after every
action batch of ≤ 10 actions** and at the end of the pass, so a crash mid-pass loses the state records of at
most 10 actions. Every action is idempotent and re-derivable from the floe side, including a publish whose
state record was lost (§B.7.1, the `author` rule). Only the lease holder writes, so the CAS is a safety net against a
lost lease, not a hot contention point. A 412 after retries aborts the pass (`warn!`), and the next holder
re-plans.

### B.7 Repository creation and settings (exact calls)

The floe side is a trait, so the planner and applier test without a server:

```rust
#[async_trait::async_trait]
pub trait Target: Send + Sync {
    async fn exists(&self, id: &RepoId) -> anyhow::Result<Option<ExistingRepo>>; // settings toml + revision + author + upstream.source + head_seq
    async fn create(&self, id: &RepoId) -> anyhow::Result<CreateOutcome>;          // Created | AlreadyExists
    async fn publish_upstream(&self, id: &RepoId, upstream: &toml::Table, expected_revision: u64) -> anyhow::Result<PublishOutcome>; // Published(rev) | Conflict
    async fn put_policy_if_absent(&self, id: &RepoId, policy_json: &[u8]) -> anyhow::Result<()>;
    /// Ask the maintaining host to follow this repository now. Best effort; a no-op where unsupported (CLI).
    fn nudge_follow(&self, id: &RepoId);
}
```

`WalTarget` (in `floe-mirror`, used by both the server and the CLI) implements it with existing functions:

| Step | Call |
|---|---|
| exists | `registry.open(&id)` → `NotFound` ⇒ `None`; else `handle.sync_refs()` then `handle.settings()` (D24 settings ride on the manifest, so this is a refs-level sync with no pack I/O) |
| create | `registry.create(&id, floe_git::ObjectFormat::Sha1)` (GitHub is SHA-1; `git.object_format` is ignored on purpose) → `WalError::AlreadyExists` ⇒ `AlreadyExists`. Today `Registry::create` returns `Ok` for a handle it has cached (registry.rs ~192), so a repository opened earlier on this instance would read as `Created`. It changes to return `AlreadyExists` for a cached handle too, matching its doc comment; its callers (`admin.rs`, `floe repo create`, tests) create once per id and want that answer anyway |
| settings | parse the current `RepoSettings.toml` (empty when none) into `toml::Table`, replace only the `upstream` table with the mirror's (§B.7.1), serialize, then **`handle.publish_settings_if(text, "github-mirror", "floe github mirror: <reason>", expected_revision)`** |
| policy | `store.get_bytes(floe_proto::keys::policy_key(o, n))`; absent ⇒ `put_bytes(.., PutMode::Create)`. The same object `crate::policy::save` writes; `Create` makes it never overwrite a human's file |
| nudge | server: closure (§D.2); CLI: no-op |

**New floe-wal method** (small, in `handle.rs` + `publish::publish_settings_impl`): `publish_settings_if(toml,
author, message, expected_revision: u64) -> Result<u64, WalError>`. It is the same loop, but after the refs
sync it compares `manifest.settings.revision` (0 when none) with `expected_revision` and returns the new
variant `WalError::SettingsConflict { expected: u64, actual: u64 }` on mismatch, without retrying (`WalError`
has no generic conflict variant today; `RefConflict` is about refs). `WalTarget` maps it to
`PublishOutcome::Conflict`. `publish_settings` stays as it is (HTTP API). This makes
the mirror's read-modify-write safe against a concurrent human `PUT …/api/settings`.

#### B.7.1 The `[upstream]` table the mirror owns

```toml
[upstream]
source = "github:123456789"
git = "https://github.com/acme/widgets.git"
lfs = "https://github.com/acme/widgets.git/info/lfs"     # when github_mirror.lfs
follow = ["refs/heads/*", "refs/tags/*"]
on_rewrite = "archive"
head = "refs/heads/main"                                   # from default_branch; omitted for an empty repository
follow_interval = "10m"
```

- Other sections (`[bundles]`, `[maintenance]`, `[compaction]`) are the operator's and are preserved byte-for-
  byte in meaning (re-serialized through `toml::Table`).
- **Ownership** (`ExistingRepo` is ours), checked before any publish:
  1. `upstream.source == "github:<id>"` ⇒ ours.
  2. Otherwise, the repository was never written (`head_seq == 0`: no settings, no refs) **and** its name is
     the one this entry maps to ⇒ ours. This is the crash between `Create` and the first `PublishUpstream`,
     where the marker is not written yet. An empty repository that a human created under a `gh-*` name in that
     window is the only false positive, and it holds no data.
  3. Otherwise **never** modified (requirement 3): `conflict`, and naming picks `--<id>`.
- **Human edits win**, decided by who wrote the settings, not by a hash alone. Before publishing, for a
  repository that is ours:
  1. The current `[upstream]` table's canonical hash equals `settings_sha` in state ⇒ unchanged; publish if the
     rendered table differs.
  2. Else the current `RepoSettings.author == "github-mirror"` ⇒ the mirror wrote it, and the state record of
     that publish was lost (a crash between `PublishUpstream` and the state CAS, §B.6). Re-adopt: record its
     hash and revision as `settings_sha`/`settings_revision`, then continue as in 1. The same applies when the
     current table equals the one the mirror would render now, whoever the author is.
  3. Else, when `settings_sha` is null and either the repository has no settings yet (ownership rule 2) or
     the source marker matches (an operator prepared the repository for the mirror: the too-large handoff
     below) ⇒ adopt: publish the rendered `[upstream]` table over it.
  4. Else a human changed `[upstream]`: the entry becomes `detached`, the mirror logs once and never touches
     that repository's settings again until an operator runs `floe github adopt <floe repo>` (post-MVP; for
     the MVP, deleting the state entry re-adopts it through rule 3).
  An operator who edits only `[bundles]`/`[maintenance]`/`[compaction]` changes the author but not the
  `[upstream]` hash, so rule 1 still holds and the repository stays managed.
- **Too-large handoff** (`max_repo_size`): the logged recipe is (1) `floe import` into the entry's mapped name
  on an SSD host, then (2) `floe repo settings set <floe repo>` with `[upstream] source = "github:<id>"` (the
  recipe prints the exact line). The next pass finds the mapped repository with a matching marker and
  `settings_sha = null`, adopts it (rule 3), sets `status = active` and nudges follow, which fetches only the
  delta since the import. No `adopt` command is needed for this.

#### B.7.2 Read-only policy (when `read_only`)

```json
{ "version": 1, "rules": [ { "name": "github-mirror-read-only",
  "_comment": "managed by floe github mirror; follow bypasses policy (D33)",
  "match": { "refs": ["refs/**"] },
  "effect": { "protect": { "restricts": ["create", "update", "delete", "force-push"] } } } ] }
```

Pushing to a mirror would otherwise be undone (and archived) by the next follow round. Refusing it with
`rejected by rule 'github-mirror-read-only'` is the honest answer. The test pins this file through
`policy::parse_document` + `evaluate`.

### B.8 Reconcile loop, lease and placement in `floe-server`

```rust
pub async fn run_loop(target: Arc<dyn Target>, source: Arc<dyn Source>, store: DynStore,
                      cfg: Arc<Config>, recorder: Arc<dyn floe_catalog::Recorder>);
pub async fn reconcile_once(/* same */, opts: PassOptions { dry_run: bool }) -> anyhow::Result<PassReport>;
```

- **Lease**: `leases/mirror-github.pb` (`leases/mirror-<kind>.pb`) at the bucket root (store root, not a repo
  prefix), via `floe_store::coord::try_acquire(store, key, instance_id(), "github-mirror", lease_ttl)` and
  `LeaseGuard::spawn_heartbeat(guard, lease_ttl/3, lease_ttl)`. Exactly one reconciler fleet-wide. A host that
  does not get the lease sleeps `interval` and tries again. If the heartbeat reports a lost lease, the pass
  aborts before its next action. `LeaseGuard`'s `released: AtomicBool` is private today, and reading it through
  the heartbeat's `Arc<Mutex<LeaseGuard>>` would wait behind a heartbeat PUT in flight. So floe-store gains a
  small change (§0.3): the field becomes `Arc<AtomicBool>` and `LeaseGuard::released_flag(&self) ->
  Arc<AtomicBool>` hands out a clone before `spawn_heartbeat` takes the guard. The reconciler checks it
  lock-free between actions.
- **Loop**: tick every `interval` (first tick 10 s after start, jittered ±10%); `floe_wal::tasks::draining()`
  ⇒ release the lease and return (D31 phase 1). One pass:
  1. load state (+ http cache);
  2. `source.discover(sel)`, filter with `select.rs`;
  3. for ids in state but not discovered: set `missing_since` (complete discovery only), then
     `source.lookup(id)` for at most 50 of them, in the order §B.6 gives;
  4. `registry`-side facts via `target.exists` for new mappings only;
  5. `plan.rs` → `Vec<Action>` (`Create`, `PublishUpstream{reason}`, `PutPolicy`, `Nudge`, `MarkStatus`);
  6. `apply.rs`, in action order, CAS state every ≤ 10 actions;
  7. record `SyncRun{kind: "discovery"}` + `InventoryRecord`s for changed entries (§C.6).
- **Nudge on change**: for every `active` repository whose `pushed_at` changed since state (or that was just
  created), call `target.nudge_follow(id)`. In the server, the nudge closure runs
  `crate::ops::start(state, id, "follow", {})` **only when `cfg.placement.maintains(owner, name)` on this host**
  (D28/D30). Otherwise it does nothing, and the maintaining host's follow loop picks the change up within
  `upstream.follow_interval`. `ops::start` joins a running `follow` task for the same repository, and the
  follow loop starts its rounds through the same task lock and no longer fetches outside it (§A.4), so a nudge
  never runs a second fetch against the repository's scratch.
- **Server placement**: spawned in `floe-cli/src/serve.rs` next to `follow::run_loop`, inside `if maintainer`,
  when `cfg.github_mirror.enabled`. It is its own loop, never a unit of the priority loop (same reasoning as
  D33: discovery must not wait behind a base rebuild).

### B.9 Source-side changes

| Event at GitHub | Detected by | Mirror action |
|---|---|---|
| New repository matching the selection | discovery | `Create` → `PutPolicy` → `PublishUpstream("created")` → `Nudge`; `status=active` (or `too-large`) |
| Push | `pushed_at` changed | `Nudge` |
| Default branch changed | `default_branch` changed | `PublishUpstream("default branch")` (`head`) |
| Renamed / transferred, still selected | same `id`, new `full_name`, passes §B.4 selection | floe name **unchanged** (stable URLs, no floe rename exists); `PublishUpstream("renamed")` with the new `git`/`lfs` URLs; `full_name` updated; `info!` |
| Transferred to an owner outside the selection | absent from a complete discovery, `lookup` = Found under a new `full_name` | Selection decides (§B.4) on the new `full_name`: `excluded` (frozen, below) unless the id is in explicit `repos` by its old name, in which case it stays `active` and is handled as a rename. `exclude` still wins |
| Archived | `archived` became true | Keep following (nothing moves, and the cost is one `ls-refs` per `follow_interval`); `PublishUpstream` with `follow_interval = "24h"`; inventory row |
| Unarchived | `archived` became false | restore `follow_interval` |
| Visibility public ↔ private | `private` changed | state + inventory only (floe has no per-repo read ACL, see Risks R2). If access is lost, it is handled as below |
| No longer selected (exclude added, star removed) | absent from a **complete** discovery (or present but rejected by §B.4), `lookup` = Found | `status=excluded`; **frozen**: `PublishUpstream` with `follow = []` (valid: an empty list is follow off, §A.1; data kept, follow stopped) |
| Deleted / access revoked | absent from a complete discovery, `lookup` = Gone/Forbidden, `missing_since` older than `gone_after` (§B.6) | `status=gone`/`forbidden`; frozen as above. Never deleted in floe |
| `excluded` / `gone` / `forbidden` → selected again (star re-added, exclude removed, access restored, repository restored) | present and selected in a discovery, or `lookup` = Found and selected | `status=active`; `missing_since` cleared; `PublishUpstream("resumed")` restoring the configured `follow` (and URLs, `head`); `Nudge`. The floe name is the stored one |
| `too-large` → under the limit (`max_repo_size` raised, or the repository shrank) | selected and size ≤ limit | handled as a new repository: `Create` → … → `Nudge`, `status=active`. A `too-large` entry whose mapped repository now exists with a matching marker is adopted (§B.7.1 handoff) |
| `conflict` → the blocking repository is gone, or became ours | `target.exists` on the stored candidate names, re-checked every pass for `conflict` entries (cheap: refs-level, and conflicts are rare) | re-run naming from scratch (the plain name first) and proceed as a new repository |
| `error` | the failed action | retried every pass; the status follows from the next successful plan |
| `detached` | — | stays until an operator re-adopts (§B.7.1). Freeze/rename rows do not apply to it: the mirror never publishes there |
| Discovery incomplete (rate limit, 5xx) | `complete=false` | no absence-based action this pass; lookups and reverse transitions still apply to what was seen |
| Token invalid (401 on `/user`) | discovery error | pass fails; nothing changes; `error!` naming `token_env` |

### B.10 Wikis and LFS

- **Wikis** (`wikis = true` and `has_wiki`): a second floe repository `<prefix>-<owner>/<name>.wiki` with
  `upstream.git = https://github.com/<owner>/<name>.wiki.git`, `follow = ["refs/heads/*"]`, same policy.
  GitHub reports `has_wiki = true` even when the wiki has no pages, and then the wiki git URL is a 404. Follow
  reports `fetch failed`. The mirror sees the `failed` SyncRun only through the catalog, so it does not react.
  The cost is one failing `ls-refs` per `follow_interval`, and the MVP accepts it (R9).
- **LFS**: `upstream.lfs` makes LFS **read-through** (`docs/LFS.md` §2): objects are fetched on first download
  and persisted. They are **not** prefetched. If the GitHub repository is deleted before an object was ever
  downloaded, that object is lost. This is the one place where "nothing is ever lost" does not hold in the MVP
  (R5). A post-MVP `lfs_prefetch` unit (walk new commits for pointer files, batch-download) closes it.

### B.11 GitHub REST usage (`github/http.rs`)

Headers on every request: `Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`,
`User-Agent: floe-mirror/<version>`, `Authorization: Bearer <token>`, `If-None-Match: <etag>` when cached.

| Purpose | Endpoint |
|---|---|
| Token identity (`@me`, sanity) | `GET /user` |
| `users = ["@me"]` | `GET /user/repos?affiliation=owner&visibility=all&per_page=100&sort=full_name` |
| `users = ["other"]` | `GET /users/{u}/repos?type=owner&per_page=100&sort=full_name` (public only; GitHub's rule) |
| `orgs` | `GET /orgs/{org}/repos?type=all&per_page=100&sort=full_name` |
| `starred` | `GET /user/starred?per_page=100` / `GET /users/{u}/starred?per_page=100` |
| `repos` and `lookup` | `GET /repos/{owner}/{name}`; by id `GET /repositories/{id}` (follows renames/transfers) |

- **Pagination**: follow `Link: <…>; rel="next"` to the end. Each page URL has its own ETag entry. A 304 page
  reuses the cached projection, and its next page is the response's `Link rel="next"` if present, else the
  cached entry's `next`. A 304 with neither (an entry from an older cache, say) marks the discovery
  incomplete; it is never treated as the last page. Every page is requested every pass (a 304 does not count
  against the primary rate limit for authenticated requests).
- **Rate limit**: read `x-ratelimit-remaining` / `x-ratelimit-reset` from every response into `ApiStats`. Below
  `min_rate_remaining`, stop (incomplete pass). A `403` or `429` is `SourceError::RateLimited{until}` when
  **any** of these holds: it has `retry-after` (`until = now + retry-after`); `x-ratelimit-remaining = 0`
  (`until = x-ratelimit-reset`); or the body's `message` or `documentation_url` mentions a secondary rate limit
  (`"secondary rate limit"`, `…#secondary-rate-limits`) — GitHub's secondary-limit 403 often carries neither
  header and has `remaining > 0`, so then `until = now + 60 s` at least (GitHub's guidance), doubled on each
  consecutive secondary hit up to 15 min. The pass ends incomplete and the loop sleeps until `until` (capped at
  1 h). Requests are strictly sequential (no concurrency against the API), which avoids most secondary limits.
- **Errors**: 5xx/timeouts retry twice with jittered backoff (`floe_store::util::backoff` shape), then mark the
  pass incomplete. 401 = `Unauthorized` (pass fails). On `lookup`, 404 = `Gone` and a 403 that is **not**
  rate-limited by the rule above (an access-denied message) = `Forbidden`. A rate-limited lookup never counts as
  `Gone`/`Forbidden`; it ends the pass and the id keeps its status.
- Client: `reqwest` (workspace, rustls), 30 s timeout, built once (a `reqwest::Client::builder()` error is a
  startup `Err`, never `expect`).

### B.12 CLI: `floe github …`

```
floe --config floe.toml github sync [--once] [--dry-run]
floe --config floe.toml github status [--json]
```

- `sync` without `--once` runs `run_loop` in the foreground (the same lease, so it and a server never reconcile
  together). `--once` acquires the lease (waits up to `lease_ttl`; otherwise exit 3, naming the holder and
  expiry), runs `reconcile_once`, prints the `PassReport`, then releases. `--dry-run` prints the plan
  (`create gh-acme/widgets ← Acme/Widgets (private, 5.0 MiB)`) and changes nothing (no lease, no writes).
  Follow is not run by the CLI (`nudge_follow` is a no-op): the maintaining host picks new repositories up
  within one `follow_interval`. To force it, run `floe repo …` ops on that host.
- `status` reads `mirror/github/state.json` and prints counts per status, the last pass, and every non-`active`
  entry with its reason.
- Module `crates/floe-cli/src/github_cmd.rs`. The subcommand is `Command::Github { action: GithubAction }`.
  The token check happens before the lease (`token_env` unset ⇒ exit 2 with the variable's name).

### B.13 Metrics and logging

- `floe_mirror_pass_total{source, outcome="ok"|"incomplete"|"failed"|"lease-held"}`,
  `floe_mirror_pass_seconds{source}` (histogram), `floe_mirror_repos{source, status}` (gauge, set at pass end),
  `floe_mirror_actions_total{source, action}`, `floe_mirror_api_requests_total{source, status="200"|"304"|"4xx"|"5xx"}`,
  `floe_mirror_rate_remaining{source}` (gauge).
- Logs: one `info!` per pass (counts, `elapsed_ms`, `complete`), one `info!` per action, `warn!` per conflict
  or detachment, `error!` on token failure. Never log the token or a URL with credentials (there are none:
  the token is only in the `Authorization` header and git's one-shot helper).

### B.14 Tests (§B, no network)

- `select.rs`, `naming.rs`, `settings.rs` (merge preserves other sections, hash stability), `plan.rs`: pure table
  tests, covering every row of §B.9 (reverse transitions included) as a `(state, discovery, lookup) →
  actions` case, plus "incomplete discovery never marks gone", "human-edited upstream ⇒ detached, no publish",
  "author github-mirror with a stale `settings_sha` ⇒ re-adopted, not detached", "transfer out of the
  selection ⇒ excluded", "lookup order is oldest `last_lookup` first", and the §B.4 selection table.
- `FakeSource` (scripted `Discovery`/`Lookup` per pass) + `WalTarget` over `MemoryStore` (real `Registry`, no
  server): `reconcile_creates_repos_with_settings_and_policy`, `reconcile_is_idempotent` (second pass: zero
  actions, state unchanged), `crash_between_create_and_settings_is_repaired` (create done, state not written →
  next pass claims the empty repository and publishes settings), `crash_after_publish_before_state_cas_is_not_detached`
  (a rename publish lands, state CAS skipped → next pass re-adopts and does nothing else),
  `freeze_publishes_empty_follow` (through `WalTarget`, the settings validation accepts `follow = []`),
  `rename_keeps_floe_name_and_updates_url`, `max_new_per_pass_bounds_creations`,
  `existing_unmanaged_repo_is_never_touched`, `too_large_handoff_is_adopted`, `create_on_a_cached_handle_is_already_exists`.
- `github/http.rs`: an in-process axum stub on `127.0.0.1` (loopback, as `tests/follow.rs` does with a second
  floe): Link pagination, ETag 304 reuse **with and without** a `Link` header on the 304 (cached `next`),
  `retry-after` → `RateLimited`, secondary-limit 403 without headers and `remaining > 0` → `RateLimited` with
  ≥ 60 s, access-denied 403 on lookup → `Forbidden`, `x-ratelimit-remaining` floor → incomplete.
- Lease: two `run_loop`s over one `MemoryStore` → exactly one reconciles (assert on the state's `holder`).
- End to end (`crates/floe-server/tests/mirror.rs`): `FakeSource` pointing `git_url` at a second floe instance
  (as `tests/follow.rs`); the mirror creates the repository, the nudge runs the follow op, and refs arrive. This
  is the one test that crosses §A and §B.

### B.15 As landed (2026-10-04)

Where the code differs from §B.1–§B.14 (the code wins):

- **Not landed**: wikis (`wikis` key, §B.10) are left out entirely rather than added as a refused key; the
  `floe-catalog` `Recorder` is not on this branch, so `run_loop` takes an `on_pass: Fn(&PassReport)` hook and
  `PassReport.changes` carries the inventory changes; `floe_server::mirror::run_loop` passes a no-op until the
  catalog lands. `floe config check` does not print `[github_mirror]` yet. The end-to-end
  `crates/floe-server/tests/mirror.rs` (mirror → nudge → follow) is open.
- **Plan/apply shape**: the planner emits per-repository step lists (`Create { allow_create }`, `PutPolicy`,
  `Publish { reason }`, `Nudge`) instead of a flat `Vec<Action>` with `MarkStatus`; statuses are bookkeeping on
  the planned state. `Create { allow_create: false }` is the too-large handoff check (adopt a repository carrying
  the marker, never create). Ownership rule 2 (an empty, never-written repository is ours) applies only when the
  mirror may create. A new repository beyond `max_new_per_pass` is not recorded in the state at all.
- **State CAS**: `state::save` is `cas_update_json` guarded by `generation` (a write whose stored generation is
  not the pass's aborts), every ≤ 10 applied steps and at the end. `floe_store::coord` also gained `get_json`.
- **Source**: `Source` has no `wiki_url`; `Discovery` carries `login`; `ApiStats` carries
  `rate_limited_until` (the loop sleeps until then, capped at 1 h); `SourceError::NoToken(var)` is the missing
  env var. Lookups are not counted in `ApiStats`. `GithubSource` reads the token at every pass.
- **Config**: `GithubMirrorConfig::validate` runs only when `enabled`. The `maintenance.follow_interval = 0`
  warning is logged by `run_loop` at start, not by `validate` (which runs on every settings merge). The derived
  `token_env_by_host` default is `Config::derive_mirror_token_env`, called from `Config::load`.
- **CLI**: `floe github sync` without `--once` runs `run_loop` in the foreground (its first pass after the
  ~10 s jittered first tick); `--once` waits up to `lease_ttl` and exits 3 naming the holder.

---

## C. `floe-catalog` (D50)

### C.1 Crate and features

```
crates/floe-catalog/
  Cargo.toml   # [features] iceberg = ["dep:iceberg", "dep:iceberg-catalog-rest", "dep:iceberg-storage-opendal", "dep:arrow-array", "dep:arrow-schema", "dep:parquet"]
  src/lib.rs        # always: rows, Recorder trait, NoopRecorder, parse_follow_archived, CatalogConfig re-export
  src/rows.rs       # RefEventRow, ForcePushRow, SyncRun, InventoryRecord (+ Arrow schema behind feature)
  src/buffer.rs     # always: bounded group-commit buffer, flush policy (unit-tested without iceberg)
  src/iceberg.rs    # #[cfg(feature = "iceberg")] IcebergWriter: connect, ensure tables, append
  src/schema.rs     # #[cfg(feature = "iceberg")] Iceberg schemas + partition specs
```

- Feature chain: `floe-catalog/iceberg` ← `floe-server/catalog` ← `floe-cli/catalog`. The **default build
  compiles only the core** (serde types and a channel; no arrow, no parquet). `floe-mirror` and `floe-server`
  depend on the core unconditionally, so call sites have no `cfg`.
- Versions (crates.io, checked 2026-10-04): **`iceberg = "0.10.1"`**, **`iceberg-catalog-rest = "0.10.1"`**,
  **`iceberg-storage-opendal = "0.10.1"`** (default features `opendal-s3`, `opendal-memory`, `opendal-fs`; in
  0.10 the storage backends moved out of `iceberg` into this crate). They pull `arrow-* = "58"` and
  `parquet = "58"`, which `floe-catalog` pins to the same major. `iceberg` 0.10.1 declares **MSRV 1.94**. The
  toolchain is 1.97.1, but `workspace.package.rust-version = "1.90"`. `floe-catalog` declares no
  `rust-version` of its own: the default (featureless) build is a dependency of every `floe` binary and keeps
  the 1.90 floor, and `iceberg`'s own manifest enforces 1.94 when the `catalog` feature is on. Pin exact versions (`=0.10.1`): the project is pre-1.0, and its
  writer API has changed between minors.
- Startup: `catalog.enabled = true` in a binary built without the feature is a **fatal config error**
  (`floe-server` checks `cfg!(feature = "catalog")` in `AppState::new`; fail closed, §1.3 style), and the
  message names the build flag.

### C.2 Config: `[catalog]` (`floe-config`, always present so config parsing never depends on features)

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | |
| `uri` | — (required when enabled) | Iceberg REST catalog base URL. RustFS S3 Tables: its Iceberg REST endpoint (see R6). |
| `warehouse` | — (required) | Warehouse identifier (S3 Tables: the table bucket name/ARN the endpoint expects). |
| `namespace` | `"floe"` | Iceberg namespace; created if absent. |
| `token_env` | unset | Env var with a bearer token for the catalog (`Authorization: Bearer`). |
| `credential_env` | unset | Env var with `client_id:client_secret` for the REST OAuth2 client-credentials flow. |
| `s3_endpoint` | unset | FileIO endpoint for data files when the catalog does not vend credentials (RustFS: `http://rustfs:9000`). |
| `s3_region` | `"us-east-1"` | |
| `s3_access_key_env` / `s3_secret_key_env` | `"AWS_ACCESS_KEY_ID"` / `"AWS_SECRET_ACCESS_KEY"` | Env var *names* (D43 style; never values in config). |
| `s3_path_style` | `true` | RustFS/MinIO addressing. |
| `flush_interval` | `"30s"` | Max age of buffered rows before a commit. |
| `flush_rows` | `5000` | Commit when this many rows are buffered (per table). |
| `max_buffer_rows` | `100000` | Bound on buffered rows. Beyond it, durable appends fail fast (lag stays in the WAL) and lossy telemetry is dropped. |
| `commit_timeout` | `"60s"` | Bound on one commit (and one connect attempt): a durable append not committed within `flush_interval` + this fails (cursor not advanced). |
| `backfill` | `false` | First cursor for a repository that existed **before** the catalog was first enabled: `false` = its current `head_seq` (start now); `true` = the retained log start (full history, like the events bridge). A repository created after that always starts at its retained log start, so its whole history (a mirror's initial import included) reaches `ref_events` (§C.4). |
| `create_tables` | `true` | Create namespace and tables if missing; `false` = fail if they are missing. |

### C.3 Table schemas

All timestamps are `timestamptz` (µs, UTC). Field ids are fixed as listed, and they are append-only
(schema evolution adds fields, never reuses ids). `identifier` marks the dedup key, which is declared as
Iceberg identifier fields for documentation and not enforced by appends.

**`ref_events`** — every ref transition committed to any repository's WAL (own and mirrored). Source: WAL via
the bridge (§C.4). Partition: `day(committed_at)`. Sort order: `repo, seq`.

| id | column | type | notes |
|---|---|---|---|
| 1 | `repo` | string, required, identifier | `owner/name` |
| 2 | `seq` | long, required, identifier | WAL seq |
| 3 | `ref_name` | string, required, identifier | |
| 4 | `action` | string, required | `create`/`update`/`delete` (EVENTS.md) |
| 5 | `ref_type` | string, required | `branch`/`tag`/`""` |
| 6 | `old_oid` | string, required | full zero OID on create (EVENTS.md convention) |
| 7 | `new_oid` | string, required | full zero OID on delete |
| 8 | `principal` | string, required | `meta.principal` (`upstream` for follow) |
| 9 | `request_id` | string | |
| 10 | `entry_kind` | string, required | `push`/`ref_update` |
| 11 | `writer` | string | log entry writer instance |
| 12 | `committed_at` | timestamptz, required | `LogEntry.created_at` |
| 13 | `ingested_at` | timestamptz, required | catalog write time |
| 14 | `is_archive` | boolean, required | `ref_name` starts with `refs/archive/` |
| 15 | `upstream` | string | `meta.upstream` when principal = upstream |

**`force_push_log`** — every rewrite/delete follow archived (§A.5). Source: WAL `meta["follow.archived"]`.
Partition: `month(committed_at)`.

| id | column | type | notes |
|---|---|---|---|
| 1 | `repo` | string, required, identifier | |
| 2 | `seq` | long, required, identifier | |
| 3 | `ref_name` | string, required, identifier | original ref |
| 4 | `kind` | string, required | `rewrite` / `delete` |
| 5 | `old_oid` | string, required | the archived tip |
| 6 | `new_oid` | string | null on delete |
| 7 | `archive_ref` | string, required | `refs/archive/<ts>/<ref>` |
| 8 | `upstream` | string | |
| 9 | `committed_at` | timestamptz, required | |
| 10 | `ingested_at` | timestamptz, required | |

(Human force pushes through receive-pack are not in the MVP. Recording them would need `meta["forced"]` from
`receive.rs`, which already classifies `force-push` for policy. See R11.)

**`sync_runs`** — one row per follow round that did work, per follow failure, and per mirror pass. Source:
`Recorder` telemetry (lossy, §C.6). Partition: `day(started_at)`.

| id | column | type | notes |
|---|---|---|---|
| 1 | `run_id` | string (uuid v4), required | |
| 2 | `kind` | string, required | `follow` / `discovery` |
| 3 | `source` | string | `upstream.source` prefix (`github`), null for hand-configured follow |
| 4 | `repo` | string | null for `discovery` |
| 5 | `instance` | string, required | `instance_id()` |
| 6 | `started_at` | timestamptz, required | |
| 7 | `finished_at` | timestamptz, required | |
| 8 | `outcome` | string, required | follow: `in-sync`/`published`/`archived`/`refused`/`failed`; discovery: `ok`/`incomplete`/`failed` |
| 9 | `refs_published` | int | |
| 10 | `refs_archived` | int | |
| 11 | `bytes_fetched` | long | pack size ingested |
| 12 | `seq` | long | published seq |
| 13 | `detail` | string | human line (FollowStatus.detail / pass summary) |
| 14 | `api_requests` | int | discovery |
| 15 | `api_not_modified` | int | discovery |
| 16 | `rate_remaining` | int | discovery |

(Follow rounds that find nothing are **not** recorded. They would dominate the table and say nothing a metric
does not.)

**`repo_inventory`** — slowly changing record of every floe repository and its source. Sources: the mirror
(one lossy row per change of a state entry) + a **daily full snapshot taken by the catalog itself** (§C.6),
which runs whether or not the mirror is enabled, so own repositories (`source = 'own'`) are always present.
Partition: `day(observed_at)`.

| id | column | type | notes |
|---|---|---|---|
| 1 | `observed_at` | timestamptz, required | |
| 2 | `change` | string, required | `snapshot`/`created`/`updated`/`renamed`/`archived`/`excluded`/`gone`/`detached`/`conflict` |
| 3 | `floe_repo` | string, required | |
| 4 | `source` | string, required | `github` / `own` |
| 5 | `source_id` | string | GitHub id |
| 6 | `full_name` | string | at the source |
| 7 | `status` | string | mirror status, `own` for own repos |
| 8 | `private` | boolean | |
| 9 | `archived` | boolean | |
| 10 | `fork` | boolean | |
| 11 | `default_branch` | string | |
| 12 | `upstream_url` | string | |
| 13 | `pushed_at` | timestamptz | |
| 14 | `size_kb` | long | |
| 15 | `head_seq` | long | snapshot rows only (manifest) |
| 16 | `snapshot_id` | string | snapshot rows only; the completed one is in `catalog/inventory.json` (§C.6) |

### C.4 Consuming the WAL (durable cursor, D32/D46/D47)

The catalog is a WAL reader on the events host with the bridge's cursor machinery (`Cursor`,
`read_log_retained`, D47 traversal) but **its own loop**, never a future inside `Bridge::catch_up`. Today
`catch_up` joins its targets with `tokio::join!` and propagates errors with `?`, `http_notify` awaits it inside
the HTTP request (503 on any error), and `sweep` drives it with `buffer_unordered(16)`. A catalog target in that
join would make every bucket notification wait for an Iceberg group commit (up to `flush_interval`), turn a
catalog outage into a 503 storm of redeliveries for targets that already succeeded, and hold the webhook sweep
to about 16 repositories per flush. So:

- New `crates/floe-server/src/catalog_tail.rs` (`CatalogTail`), started next to the bridge when `events`
  role ∧ `catalog.enabled`. Its own sweep every `events.sweep_interval` with its own concurrency (8), and
  its own wake channel: `object_finalized` sends the repository id to it with `try_send` (dropped when full;
  the sweep is the backstop) and **never awaits it**. The webhook's `catch_up`, notify response and sweep are
  unchanged.
- Cursor key: `repos/<o>/<r>/catalog/cursor.json`, the same `Cursor { published_seq, updated_at }` JSON. It is
  distinct from `floe_proto::keys::CATALOG` (`meta/repos.pb`), which is unrelated. Each target advances only
  its own cursor (the D46 rule).
- While the writer is down (`floe_catalog_up == 0`), `append_durable` fails at once with
  `CatalogError::Unavailable` (§C.5), so the tail costs a cursor GET per repository per sweep and nothing
  waits.
- Rows: the tail reads `&[LogEntry]` with `read_log_retained`, builds `RefEvent`s with the same
  `events::refs_from_entries` the webhook uses (it is `pub(crate)` in floe-server, and the tail lives in
  floe-server, so there is one implementation of the zero-OID, HEAD-skip and classify rules), and converts
  each `RefEvent` plus the `LogEntry` it came from (same seq: `principal`, `request_id`, `writer`,
  `created_at`, `meta.upstream`, entry kind) into a `RefEventRow`. `force_push_log` rows come from
  `floe_catalog::parse_follow_archived(&meta) -> Vec<ArchivedLine>` (pure, in the catalog core; it parses only
  the §A.5 format). Then `writer.append_durable(rows).await`, then CAS the cursor. The golden test that pins
  `ref_events` = webhook events lives in floe-server (`tests/events.rs`).
- **Cold cursor** (no `catalog/cursor.json` yet). The catalog's first enablement is recorded once in
  `catalog/epoch.json` at the bucket root (`{ "enabled_at": … }`, CAS `Create`; whoever wins, every host then
  reads the same value). For a repository without a cursor:
  - `backfill = true` ⇒ `handle.retained_log_start()` (checkpoint history, D47);
  - else, when the repository's oldest retained log entry was committed at or after `enabled_at` (it was created
    after the catalog was enabled: every mirror-created repository, for instance) ⇒ the retained log start, so
    its initial import is in `ref_events`;
  - else ⇒ the current `head_seq` (the catalog was enabled after this repository had history, and the operator
    did not ask for a backfill).
  The cursor is persisted before any delivery, as the bridge does. Reading the oldest entry is one log GET, on
  the cold path only.
- Latency ≈ notification latency + flush interval, or the sweep interval without notifications. **No
  `wake()` from writers** (that concession is the GitHub facade's alone).
- Semantics: **at-least-once**. A crash after the Iceberg commit and before the cursor CAS re-appends the
  batch, and so does a `ref_events` commit followed by a failed `force_push_log` commit. Consumers dedup on
  `(repo, seq, ref_name)`. Recommended views: `SELECT DISTINCT` or `ROW_NUMBER() … = 1` over the identifier.
  Exactly-once (watermarks committed atomically in table properties) is post-MVP (R10).

### C.5 Batching, flush and failure isolation (`buffer.rs`, core)

- One writer per process (`Arc<CatalogWriter>`), with one in-memory buffer per table.
- `append_durable(rows) -> Result<(), CatalogError>` (bridge path): enqueue with a oneshot, then wait for the
  commit that contains the rows (group commit, like publish's batch window). It fails fast with
  `CatalogError::Unavailable` while the writer has no live catalog connection (`floe_catalog_up == 0`), with
  `Backpressure` when `max_buffer_rows` would be exceeded, and with `Timeout` after `commit_timeout`. On any
  error the catalog tail leaves its cursor, so the WAL holds the backlog and memory does not. Only the catalog
  tail (§C.4) and the inventory snapshot (§C.6) call it; nothing on a webhook, git, sync, follow or mirror path
  does.
- `record_*` (telemetry path): `try_send` into the same buffer. When full, drop and count
  (`floe_catalog_dropped_total{table}`). This path never awaits.
- Flusher task: when a table has `≥ flush_rows` rows or its oldest row is `≥ flush_interval` old, do this per
  table: write one Parquet data file through the table's `FileIO` → `Transaction::fast_append` → `commit`.
  On a commit conflict (another instance appended), reload the table and retry ≤ 5 times with backoff. Then
  complete the waiters. Commits for different tables are independent. (Exact iceberg-rust 0.10 builder names
  are the implementer's to confirm: `ParquetWriterBuilder` → `DataFileWriterBuilder` → `fast_append`.)
- **Isolation**: the writer is the only code that talks to the catalog. It runs on the serving runtime but
  does only network I/O (Parquet encoding of ≤ `flush_rows` rows is small; move it to `spawn_blocking` if a
  flush exceeds 50 ms in the benchmark). Startup **never** waits for the catalog: `connect` happens in the
  flusher with retry and backoff (cap 5 min), and until it succeeds durable appends fail with `Unavailable`
  (lag) and telemetry drops (counted). Nothing on a git, sync, follow, mirror or webhook path awaits the writer.
- Metrics: `floe_catalog_rows_total{table}`, `floe_catalog_commits_total{table, outcome}`,
  `floe_catalog_commit_seconds{table}`, `floe_catalog_buffer_rows{table}`, `floe_catalog_dropped_total{table}`,
  `floe_catalog_up` (0/1). Bridge lag per target: `events_bridge_lag_entries{repo, target="catalog"}` (add the
  `target` label to the existing gauge).

### C.6 The `Recorder` seam (core, always compiled)

```rust
pub trait Recorder: Send + Sync {
    fn record_sync_run(&self, run: SyncRun);            // non-blocking, lossy
    fn record_inventory(&self, rec: InventoryRecord);   // non-blocking, lossy
}
pub struct NoopRecorder;                                  // default when catalog is off or not compiled
```

`AppState` gains `pub recorder: Arc<dyn floe_catalog::Recorder>` (`NoopRecorder` unless the feature is
compiled *and* `catalog.enabled`). Follow (§A.4, after the op) and the mirror (§B.8) call it. `SyncRun` and
`InventoryRecord` are plain structs with the columns of §C.3, so their field names are the contract.

Why `sync_runs`/`repo_inventory` are lossy and `ref_events`/`force_push_log` are not: the latter are derived
from the WAL and can always be replayed from a cursor. The former describe processes, not data (they are
metrics with more columns). Making them durable would need either a second store of truth (principle I) or
writing operational telemetry into the WAL. The daily inventory snapshot bounds what a loss can hide, so the
snapshot itself is **not** lossy:

- **Inventory snapshot** (`crates/floe-server/src/catalog_tail.rs`, next to the tail): once a day, on the
  events host that holds `leases/catalog-inventory.pb`, whether or not the mirror is enabled. Its schedule
  lives in `catalog/inventory.json` (`{ "last_snapshot": …, "snapshot_id": … }`, CAS), so restarts and lease
  handovers neither skip nor repeat a day. It walks `registry.list()` (the bridge's sweep already lists; this is
  a daily background walk, not a hot path) plus `mirror/*/state.json` when present, and emits `change =
  'snapshot'` rows: `source = 'own'` for repositories without a mirror entry.
- Rows go through `append_durable` in chunks of `flush_rows`, each awaited (backpressure, never `try_send`).
  `last_snapshot` is CAS'd only after the last chunk committed. A snapshot interrupted by an outage is retried
  whole the next time the lease holder runs it; rows of the partial attempt share a `snapshot_id` that
  consumers can ignore unless it completed (the completed id is the one in `catalog/inventory.json`). The
  column `snapshot_id` (string, id 16) is added to `repo_inventory` for this.

### C.7 Tests (§C)

Without a live catalog (run in `just test`):

- `parse_follow_archived` (catalog core): one `ArchivedLine` per line, `-` ⇒ delete, malformed lines skipped
  and counted, never a panic.
- Golden test in floe-server (`tests/events.rs`): PUSH with archive meta → one `ForcePushRow` per line and
  `is_archive` on the archive `RefEventRow`; HEAD retargets and SETTINGS/COMPACT emit nothing; `ref_events` rows
  equal the webhook's `RefEvent`s row for row (both come from `events::refs_from_entries`).
- `buffer.rs` with a fake committer (a trait object inside `buffer.rs`; the Iceberg committer is the real
  implementation): flush by rows, flush by age, backpressure, timeout, waiter completion, telemetry drop when
  full, a committer error fails the waiters and keeps the rows out.
- Catalog tail (`crates/floe-server/tests/events.rs`): with a fake writer, a failing catalog leaves
  `catalog/cursor.json` and still advances `events/cursor.json`, and the reverse;
  `notify_latency_is_independent_of_the_catalog` (a writer that never commits: `/_events/notify` still answers
  200 at webhook speed); `backfill=false` starts an old repository at head; `repo_created_after_enable_gets_its_history`
  (cold cursor at the retained start, the initial import is in the rows).
- Inventory snapshot: runs with the mirror disabled (own repositories as `source = 'own'`), chunks through
  `append_durable`, `last_snapshot` advances only after the last chunk, a lease handover does not repeat a day.
- `#[cfg(feature = "iceberg")]` schema tests: Arrow ↔ Iceberg schema round trip, field ids stable (snapshot of
  the schema JSON).

Against RustFS, `#[ignore]` (`just test-catalog`, needs `podman compose --profile catalog up`):
`appends_and_reads_back_all_four_tables`, which creates the namespace and tables, appends via the writer, scans
with `iceberg`'s reader, and asserts counts/values; and `concurrent_writers_retry_conflicts` (two writers, one
table).

### C.8 Implementation notes (2026-10-04, §C landed; the code wins)

Where `crates/floe-catalog` differs from or pins down §C.1–§C.7:

- **Feature deps**: `iceberg = ["dep:iceberg", "dep:iceberg-catalog-rest", "dep:iceberg-storage-opendal",
  "dep:arrow-array", "dep:reqwest"]`. No direct `parquet`/`arrow-schema`: the Parquet writer is built with
  `ParquetWriterBuilder::from_table_properties`, and schemas go through `iceberg::arrow`.
  `iceberg-storage-opendal` is built with only `opendal-s3`. `iceberg-catalog-rest` 0.10.1 builds `reqwest`
  **without a TLS backend**; `dep:reqwest` (workspace, rustls) is there only so feature unification makes an
  `https://` catalog URI work.
- **Conflict retry**: iceberg-rust 0.10's `Transaction::commit` already reloads the table and re-applies the
  append on a retryable conflict. floe does not add its own loop. It sets `commit.retry.num-retries = 5` on the
  tables it creates instead.
- **Partitions**: rows are split with `RecordBatchPartitionSplitter` and written through `FanoutWriter`, which
  gives one data file per partition per flush.
- **Schema check at connect**: `schema::check_compatible` requires every floe field id to exist in the
  catalog's current schema with the same name, type and requiredness. A catalog that reassigns field ids on
  create, or a table someone altered, fails `connect` loudly. The writer then stays down, with lag only.
- **Buffer** (`buffer.rs`): `max_buffer_rows` counts all tables **and rows in flight**. A failed commit fails
  its waiters, drops its rows and takes the writer **down until a reconnect succeeds**, so later appends fail
  fast with `Unavailable` instead of piling up. A durable waiter gives up after `flush_interval +
  commit_timeout` (the age wait, then the commit), so a small append never times out just because
  `flush_interval >= commit_timeout`. The flusher bounds a single commit at `flush_interval + 2 ×
  commit_timeout`, so the waiters' `Timeout` always fires first. One connect attempt is bounded by
  `commit_timeout` (the REST client has no request timeout) and counts as a failure with backoff when it
  elapses; `shutdown` interrupts a connect in progress. One append carries at most
  `max_append_rows() = min(flush_rows, max_buffer_rows)` rows (more is `Backpressure` even on an empty buffer). `ingested_at` is stamped by the writer at commit time and is not
  a row field.
- **Cursor** (`cursor.rs`): the catch-up itself lives in the catalog core as `cursor::catch_up(store, source,
  sink, cold_start)`. The server's `CatalogTail` implements `cursor::TailSource` (head, retained start,
  `committed_at(seq)`, rows of `(from, to]`) over a `RepoHandle` + `events::refs_from_entries`, and serializes
  catch-ups per repository. Rows are built with `floe_catalog::rows_for_entry(repo, entry, transitions)`
  (`RefTransition` is the `RefEvent` fields). `cursor::load_epoch` does the `catalog/epoch.json` create-once.
  While the writer is down, a catch-up returns `Unavailable` **before** the cursor GET, so it costs nothing on
  the bucket (§D.7's "cursor GET only" becomes "no request"). The range is read in windows of
  `WINDOW_ENTRIES` (256) entries, each delivered in appends of at most `max_append_rows()` rows (one entry with
  many rows, a mirror's initial import, is split), and the cursor is CASed after every window. A backlog larger
  than `max_buffer_rows` therefore drains, only one window is in memory, and a failure keeps the windows already
  committed. A lost cursor CAS ends the catch-up (the rest is the other tail's).
- **Throughput bound (kept)**: a catch-up whose last append is under `flush_rows` waits for the age flush, up
  to `flush_interval`, while holding one of the tail's concurrency slots. A sweep over N repositories that each
  changed a little takes about `N / concurrency × flush_interval` when nothing else fills the buffer (other
  tails' rows share the same group commit and shorten it). Lower `flush_interval`, or raise the tail's
  concurrency, for many small active repositories. An early flush for durable waiters is post-MVP.
- **Upstream URLs** in `ref_events.upstream` and `force_push_log.upstream` lose userinfo, query and fragment
  (`rows::redact_url`): a token placed in `[upstream] git` instead of `token_env` never reaches the tables.
- **`parse_follow_archived`** is generic over the map's hasher. It also rejects a line whose
  `refs/archive/<ts>/<ref>` does not end in its `<original_ref>`, or whose OIDs are not full hex.
- **Config**: `floe_config::CatalogConfig` (re-exported as `floe_catalog::CatalogConfig`) is validated by
  `Config::validate` when `enabled`: `uri`, `warehouse` and a non-empty `namespace` are required,
  `flush_rows > 0` and `max_buffer_rows >= flush_rows` must hold, and `flush_interval`/`commit_timeout` must
  be non-zero. The `cfg!(feature = "catalog")` startup check stays §D's.

---

## D. Wiring

### D.1 CLI (`crates/floe-cli`)

- `Command::Github { action: GithubAction }` with `Sync { once: bool, dry_run: bool }` and `Status { json: bool }`
  → `github_cmd.rs` (§B.12).
- `floe config check` prints the effective `[github_mirror]` and `[catalog]` and checks `token_env` presence
  (not the value).
- `crates/floe-cli/Cargo.toml`: `floe-mirror.workspace = true`; `[features] catalog = ["floe-server/catalog"]`.
- `floe.rs` doc line: add `github`.

### D.2 Server loops (`crates/floe-cli/src/serve.rs`, `crates/floe-server`)

Inside the existing `if maintainer { … }` block, after `follow::run_loop`:

```rust
if cfg.github_mirror.enabled {
    let st = state.clone();
    bg_handles.push(tokio::spawn(async move {
        let target = Arc::new(floe_mirror::WalTarget::new(
            st.registry.clone(),
            st.store.clone(),
            floe_server::mirror::nudge(st.clone()), // Box<dyn Fn(&RepoId) + Send + Sync>: placement-gated ops::start("follow")
        ));
        let source = match floe_mirror::github::GithubSource::new(&st.cfg.github_mirror) {
            Ok(s) => Arc::new(s),
            Err(e) => { tracing::error!(error = %e, "github mirror disabled: client setup failed"); return; }
        };
        floe_mirror::run_loop(target, source, st.store.clone(), st.cfg.clone(), st.recorder.clone()).await;
    }));
}
```

- `crates/floe-server/src/mirror.rs` (new, small): `pub fn nudge(state) -> Box<dyn Fn(&RepoId) + Send + Sync>`.
  It spawns `ops::start(state, id, "follow", {})` iff `state.cfg.placement.maintains(..)`. This module is the
  only coupling between the server and `floe-mirror`.
- Catalog: `AppState::new` builds the writer when `cfg!(feature = "catalog") && cfg.catalog.enabled` (writer
  + flusher task) and sets `recorder`. `serve.rs` spawns `CatalogTail` (WAL tail + inventory snapshot, §C.4,
  §C.6) on the events host and hands `Bridge` only the tail's wake sender, which `object_finalized` uses with
  `try_send`. `Bridge::new`'s condition and its `catch_up` join are unchanged.
- Drain (D31): the mirror loop checks `draining()` before each action; the flusher gets a final best-effort
  flush in phase 2, bounded by `server.drain_timeout`, and losing it costs only telemetry (durable rows are
  re-read from the cursor).

### D.3 `floe.example.toml` additions (verbatim shape; every key with its default)

```toml
[upstream]
# follow = ["refs/heads/*", "refs/tags/*"]  # ref patterns (git refspec globs: one `*`, crosses `/`; `^` excludes);
#                                           # refs/archive/* and refs/follow/* are never followed
# on_rewrite = "archive"                    # upstream force-push/tag move/delete: keep the old tip at
#                                           # refs/archive/<unix-ts>/<ref> then apply | "refuse" (leave as is, log)
# head = "refs/heads/main"                  # retarget HEAD to this when it exists (the GitHub mirror sets it)
# follow_interval = "10m"                   # per-repo minimum pause between follow rounds (unset = every tick)
# source = "github:123"                     # provenance label written by the mirror; informational
# [upstream.token_env_by_host]              # host-only: env var per upstream host (beats token_env)
# "github.com" = "FLOE_GITHUB_TOKEN"

[github_mirror]
enabled = false
api_url = "https://api.github.com"
git_url = "https://github.com"
token_env = "FLOE_GITHUB_TOKEN"   # PAT: classic `repo`, or fine-grained Contents:read + Metadata:read
prefix = "gh"                     # floe repo = <prefix>-<owner>/<name>, lowercased
interval = "5m"
users = []                        # "@me" = the token's user (private included)
orgs = []
starred = []
repos = []                        # explicit "owner/name"
include = ["*/*"]                 # globs over owner/name; `*` stops at `/`
exclude = []
skip_archived = true
skip_forks = true
include_private = true
private_visible_to_all_readers = false   # must be true when include_private (no per-repo read ACL; any auth mode)
wikis = false
lfs = true
follow = ["refs/heads/*", "refs/tags/*"]
on_rewrite = "archive"
follow_interval = "10m"
read_only = true
max_repo_size = "2GiB"
max_new_per_pass = 20
min_rate_remaining = 200
gone_after = "24h"
lease_ttl = "2m"

[catalog]                          # needs a binary built with --features catalog
enabled = false
# uri = "http://localhost:9000/iceberg"   # Iceberg REST endpoint (RustFS S3 Tables)
# warehouse = "floe-catalog"
namespace = "floe"
# token_env = "FLOE_CATALOG_TOKEN"
# credential_env = "FLOE_CATALOG_CREDENTIAL"   # "client_id:client_secret" (OAuth2 client credentials)
# s3_endpoint = "http://localhost:9000"
s3_region = "us-east-1"
s3_access_key_env = "AWS_ACCESS_KEY_ID"
s3_secret_key_env = "AWS_SECRET_ACCESS_KEY"
s3_path_style = true
flush_interval = "30s"
flush_rows = 5000
max_buffer_rows = 100000
commit_timeout = "60s"
backfill = false
create_tables = true
```

`floe.standalone.toml`: add a commented `[github_mirror]` block with `users = ["@me"]`, and a commented
`[catalog]` block pointing at the compose service.

### D.4 `compose.yaml`

- `rustfs`: pin the image to a release that ships S3 Tables, instead of `latest` (the implementer records the
  tag after verifying; R6), and add whatever environment flag that release needs to enable the Iceberg REST
  endpoint. Add the endpoint to the header comment.
- New one-shot `create-table-bucket` under `profiles: ["catalog"]`: creates the table bucket with
  `aws s3tables create-table-bucket --endpoint-url http://rustfs:9000 --name floe-catalog`, if RustFS supports
  that API, or with RustFS's documented equivalent.
- **Fallback** `iceberg-rest` service under the same profile: `apache/iceberg-rest-fixture` (Iceberg's
  reference REST catalog) with its warehouse in `s3://floe-test/warehouse` on RustFS
  (`CATALOG_S3_ENDPOINT=http://rustfs:9000`, path style). The ignored integration test runs against
  whichever endpoint `FLOE_TEST_CATALOG_URI` names, so the floe side can be tested while RustFS S3 Tables is
  in preview.
- `justfile`: `test-catalog` (`cargo test -p floe-catalog --features iceberg -- --ignored`); `just ci` stays
  unchanged (no catalog build in the fast tier); a `just clippy-catalog` lints the feature build.

### D.5 README

A new section, **"Mirroring GitHub"**, after "Running it": five lines on what it does (discover → create →
follow → archive on rewrite), the minimal config (`[github_mirror] enabled, users = ["@me"]` +
`FLOE_GITHUB_TOKEN` + a `maintain` host, and `private_visible_to_all_readers = true` or `include_private =
false`), the naming rule (`gh-acme/widgets`), `floe github sync --once
--dry-run`, the archive ref convention, the no-per-repo-ACL warning (R2), and one line on `--features catalog` +
`[catalog]`. Update the code map (`floe-mirror`, `floe-catalog`) and the `floe-cli` subcommand list.
`docs/LFS.md`: correct the `token_env`-in-settings example (`token_env` is host-only in the code), and mention
`token_env_by_host`.

### D.6 AGENTS.md decisions (append-only; next free number is D48)

- **D48 (2026-10-04) Follow patterns and archive-on-rewrite.** `[upstream] follow` takes git refspec patterns
  (one `*` crossing `/`, `^` negatives; `refs/archive/` and `refs/follow/` are never followed). With
  `on_rewrite = "archive"` (default), a non-fast-forward, a tag move, or a deletion upstream publishes **one**
  PUSH entry that creates `refs/archive/<unix-ts>/<original-ref>` at the old tip (`old_oid = ""`, never
  overwritten) and applies upstream's state, with `meta["follow.archived"]`. The round is one atomic
  transaction: any ref that moved under it rejects the whole round, which the next round re-plans. An upstream
  that advertises no refs at all never causes deletions. A round probes refs first and does Serve-level work
  only when something moved. `"refuse"` is D33's behaviour. Follow still bypasses policy. Supersedes D33's
  "fast-forward only" clause; the rest of D33 stands.
- **D49 (2026-10-04) The GitHub mirror decides, follow moves bytes.** `floe-mirror` runs on a `maintain` host
  under `leases/mirror-github.pb`, keeps `mirror/github/state.json` (CAS) and a disposable HTTP cache in the
  bucket, maps `owner/name` to `<prefix>-<owner>/<name>` (identity stays two segments, D5/D26; **this
  mapping is recorded only after the owner signs off**, R1), creates repositories by the manifest CAS, and owns
  only the `[upstream]` table of repositories marked `upstream.source = "github:<id>"`. A human edit of that
  table (an `[upstream]` change whose settings author is not `github-mirror`) detaches the repository. It never deletes a
  floe repository, never stores a token (`token_env` + `upstream.token_env_by_host`), and never transfers git
  objects. GitLab/Gitea implement `Source`.
- **D50 (2026-10-04) Iceberg audit tables are a WAL reader, behind a feature.** `floe-catalog` (`--features
  catalog`) writes `ref_events`/`force_push_log` from the WAL in its own loop on the events host, outside the
  webhook's catch-up, with its own cursor `catalog/cursor.json` (at-least-once, dedup `(repo, seq,
  ref_name)`), `sync_runs`/`repo_inventory` changes from lossy telemetry (`Recorder`), and a durable daily
  `repo_inventory` snapshot. The catalog is never a source of truth, never holds git objects, and its
  outage only adds catalog lag. Git, sync, follow and the mirror never await it.
- **D51 (reserved) Push to an upstream is a WAL reader on the maintaining host** (§E). It is reserved now so
  the rule "a ref is either followed or pushed, never both; entries with `principal = upstream` are never
  pushed" is on record before anyone builds it.

Also update: AGENTS.md §0 document map (this file), §2.1 table (`mirror/github/state.json`,
`mirror/github/http-cache.json`, `catalog/cursor.json`, `catalog/epoch.json`, `catalog/inventory.json`,
`refs/archive/`), §2.2 (follow line), **§3 principle II's `PutMode::Overwrite` list** (add the mirror's HTTP
cache), `docs/CONTRACT.md` (`floe-mirror`, `floe-catalog` blocks; `RefPatterns` in floe-config),
`docs/EVENTS.md` (§A.5 paragraph, catalog tail), `docs/ROUNDTRIPS.md` (§D.7).

**`GOAL.md` §4** ("all the features a git host needs, and only those") gains, in the same change: "upstream
mirroring (follow an upstream's refs, discover and mirror a forge's repositories, nothing rewritten upstream is
ever lost) and derived audit tables of ref history". Principle X measures scope against that line, and without
it neither the mirror nor the catalog has one.

### D.7 Round trips (`docs/ROUNDTRIPS.md` rows to add)

| Operation | Bucket critical path |
|---|---|
| Follow round, nothing moved | refs-level only: conditional manifest GET (+ checkpoint/log tail GETs on a cold handle); no pack GETs, even for a repository the LRU evicted (today: a Serve-level `sync()` every round). Upstream: one `ls-refs` (probe) |
| Follow round, something moved | Serve-level `sync()` (pack GETs only for packs not local) + log PUT + manifest CAS; archive refs ride in the same txn. Upstream: probe `ls-refs` + fetch |
| Mirror pass, nothing changed | lease GET/PUT (heartbeat) + state GET + http-cache GET/PUT; GitHub: one request per listing page (304s) |
| Mirror create | manifest PUT(Create) + policy GET + PUT(Create) + manifest GET + log PUT + manifest CAS (settings) + state CAS (amortised per ≤ 10 actions) |
| Catalog catch-up per repo (own loop) | cursor GET + manifest conditional GET + log GETs + cursor CAS; the Iceberg commit is off the bucket's git path (catalog + table files). Cold cursor: + `catalog/epoch.json` GET + one log GET. While the catalog is down: cursor GET only. Adds nothing to the webhook's notify path |
| Inventory snapshot (daily) | lease GET/PUT + `catalog/inventory.json` GET/CAS + one `registry.list()` + mirror state GET; off every hot path |

---

### D.8 As landed (2026-10-04; the code wins)

- **Telemetry wiring**: `AppState` carries `recorder` (the writer, or `NoopRecorder`), `catalog` (the writer, for the
  final flush) and `catalog_tail`. The mirror's `on_pass` hook (§B.15) maps a `PassReport` to one `sync_runs` row
  (`kind = discovery`) and one `repo_inventory` change row per `changes` entry (`floe_server::mirror::record_pass`);
  a failed pass is reported to the hook too (`outcome = failed`, `PassReport::error` as detail).
  Follow records a `sync_runs` row for every round that did work, and for a refused or failed round whose
  outcome or detail differs from the repository's last round on this host; `in-sync` rounds and repeats of a
  standing refusal or failure are not recorded (one row per repository per tick would be noise).
- **Notify**: `Bridge` is unchanged. `http_notify` wakes the catalog tail itself (the same key parsing,
  `try_send`), and answers `200 []` on an events host that has a catalog tail but no bridge sink.
- **Inventory snapshot**: hourly due-check, `INVENTORY_LEASE` 5 min TTL, the schedule re-read under the lease.
  A failure to read the mirror's state fails the attempt (retried within the hour), never an all-`own` snapshot;
  the schedule is written only while the lease is held and never moves backwards.
  The mirror's state is read as `mirror/github/state.json` (the only `Source` today), not by listing `mirror/*`.
- **Final flush**: `floe serve` calls `CatalogWriter::shutdown` after `serve` returns, bounded by
  `server.drain_timeout`.
- **`floe config check`** prints the effective `[github_mirror]`/`[catalog]` when enabled and whether each env var
  they name is set (never the value); `catalog.enabled` without the feature fails there as at startup.
- **compose (§D.4)**: `rustfs` stays on `latest` (no tagged release verified to ship S3 Tables, R6).
  `create-table-bucket` creates the bucket and PUTs `/iceberg/v1/buckets/floe-catalog` with SigV4 (curl
  `--aws-sigv4`), falling back to a message; the `iceberg-rest` fixture is what the writer can reach today, since
  `iceberg-catalog-rest` 0.10 does not sign SigV4.

## E. Push-to-GitHub seam (post-MVP, design only; D51)

- **Where**: a **WAL reader on the maintaining host**, symmetric to follow: `crates/floe-server/src/push_back.rs`,
  its own loop next to `follow::run_loop`, with a per-repo durable cursor `repos/<o>/<r>/push/<remote>.json`
  (the bridge's `Cursor` shape). It must run where the objects are (Serve-level sync, `packs_fit()`), not on
  the events host, which is refs-level. Each catch-up reads `(cursor, head]`, computes the latest WAL value of
  every pushable ref touched, runs one `git push --atomic <upstream> <oid>:<ref>…` from the serving copy, and
  then advances the cursor. A failure leaves the cursor, so it retries, and it is narrated as a `push-back` task.
- **Config seam** (reserved names; `validate` refuses them until implemented): `[upstream] push = ["refs/heads/*"]`,
  `push_force = false`.
- **Loop prevention (normative)**:
  1. **Disjointness**: no ref may match both `upstream.follow` and `upstream.push` (`validate`, at publish).
     A ref has one direction. Bidirectional sync of the same ref is out of scope by construction.
  2. **Provenance**: log entries with `meta.principal == "upstream"` (follow's writes) are never pushed,
     even if a pattern overlap slips through.
  3. **Idempotence**: before pushing, `ls-remote` the target refs. A ref already at the WAL value is skipped,
     so a replay after a crash is a no-op.
  4. **No force by default**: a rejected non-fast-forward is reported (task + metric + `sync_runs` row,
     `kind = "push"`) and never retried with force unless `push_force = true`. `refs/archive/*` is never pushed.
- **Mirror interplay**: the mirror would set `push` only for repositories an operator marks writable (a future
  `[github_mirror] writable = [globs]`), which turns the read-only policy into "only via floe". Out of MVP.

---

## Risks / open questions

| # | Risk / question | Mitigation / owner decision needed |
|---|---|---|
| R1 | **Naming deviates** from the requested `<prefix>/<owner>/<repo>`: floe ids are two segments (D5, D26), so we propose `<prefix>-<owner>/<repo>`. **Open: needs the owner's sign-off before §B starts** (status line, §B.5, D49). | Accept (recommended), or open a separate decision to allow nested owners, which touches `RepoId`, routing, the edge contract and the UI. Only `naming.rs` changes if the answer differs. |
| R2 | **No per-repository read ACL**: a mirrored private GitHub repository is readable by every principal with read on floe. | `validate` refuses `include_private = true` unless `private_visible_to_all_readers = true` (§B.4), so the exposure is an explicit operator decision; README says it loudly; per-repo read ACL is a separate feature. |
| R3 | **Follow does not scale linearly**: one sequential loop (an `ls-refs` probe per repo per round; Serve-level sync and `packs_fit()` only for repos that moved), the whole object set local while a round publishes. Hundreds of repos are fine; thousands, or one huge repo, are not. | `max_repo_size` + `max_new_per_pass`; refs-first rounds (§A.4); nudges instead of tight polling; huge repos go through the too-large handoff (`floe import` on an SSD host, then the source marker, §B.7.1). A post-MVP item is a concurrency limit for follow ops. |
| R4 | Archive refs grow the ref count forever (a repository force-pushed hourly gets about 8.7k archive refs a year), and `ls-refs` advertises them. | Acceptable at these sizes (refs are O(1) on hot paths). Post-MVP: optional `upstream.archive_retention` (still never auto-deletes by default), and hiding `refs/archive/` from v0 advertisement. |
| R5 | **LFS is read-through, not prefetched**: an LFS object never downloaded before the GitHub repository disappears is lost. | Post-MVP `lfs_prefetch` unit. Call it out in README. |
| R6 | **RustFS S3 Tables is preview**: exact REST path, warehouse identifier, and auth (SigV4 vs bearer/OAuth). `iceberg-catalog-rest` 0.10 is not known to sign SigV4. | Verify against the pinned RustFS release before §C lands. If SigV4 is mandatory, add a signing `reqwest` middleware or put the Iceberg REST fixture in front. The ignored test runs against either. |
| R7 | iceberg-rust 0.10 MSRV 1.94 > workspace 1.90; arrow/parquet 58 add compile time and binary size. | Feature-gated (iceberg's manifest enforces 1.94 only with the feature; no `rust-version` on `floe-catalog`, which the default build depends on); decide whether the release image enables `catalog` (recommend: yes for the image, no for `cargo build`). |
| R8 | Tokens: PAT only. GitHub App installation tokens expire hourly, and follow reads an env var. | Post-MVP `TokenProvider` (App JWT → installation token) behind `Source`/`upstream_token_env`; the seam is `Config::upstream_token_env`. |
| R9 | GitHub `has_wiki` is true for empty wikis → a failing `ls-refs` per `follow_interval`. | `wikis = false` default. Post-MVP: the mirror marks the wiki `absent` after N failed follow rounds (needs follow status in the bucket or the catalog). |
| R10 | Catalog is at-least-once; duplicates are possible. | Dedup key + views. Post-MVP: per-repo watermarks in table properties, committed in the same transaction as the append. |
| R11 | `force_push_log` covers upstream rewrites only, not human force pushes to own repos. | Post-MVP: `receive.rs` records `meta["forced"]` (it already classifies `force-push` for policy); the catalog then picks it up with no other change. |
| R12 | Deleted vs. access revoked is indistinguishable for private repositories (both 404). | Both freeze (`gone`/`forbidden`); nothing is deleted, so a wrong guess costs nothing. |
| R13 | Renamed repositories keep their old floe name. | Intentional (stable URLs). A floe-side rename/alias is a separate decision. |
| R14 | Mass deletion upstream (e.g. a force-mirror push that drops branches) archives and deletes at scale. | Nothing is lost (archives). Only an upstream advertising *no refs at all* is guarded. A `max_delete_fraction` guard is an easy follow-up if wanted. |
| R15 | `on_rewrite = "archive"` as the default changes behaviour for existing `follow` users. | Intended (requirement). Noted in D48 and in the release notes. |
| R16 | Two hosts maintaining one repository (placement misconfig) would both follow it. | Already true for D33. Publish is CAS-safe and a round is atomic: when both plan the same rewrite, the second's round is rejected whole (its refs or its archive name moved) and re-plans to "in sync". No duplicate archive, nothing partial. |
| R17 | A followed branch upstream that points at a non-commit object makes `is_ancestor` fail, so every round of that repository fails. | Rare and visible (`failed` outcome naming the ref). Narrow the patterns with a `^` negative for that ref. |
