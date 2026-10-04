# GitHub mirror MVP: follow globs, archive-on-rewrite, `floe-mirror`, `floe-catalog`

Context: **the design of record for the GitHub mirror MVP**, written before any code so that four engineers can
build the four work packages (§A–§D) in parallel without talking to each other. Read `GOAL.md`, `AGENTS.md`
(§2.2 the write path, §3 principles, D24/D28/D30/D32/D33/D46/D47) and `docs/EVENTS.md` first. Every interface that
crosses a work package is pinned here (§0.3); everything else is the implementer's call within the house rules
(`[workspace.lints]`, no `unwrap`/`expect`/`panic` in production code, clippy pedantic, tests next to the code).
Where this document and the code disagree after landing, the code wins and this file gets a dated note.

Status: **proposed** (2026-10-04). Decision numbers D48–D51 are reserved for it (§D.6).

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
  `force_push_log`). It is fed by the events bridge's WAL reader (durable per-repo cursor) and by lossy
  telemetry from the mirror and follow loops. It sits behind the `catalog` cargo feature, and a catalog outage
  only adds catalog lag.
- **§D Wiring**: CLI, server loops, config, compose, README, AGENTS.md decisions.

### 0.2 Why this shape (principles check)

- **I (no state outside the bucket)**: mirror state is `mirror/github/state.json` (CAS); the HTTP ETag cache is
  a disposable bucket object; Iceberg tables are *derived* audit copies, never a source of truth (wipe the
  catalog and only history in the warehouse is lost; the WAL still has every ref event).
- **II (manifest CAS is the only commit point)**: follow still publishes one PUSH entry per round; the archive
  ref and the rewrite are in **one** `RefTransaction`. The mirror creates repos by the existing manifest CAS
  create and publishes settings via the existing SETTINGS entry. No new commit points.
- **III (side effects are WAL readers)**: the catalog consumes the WAL through the bridge's cursor machinery
  (D32/D46). The push-back seam (§E) is a WAL reader. Neither `follow.rs` nor the mirror makes an HTTP call
  *as a step of a write*; the only thing they hand the catalog is a lossy `try_send` of telemetry *after* the
  write finished (§C.6), the same as a metric.
- **X (keep floe small)**: no new git transport (follow reuses `git fetch`), no new auth path, no database. The
  heavy dependency (iceberg + arrow + parquet) is behind a feature that is off by default.

### 0.3 Cross-package contracts (frozen by this document)

| Contract | Producer | Consumer | Section |
|---|---|---|---|
| `[upstream]` keys `follow` (patterns), `on_rewrite`, `head`, `follow_interval`, `source`; host-only `token_env_by_host` | §A (floe-config) | §B writes them into settings | §A.1 |
| Archive ref name `refs/archive/<unix-ts>/<original-ref>` | §A | §C `force_push_log`, humans | §A.3 |
| Log entry meta `follow.archived` (format below) | §A | §C | §A.5 |
| `floe_catalog::{Recorder, SyncRun, InventoryRecord}` (always compiled, no iceberg) | §C | §A (follow), §B (mirror) | §C.6 |
| `floe_mirror::{Source, RemoteRepo, Target}` | §B | §D (server/CLI wiring) | §B.3, §B.7 |
| `RepoHandle::publish_settings_if(toml, author, message, expected_revision)` | §B (small floe-wal addition) | §B | §B.7 |
| `floe_store::coord::cas_update_json` | §B | §B, later the bridge | §B.6 |

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
    /// Ref patterns kept equal to upstream's (§A.2). Empty = follow off.
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
  `token_env`), and `public_toml`/effective view strips it. Validate `follow` with
  `floe_git::follow::RefPatterns::parse` (§A.2) and `head` (must start with `refs/heads/`), so an invalid
  pattern is a 400 at `PUT …/api/settings` and nothing is published.
- `Config::validate`: the same pattern validation for host-level `[upstream]`. `token_env_by_host` keys are
  bare hostnames (no scheme, no path), and values are non-empty.
- **Derived default** (in `Config::load`, not `validate`): when `github_mirror.enabled` (§B.4) and
  `token_env_by_host` has no entry for the mirror's git host, insert `host(github_mirror.git_url) →
  github_mirror.token_env`. One token configuration then covers discovery, follow and LFS read-through.
- `floe.example.toml` documents every new key (§D.3).

Token resolution moves to one function, `floe_config::Config::upstream_token_env(&self, url: &str) ->
Option<&str>`. `follow::token_for` and `lfs_upstream` (read-through) both call it. Today they each read
`upstream.token_env`.

### A.2 Pattern syntax and matching (`crates/floe-git/src/follow.rs`, new `RefPatterns`)

The patterns are passed to `git fetch` verbatim as refspec sources, so they use **git refspec glob
semantics**, not POLICY.md's doublestar dialect. A pattern has one consumer, and that consumer is git.

| Form | Meaning | Example |
|---|---|---|
| exact | one ref | `refs/heads/main` |
| glob | exactly one `*`, matching **any** characters **including `/`** (git refspec rule) | `refs/heads/*` matches `refs/heads/feat/x` |
| negative | leading `^`, exact or glob; excludes (git ≥ 2.29 negative refspec) | `^refs/heads/dependabot/*` |

Rules (`RefPatterns::parse(&[String]) -> Result<RefPatterns, GitError>`):

1. Every entry starts with `refs/` (after an optional `^`) and has at most one `*`. Each entry passes git's
   `check-ref-format --refspec-pattern` rules: no `..`, `@{`, `\`, control characters, space, `~^:?[`,
   trailing `/` or `.lock`. Implement the checks in Rust (cheap, no subprocess) and test them against git in
   one test.
2. At least one positive entry.
3. **Reserved namespaces are never followed**: `refs/archive/` and `refs/follow/`. An entry that starts with
   either is a parse error. `RefPatterns::matches` also returns false for any name under them, even when a
   positive pattern like `refs/*` would match. The fetch always appends `^refs/archive/*` and `^refs/follow/*`,
   so an upstream that is itself a floe never feeds its archive into ours.
4. `matches(name)` = (any positive matches) ∧ ¬(any negative matches) ∧ ¬reserved. Deterministic, no regex.
5. `refspecs()` → positive: `+<src>:refs/follow/<src minus "refs/">` (the `*` carries through), negative:
   `^<src>`. `is_exact()` reports entries without `*` (needed for the missing-ref fallback below).

`fetch_refs` changes signature: `refs: &[String]` becomes `patterns: &RefPatterns`, and `have` becomes "every
WAL ref matching `patterns`". Behaviour changes:

- **Scratch reset**: before the fetch, the scratch's `refs/follow/*` is made **exactly** `have` (delete every
  `refs/follow/*` not in `have`; `for-each-ref` + one `update-ref --stdin`). Today only the listed refs are
  reset. With globs the set changes from round to round.
- **`--prune`** is added to the fetch so a ref upstream no longer advertises disappears from `refs/follow/*`.
  Then `read_scratch().tips` is exactly upstream's matching set, and a deletion is `have.keys() − tips.keys()`.
- **Exact ref missing upstream**: git fails the whole fetch with `couldn't find remote ref <r>`. When stderr
  says that, run one `git ls-remote <upstream> <exact refs…>`, drop the missing exact refs from the refspecs,
  fetch again, and report them as absent (so they are deletions). This is the rare path; the common path stays
  one `ls-refs` round trip when nothing moved.
- The token still travels through the one-shot credential helper (never argv).

### A.3 Archive ref naming

`refs/archive/<unix-ts>/<original-ref>`, where `<original-ref>` is the **full** ref name and `<unix-ts>` is
the follow op's start time in UTC seconds, taken once per op and shared by every archive in that op. Examples:

```
refs/archive/1791072000/refs/heads/main          # main was force-pushed upstream
refs/archive/1791072000/refs/tags/v1.2.0          # tag v1.2.0 was moved upstream
refs/archive/1791075600/refs/heads/feat/old-api  # branch deleted upstream
```

- The archive ref is created with `old_oid = ""` (must not exist), so it can never overwrite an earlier
  archive. A collision (two rewrites of one ref inside one second, which is only possible with a manual op)
  fails that ref's update. The next round re-plans with a new timestamp.
- Archive refs are ordinary refs: they keep objects reachable for `fsck`/`repair`/compaction, they are
  advertised by `ls-refs`, and `git fetch origin 'refs/archive/*:refs/archive/*'` retrieves them. Follow never
  deletes or moves them.
- The mirror's read-only policy (§B.7) protects them from pushes. For own repositories, POLICY.md gains an
  example rule `archive-immutable` (`refs/archive/**`, restricts `update`, `delete`).

### A.4 Where the change goes in `follow.rs`

Split `crates/floe-server/src/follow.rs` into `follow/mod.rs` (loop, op, statuses: today's code) and
`follow/plan.rs` (pure, synchronous, unit-tested):

```rust
// follow/plan.rs
pub(crate) struct Observed<'a> {
    pub have: &'a HashMap<String, String>,   // WAL refs matching the patterns
    pub tips: &'a HashMap<String, String>,   // upstream refs matching the patterns (post --prune)
}
pub(crate) enum Change { Create { name, new }, Update { name, old, new }, Delete { name, old } }
/// Diff; never emits a no-op. Empty `tips` with non-empty `have` yields no Delete (the empty-advertisement
/// guard: an upstream that suddenly advertises nothing is a misconfiguration or an outage, not a mass delete).
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

Classification, done in the op before `build`, under the existing read guard:

| Change | Kind |
|---|---|
| Create | FastForward (nothing to lose) |
| Update under `refs/tags/` | **Rewrite**, always (POLICY.md: "a tag retarget is `force-push`") |
| Update elsewhere | `is_ancestor(old, new)`: true ⇒ FastForward; false **or error** (non-commit objects) ⇒ Rewrite |
| Delete | Rewrite |

`build` with `Archive`: FastForward ⇒ one `RefUpdate{name, old, new}`. A rewritten update ⇒ two updates,
`RefUpdate{archive_ref, "", old}` + `RefUpdate{name, old, new}`. A delete ⇒ `RefUpdate{archive_ref, "", old}` +
`RefUpdate{name, old, ""}` (an empty new oid is a delete in `apply_txn_to_map`/`apply_ref_txn`). With
`Refuse`, a rewrite or delete becomes a `refused` line (today's text for rewinds, plus `"<ref>: deleted
upstream; left as is"`).

`HeadMove`: when `upstream.head` is set, differs from the WAL's HEAD target, and the target is in the
post-plan ref set, add `RefUpdate{name: "HEAD", new_symbolic_target: head}`. `publish.rs` already handles
symbolic updates.

Changes in `follow/mod.rs`:

- `run_pass`: replace `cfg.upstream.follow.is_empty()` with `RefPatterns::parse` (a parse error is a `warn!`
  and skip; it cannot normally happen, because settings are validated at publish). Skip the repository when
  `upstream.follow_interval` is set and the last round's `Instant` (new field on `FollowStatus`, not
  serialized) is younger. `moved` becomes `!diff(..).0.is_empty() || head differs`.
- `current(handle, refs)` → `current_matching(handle, &patterns)`: every snapshot ref with
  `patterns.matches(name)`.
- `op`: `let ts = unix_now_secs();`, diff → classify → `build` → connectivity over the non-delete new oids
  (archive refs point at objects already held) → `fill_peeled` → one `publish_push`. `per_ref` results are
  reported as today. An archive ref that lost (moved under us) makes its paired apply lose too, because the
  transaction is per-ref in the WAL's eyes. **The op therefore checks `per_ref`. If an archive ref failed
  while its paired apply succeeded, it logs `error!` and records `refused`.** That cannot happen with
  `old_oid=""` on a fresh name unless someone pushed that exact archive name; the test below pins it.
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
- `docs/ROUNDTRIPS.md`: no change on the bucket side (still one log PUT + one manifest CAS). Upstream side: +1
  `ls-remote` only on the missing-exact-ref path. Add the row anyway.

### A.6 Tests (§A)

`floe-git` unit tests (`follow.rs` `#[cfg(test)]`, real git, `file://` upstream in a tempdir):

- `ref_patterns_parse_and_match` (table test: exact/glob/negative, `refs/*` never matches `refs/archive/x`,
  invalid forms rejected, agreement with `git check-ref-format --refspec-pattern` on a corpus).
- `fetch_with_globs_reports_new_and_pruned_refs` (branch created and branch deleted upstream between two
  rounds → `tips` reflects both).
- `fetch_with_missing_exact_ref_falls_back_to_ls_remote`.
- `negative_refspec_excludes`.

`floe-server` unit tests (`follow/plan.rs`): `diff` cases, the empty-advertisement guard, `build` with both
policies, tag-move-is-rewrite, archive-and-delete pairing, HEAD move only when its target exists, ordering
determinism of `follow.archived`.

`crates/floe-server/tests/follow.rs` (extends the existing two-instance test, which uses a second floe as the
upstream):

1. `follows_globs_creating_branches_and_tags`.
2. `upstream_force_push_is_archived_then_applied`: one PUSH entry, `refs/archive/<ts>/refs/heads/main = old`,
   `main = new`, `meta["follow.archived"]` correct, `old` still reachable (`git cat-file -e`).
3. `upstream_delete_is_archived_then_deleted`.
4. `on_rewrite_refuse_keeps_d33_behaviour` (the existing assertion moves here).
5. `empty_advertisement_never_deletes`.
6. `moved_tag_is_archived`.
7. `own_repo_without_follow_is_untouched` (requirement 3).

`tests/events.rs`: one golden test that a rewrite yields `create(refs/archive/…)` + `update` with the same seq.

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
  src/github/mod.rs   # GithubSource: Source
  src/github/http.rs  # conditional GET, pagination (Link), rate limit, Retry-After; HttpCache
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
pub struct Selection { pub users: Vec<String>, pub orgs: Vec<String>, pub starred: Vec<String>, pub repos: Vec<String> }
pub struct Discovery { pub repos: Vec<RemoteRepo>, pub complete: bool, pub stats: ApiStats }
pub enum Lookup { Found(RemoteRepo), Gone /* 404 */, Forbidden /* 401/403 non-rate-limit */ }
pub struct ApiStats { pub requests: u32, pub not_modified: u32, pub rate_remaining: Option<u32>, pub rate_reset: Option<i64> }
#[derive(Debug, thiserror::Error)]
pub enum SourceError { Unauthorized, RateLimited { until: SystemTime }, Http(String), Decode(String) }
```

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
| `repos` | `[]` | Explicit `"owner/name"`. Always included, even if archived/fork. Still subject to `exclude`. |
| `include` | `["*"]` | Globs over `owner/name` (case-insensitive). `*` stops at `/`; `owner/*` and `*/name` both work. |
| `exclude` | `[]` | Globs; exclude wins. |
| `skip_archived` | `true` | Do not *start* mirroring archived repositories (already mirrored ones are kept, §B.9). |
| `skip_forks` | `true` | Same for forks. |
| `include_private` | `true` | `false` = public only. |
| `wikis` | `false` | Also mirror `<repo>.wiki.git` as `<prefix>-<owner>/<name>.wiki` (§B.10). |
| `lfs` | `true` | Write `upstream.lfs` so LFS objects read through (`docs/LFS.md`). |
| `follow` | `["refs/heads/*", "refs/tags/*"]` | Patterns written into each repository's `upstream.follow`. |
| `on_rewrite` | `"archive"` | Written into `upstream.on_rewrite`. |
| `follow_interval` | `"10m"` | Written into `upstream.follow_interval` (backstop; pushes are nudged, §B.8). |
| `read_only` | `true` | Publish a deny-all `policy.json` at creation, so only follow (which bypasses policy, D33) moves refs. |
| `max_repo_size` | `"2GiB"` | Repositories larger than this by GitHub `size` are `too-large`: listed in state and logged with the `floe import` recipe, never auto-created. `0` = no limit. |
| `max_new_per_pass` | `20` | Bound on creations per pass (a 2,000-repository org arrives over several passes, and follow is not flooded). |
| `min_rate_remaining` | `200` | Stop a pass (incomplete) when `x-ratelimit-remaining` drops below this. |
| `gone_after` | `"24h"` | A repository must be missing/404 for this long (several passes) before it is marked `gone`. |
| `lease_ttl` | `"2m"` | TTL of `leases/mirror-github.pb`; heartbeat every `lease_ttl / 3`. |

`Config::validate`: `enabled` ⇒ `has_role(Maintain)`; `prefix` charset; globs well-formed; `api_url`/`git_url`
are http(s); `follow` parses (§A.2). `maintenance.follow_interval > 0` gets a **warning** when the mirror is
enabled, because it cannot tell whether another host follows the repositories.

### B.5 Naming: GitHub → floe `RepoId`

The requirement says `<prefix>/<owner>/<repo>`. floe identity is exactly two segments, `<owner>/<repo>` (D5,
`RepoId`), and routing is by those two segments (D26). A third segment would change both, so the mirror maps
**`gh/acme/widgets` → `gh-acme/widgets`**:

- `owner = format!("{prefix}-{gh_owner}")`, lowercased (GitHub names are case-insensitive, so lowercase is
  lossless for identity). GitHub owners are `[A-Za-z0-9-]{1,39}`, so a fixed prefix plus `-` is unambiguous.
- `name = gh_name` lowercased. GitHub allows a leading `.` (`.github`), and floe does not. A leading `.` maps to
  `_.` (`.github` → `_.github`).
- Wiki: `name + ".wiki"`.
- **Collision** (the mapped id already exists and is not ours, i.e. its `upstream.source` ≠ `github:<id>`, or
  two GitHub repositories map to one name): use `<name>--<id>`. If that also exists and is not ours, the state
  entry is `conflict` and the mapping is skipped with a `warn!`. The mapping is computed **once, at creation**,
  and stored in state. A rename never changes it (§B.9).

`naming.rs` is pure, and it is table-tested, including `RepoId::new` acceptance of every output.

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
      "first_seen": "…", "last_seen": "…", "missing_since": null,
      "settings_sha": "<sha256 of the [upstream] table the mirror last published>",
      "settings_revision": 3,
      "last_error": null
    }
  }
}
```

`status` ∈ `active` | `too-large` | `excluded` (no longer selected; frozen) | `gone` (404 for `gone_after`;
frozen) | `forbidden` | `conflict` | `detached` (a human edited `[upstream]`, §B.7) | `error`.

**`mirror/github/http-cache.json`**: `{ "<url>": { "etag": "…", "body": <projected JSON> } }`, only the
fields of `model.rs`. This is a **cache**: losing it costs rate limit, nothing else (principle I). It is
overwritten without CAS at the end of a pass.

CAS: add `floe_store::coord::cas_update_json<T: Serialize + DeserializeOwned>(store, key, max_retries, f)`
next to `cas_update`, with the same loop shape (`get_bytes` → `f(Option<&T>)` → `put_bytes(Create |
Update(version))`, re-read on 412, jittered backoff on `Retryable`). The reconciler writes state **after every
action batch of ≤ 10 actions** and at the end of the pass, so a crash mid-pass loses at most 10 idempotent
actions. Every action is idempotent (§B.7). Only the lease holder writes, so the CAS is a safety net against a
lost lease, not a hot contention point. A 412 after retries aborts the pass (`warn!`), and the next holder
re-plans.

### B.7 Repository creation and settings (exact calls)

The floe side is a trait, so the planner and applier test without a server:

```rust
#[async_trait::async_trait]
pub trait Target: Send + Sync {
    async fn exists(&self, id: &RepoId) -> anyhow::Result<Option<ExistingRepo>>; // settings toml + revision + upstream.source
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
| create | `registry.create(&id, floe_git::ObjectFormat::Sha1)` (GitHub is SHA-1; `git.object_format` is ignored on purpose) → `WalError::AlreadyExists` ⇒ `AlreadyExists` |
| settings | parse the current `RepoSettings.toml` (empty when none) into `toml::Table`, replace only the `upstream` table with the mirror's (§B.7.1), serialize, then **`handle.publish_settings_if(text, "github-mirror", "floe github mirror: <reason>", expected_revision)`** |
| policy | `store.get_bytes(floe_proto::keys::policy_key(o, n))`; absent ⇒ `put_bytes(.., PutMode::Create)`. The same object `crate::policy::save` writes; `Create` makes it never overwrite a human's file |
| nudge | server: closure (§D.2); CLI: no-op |

**New floe-wal method** (small, in `handle.rs` + `publish::publish_settings_impl`): `publish_settings_if(toml,
author, message, expected_revision: u64) -> Result<u64, WalError>`. It is the same loop, but after the refs
sync it compares `manifest.settings.revision` (0 when none) with `expected_revision` and returns
`WalError::Conflict` on mismatch, without retrying. `publish_settings` stays as it is (HTTP API). This makes
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
- **Human edits win**: before publishing, the mirror compares the *current* `[upstream]` table's canonical
  hash to `settings_sha` in state. If they differ, a human changed it: the entry becomes `detached`, the mirror
  logs once and never touches that repository's settings again until an operator runs `floe github adopt
  <floe repo>` (post-MVP; for the MVP, deleting the state entry re-adopts it).
- Ownership test (`ExistingRepo` is ours) is `upstream.source == "github:<id>"`. A pre-existing repository
  without that marker is **never** modified (requirement 3). It becomes a `conflict`, and naming picks `--<id>`.

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

- **Lease**: `leases/mirror-github.pb` at the bucket root (store root, not a repo prefix), via
  `floe_store::coord::try_acquire(store, key, instance_id(), "github-mirror", lease_ttl)` and
  `LeaseGuard::spawn_heartbeat(guard, lease_ttl/3, lease_ttl)`. Exactly one reconciler fleet-wide. A host that
  does not get the lease sleeps `interval` and tries again. If the heartbeat reports a lost lease, the pass
  aborts before its next action (check a `released` flag between actions).
- **Loop**: tick every `interval` (first tick 10 s after start, jittered ±10%); `floe_wal::tasks::draining()`
  ⇒ release the lease and return (D31 phase 1). One pass:
  1. load state (+ http cache);
  2. `source.discover(sel)`, filter with `select.rs`;
  3. for ids in state but not discovered: `source.lookup(id)` (bounded to 50 per pass);
  4. `registry`-side facts via `target.exists` for new mappings only;
  5. `plan.rs` → `Vec<Action>` (`Create`, `PublishUpstream{reason}`, `PutPolicy`, `Nudge`, `MarkStatus`);
  6. `apply.rs`, in action order, CAS state every ≤ 10 actions;
  7. record `SyncRun{kind: "discovery"}` + `InventoryRecord`s for changed entries (§C.6).
- **Nudge on change**: for every `active` repository whose `pushed_at` changed since state (or that was just
  created), call `target.nudge_follow(id)`. In the server, the nudge closure runs
  `crate::ops::start(state, id, "follow", {})` **only when `cfg.placement.maintains(owner, name)` on this host**
  (D28/D30). Otherwise it does nothing, and the maintaining host's follow loop picks the change up within
  `upstream.follow_interval`. `ops::start` joins a running `follow` task for the same repository, so a nudge
  can never start a second one.
- **Server placement**: spawned in `floe-cli/src/serve.rs` next to `follow::run_loop`, inside `if maintainer`,
  when `cfg.github_mirror.enabled`. It is its own loop, never a unit of the priority loop (same reasoning as
  D33: discovery must not wait behind a base rebuild).

### B.9 Source-side changes

| Event at GitHub | Detected by | Mirror action |
|---|---|---|
| New repository matching the selection | discovery | `Create` → `PutPolicy` → `PublishUpstream("created")` → `Nudge`; `status=active` (or `too-large`) |
| Push | `pushed_at` changed | `Nudge` |
| Default branch changed | `default_branch` changed | `PublishUpstream("default branch")` (`head`) |
| Renamed / transferred | same `id`, new `full_name` | floe name **unchanged** (stable URLs, no floe rename exists); `PublishUpstream("renamed")` with the new `git`/`lfs` URLs; `full_name` updated; `info!` |
| Archived | `archived` became true | Keep following (nothing moves, and the cost is one `ls-refs` per `follow_interval`); `PublishUpstream` with `follow_interval = "24h"`; inventory row |
| Unarchived | `archived` became false | restore `follow_interval` |
| Visibility public ↔ private | `private` changed | state + inventory only (floe has no per-repo read ACL, see Risks R2). If access is lost, it is handled as below |
| No longer selected (exclude added, star removed) | absent from a **complete** discovery, `lookup` = Found | `status=excluded`; **frozen**: `PublishUpstream` with `follow = []` (data kept, follow stopped) |
| Deleted / access revoked | absent from a complete discovery, `lookup` = Gone/Forbidden for `gone_after` | `status=gone`/`forbidden`; frozen as above. Never deleted in floe |
| Discovery incomplete (rate limit, 5xx) | `complete=false` | no absence-based action this pass |
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
  reuses the cached projection. Every page is requested every pass (a 304 does not count against the primary
  rate limit for authenticated requests).
- **Rate limit**: read `x-ratelimit-remaining` / `x-ratelimit-reset` from every response into `ApiStats`. Below
  `min_rate_remaining`, stop (incomplete pass). A `403`/`429` with `retry-after`, or with `remaining = 0`, is
  `SourceError::RateLimited{until}`: the pass ends incomplete and the loop sleeps until `until` (capped at 1 h).
  Secondary limits: requests are strictly sequential (no concurrency against the API), with no pacing beyond that.
- **Errors**: 5xx/timeouts retry twice with jittered backoff (`floe_store::util::backoff` shape), then mark the
  pass incomplete. 401 = `Unauthorized` (pass fails). 404/403 on `lookup` = `Gone`/`Forbidden`.
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
  tests, covering every row of §B.9 as a `(state, discovery, lookup) → actions` case, plus "incomplete discovery
  never marks gone" and "human-edited upstream ⇒ detached, no publish".
- `FakeSource` (scripted `Discovery`/`Lookup` per pass) + `WalTarget` over `MemoryStore` (real `Registry`, no
  server): `reconcile_creates_repos_with_settings_and_policy`, `reconcile_is_idempotent` (second pass: zero
  actions, state unchanged), `crash_between_create_and_settings_is_repaired` (create done, state not written →
  next pass publishes settings), `rename_keeps_floe_name_and_updates_url`, `max_new_per_pass_bounds_creations`,
  `existing_unmanaged_repo_is_never_touched`.
- `github/http.rs`: an in-process axum stub on `127.0.0.1` (loopback, as `tests/follow.rs` does with a second
  floe): Link pagination, ETag 304 reuse, `retry-after` → `RateLimited`, `x-ratelimit-remaining` floor →
  incomplete.
- Lease: two `run_loop`s over one `MemoryStore` → exactly one reconciles (assert on the state's `holder`).
- End to end (`crates/floe-server/tests/mirror.rs`): `FakeSource` pointing `git_url` at a second floe instance
  (as `tests/follow.rs`); the mirror creates the repository, the nudge runs the follow op, and refs arrive. This
  is the one test that crosses §A and §B.

---

## C. `floe-catalog` (D50)

### C.1 Crate and features

```
crates/floe-catalog/
  Cargo.toml   # [features] iceberg = ["dep:iceberg", "dep:iceberg-catalog-rest", "dep:iceberg-storage-opendal", "dep:arrow-array", "dep:arrow-schema", "dep:parquet"]
  src/lib.rs        # always: rows, Recorder trait, NoopRecorder, rows_from_entries, CatalogConfig re-export
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
  toolchain is 1.97.1, but `workspace.package.rust-version = "1.90"`, so set `rust-version = "1.94"` on
  `floe-catalog` (or raise the workspace's). Pin exact versions (`=0.10.1`): the project is pre-1.0, and its
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
| `commit_timeout` | `"60s"` | A durable append that is not committed within this fails (cursor not advanced). |
| `backfill` | `false` | First cursor for a repository: `false` = its current `head_seq` (start now); `true` = the retained log start (full history, like the events bridge). |
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

**`repo_inventory`** — slowly changing record of every floe repository and its source. Source: the mirror (one
row per change of a state entry) + a daily full snapshot by the mirror lease holder (`registry.list()` + state).
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

### C.4 Consuming the WAL (durable cursor, D32/D46/D47)

The catalog is a **third target kind of the events bridge**, next to the native webhook (`events/cursor.json`)
and the GitHub facade targets (`github/events/<generation>.json`):

- Cursor key: `repos/<o>/<r>/catalog/cursor.json`, the same `Cursor { published_seq, updated_at }` JSON. It is
  distinct from `floe_proto::keys::CATALOG` (`meta/repos.pb`), which is unrelated.
- `Bridge::new`: a bridge exists when `events` role ∧ (webhook ∨ github facade ∨ **catalog.enabled**).
  `catch_up(id)` joins a third future, `catch_up_catalog(id)`, so each target advances **only its own cursor**
  (the D46 rule). A catalog failure never holds back the webhook or the reverse.
- Generalise `catch_up_target` so a target receives `&[LogEntry]` (it reads them already via
  `read_log_retained`) instead of only `RefEvent`s. The existing sinks keep calling `events::refs_from_entries`.
  The catalog target calls `floe_catalog::rows_from_entries(repo, &entries) -> (Vec<RefEventRow>,
  Vec<ForcePushRow>)` (in the core, pure, golden-tested against `events::refs_from_entries` so `ref_events`
  and webhook events never disagree), then `writer.append_durable(rows).await`, then CAS the cursor.
- Cold cursor: `catalog.backfill = false` ⇒ create the cursor at the current `head_seq` (persisted before any
  delivery, as the bridge does). `true` ⇒ `handle.retained_log_start()` (checkpoint history, D47).
- Wake-ups are the bridge's: `POST /_events/notify` and `events.sweep_interval`. **No `wake()` from
  writers** (that concession is the GitHub facade's alone). Catalog latency ≈ notification latency + flush
  interval, or the sweep interval without notifications.
- Semantics: **at-least-once**. A crash after the Iceberg commit and before the cursor CAS re-appends the
  batch, and so does a `ref_events` commit followed by a failed `force_push_log` commit. Consumers dedup on
  `(repo, seq, ref_name)`. Recommended views: `SELECT DISTINCT` or `ROW_NUMBER() … = 1` over the identifier.
  Exactly-once (watermarks committed atomically in table properties) is post-MVP (R10).

### C.5 Batching, flush and failure isolation (`buffer.rs`, core)

- One writer per process (`Arc<CatalogWriter>`), with one in-memory buffer per table.
- `append_durable(rows) -> Result<(), CatalogError>` (bridge path): enqueue with a oneshot, then wait for the
  commit that contains the rows (group commit, like publish's batch window). It fails fast with
  `CatalogError::Backpressure` when `max_buffer_rows` would be exceeded, and with `Timeout` after
  `commit_timeout`. On any error the bridge leaves the cursor, so the WAL holds the backlog and memory does not.
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
  flusher with retry and backoff (cap 5 min), and until it succeeds durable appends time out (lag) and
  telemetry drops (counted). Nothing on a git, sync, follow or mirror path awaits the writer.
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
writing operational telemetry into the WAL. The daily inventory snapshot bounds what a loss can hide.

### C.7 Tests (§C)

Without a live catalog (run in `just test`):

- `rows_from_entries` golden tests: PUSH with archive meta → one `ForcePushRow` per line and `is_archive` on
  the archive `RefEventRow`; HEAD retargets and SETTINGS/COMPACT emit nothing; zero-OID conventions match
  `events::refs_from_entries` row for row.
- `buffer.rs` with a fake committer (a trait object inside `buffer.rs`; the Iceberg committer is the real
  implementation): flush by rows, flush by age, backpressure, timeout, waiter completion, telemetry drop when
  full, a committer error fails the waiters and keeps the rows out.
- Bridge (`crates/floe-server/tests/events.rs`): with a fake catalog target, a failing catalog leaves
  `catalog/cursor.json` and still advances `events/cursor.json`, and the reverse; `backfill=false` starts at
  head.
- `#[cfg(feature = "iceberg")]` schema tests: Arrow ↔ Iceberg schema round trip, field ids stable (snapshot of
  the schema JSON).

Against RustFS, `#[ignore]` (`just test-catalog`, needs `podman compose --profile catalog up`):
`appends_and_reads_back_all_four_tables`, which creates the namespace and tables, appends via the writer, scans
with `iceberg`'s reader, and asserts counts/values; and `concurrent_writers_retry_conflicts` (two writers, one
table).

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
  + flusher task), sets `recorder`, and passes the writer to `Bridge::new`.
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
include = ["*"]
exclude = []
skip_archived = true
skip_forks = true
include_private = true
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
`FLOE_GITHUB_TOKEN` + a `maintain` host), the naming rule (`gh-acme/widgets`), `floe github sync --once
--dry-run`, the archive ref convention, the no-per-repo-ACL warning (R2), and one line on `--features catalog` +
`[catalog]`. Update the code map (`floe-mirror`, `floe-catalog`) and the `floe-cli` subcommand list.
`docs/LFS.md`: correct the `token_env`-in-settings example (`token_env` is host-only in the code), and mention
`token_env_by_host`.

### D.6 AGENTS.md decisions (append-only; next free number is D48)

- **D48 (2026-10-04) Follow patterns and archive-on-rewrite.** `[upstream] follow` takes git refspec patterns
  (one `*` crossing `/`, `^` negatives; `refs/archive/` and `refs/follow/` are never followed). With
  `on_rewrite = "archive"` (default), a non-fast-forward, a tag move, or a deletion upstream publishes **one**
  PUSH entry that creates `refs/archive/<unix-ts>/<original-ref>` at the old tip (`old_oid = ""`, never
  overwritten) and applies upstream's state, with `meta["follow.archived"]`. An upstream that advertises no
  matching refs never causes deletions. `"refuse"` is D33's behaviour. Follow still bypasses policy.
  Supersedes D33's "fast-forward only" clause; the rest of D33 stands.
- **D49 (2026-10-04) The GitHub mirror decides, follow moves bytes.** `floe-mirror` runs on a `maintain` host
  under `leases/mirror-github.pb`, keeps `mirror/github/state.json` (CAS) and a disposable HTTP cache in the
  bucket, maps `owner/name` to `<prefix>-<owner>/<name>` (identity stays two segments, D5/D26), creates
  repositories by the manifest CAS, and owns only the `[upstream]` table of repositories marked
  `upstream.source = "github:<id>"`. A human edit of that table detaches the repository. It never deletes a
  floe repository, never stores a token (`token_env` + `upstream.token_env_by_host`), and never transfers git
  objects. GitLab/Gitea implement `Source`.
- **D50 (2026-10-04) Iceberg audit tables are a WAL reader, behind a feature.** `floe-catalog` (`--features
  catalog`) writes `ref_events`/`force_push_log` from the WAL as a bridge target with its own cursor
  `catalog/cursor.json` (at-least-once, dedup `(repo, seq, ref_name)`), and `sync_runs`/`repo_inventory` from
  lossy telemetry (`Recorder`). The catalog is never a source of truth, never holds git objects, and its
  outage only adds catalog lag. Git, sync, follow and the mirror never await it.
- **D51 (reserved) Push to an upstream is a WAL reader on the maintaining host** (§E). It is reserved now so
  the rule "a ref is either followed or pushed, never both; entries with `principal = upstream` are never
  pushed" is on record before anyone builds it.

Also update: AGENTS.md §0 document map (this file), §2.1 table (`mirror/github/state.json`, `catalog/cursor.json`,
`refs/archive/`), §2.2 (follow line), `docs/CONTRACT.md` (`floe-mirror`, `floe-catalog` blocks), `docs/EVENTS.md`
(§A.5 paragraph, catalog target), `docs/ROUNDTRIPS.md` (§D.7).

### D.7 Round trips (`docs/ROUNDTRIPS.md` rows to add)

| Operation | Bucket critical path |
|---|---|
| Follow round, nothing moved | unchanged: conditional manifest GET (+ upstream `ls-refs`) |
| Follow round with archive | unchanged: log PUT + manifest CAS (archive refs ride in the same txn) |
| Mirror pass, nothing changed | lease GET/PUT (heartbeat) + state GET + http-cache GET/PUT; GitHub: one request per listing page (304s) |
| Mirror create | manifest PUT(Create) + policy GET + PUT(Create) + manifest GET + log PUT + manifest CAS (settings) + state CAS (amortised per ≤ 10 actions) |
| Catalog catch-up per repo | cursor GET + manifest conditional GET + log GETs + cursor CAS; the Iceberg commit is off the bucket's git path (catalog + table files) |

---

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
| R1 | **Naming deviates** from the requested `<prefix>/<owner>/<repo>`: floe ids are two segments (D5, D26), so we use `<prefix>-<owner>/<repo>`. | Accept (recommended), or open a separate decision to allow nested owners, which touches `RepoId`, routing, the edge contract and the UI. |
| R2 | **No per-repository read ACL**: a mirrored private GitHub repository is readable by every principal with read on floe. | Document loudly; `include_private = false` for shared hosts; per-repo read ACL is a separate feature. |
| R3 | **Follow does not scale linearly**: one sequential loop, Serve-level sync and `packs_fit()` per repo, the whole object set local. Hundreds of repos are fine; thousands, or one huge repo, are not. | `max_repo_size` + `max_new_per_pass`; nudges instead of tight polling; huge repos go through `floe import` on an SSD host and only *then* get `[upstream]`. A post-MVP item is a concurrency limit for follow ops. |
| R4 | Archive refs grow the ref count forever (a repository force-pushed hourly gets about 8.7k archive refs a year), and `ls-refs` advertises them. | Acceptable at these sizes (refs are O(1) on hot paths). Post-MVP: optional `upstream.archive_retention` (still never auto-deletes by default), and hiding `refs/archive/` from v0 advertisement. |
| R5 | **LFS is read-through, not prefetched**: an LFS object never downloaded before the GitHub repository disappears is lost. | Post-MVP `lfs_prefetch` unit. Call it out in README. |
| R6 | **RustFS S3 Tables is preview**: exact REST path, warehouse identifier, and auth (SigV4 vs bearer/OAuth). `iceberg-catalog-rest` 0.10 is not known to sign SigV4. | Verify against the pinned RustFS release before §C lands. If SigV4 is mandatory, add a signing `reqwest` middleware or put the Iceberg REST fixture in front. The ignored test runs against either. |
| R7 | iceberg-rust 0.10 MSRV 1.94 > workspace 1.90; arrow/parquet 58 add compile time and binary size. | Feature-gated; `rust-version` on the crate; decide whether the release image enables `catalog` (recommend: yes for the image, no for `cargo build`). |
| R8 | Tokens: PAT only. GitHub App installation tokens expire hourly, and follow reads an env var. | Post-MVP `TokenProvider` (App JWT → installation token) behind `Source`/`upstream_token_env`; the seam is `Config::upstream_token_env`. |
| R9 | GitHub `has_wiki` is true for empty wikis → a failing `ls-refs` per `follow_interval`. | `wikis = false` default. Post-MVP: the mirror marks the wiki `absent` after N failed follow rounds (needs follow status in the bucket or the catalog). |
| R10 | Catalog is at-least-once; duplicates are possible. | Dedup key + views. Post-MVP: per-repo watermarks in table properties, committed in the same transaction as the append. |
| R11 | `force_push_log` covers upstream rewrites only, not human force pushes to own repos. | Post-MVP: `receive.rs` records `meta["forced"]` (it already classifies `force-push` for policy); the catalog then picks it up with no other change. |
| R12 | Deleted vs. access revoked is indistinguishable for private repositories (both 404). | Both freeze (`gone`/`forbidden`); nothing is deleted, so a wrong guess costs nothing. |
| R13 | Renamed repositories keep their old floe name. | Intentional (stable URLs). A floe-side rename/alias is a separate decision. |
| R14 | Mass deletion upstream (e.g. a force-mirror push that drops branches) archives and deletes at scale. | Nothing is lost (archives). Only an *empty* advertisement is guarded. A `max_delete_fraction` guard is an easy follow-up if wanted. |
| R15 | `on_rewrite = "archive"` as the default changes behaviour for existing `follow` users. | Intended (requirement). Noted in D48 and in the release notes. |
| R16 | Two hosts maintaining one repository (placement misconfig) would both follow it. | Already true for D33. Publish is CAS-safe; archive refs use `old_oid = ""`, so at worst one round's archive name collides and is retried. |
