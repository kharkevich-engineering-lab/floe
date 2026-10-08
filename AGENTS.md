# floe — architecture and operating manual for contributors (humans and agents)

Context: **everyone, first.** Read this before touching the repository: the constraints the design answers
(§1), how the WAL works (§2), the principles a PR is judged against (§3), every design decision in force (§4),
and the working rules (§5). Reading order starts at `GOAL.md`; §0 maps every other document.

> **No backwards compatibility (pre-1.0).** We owe nothing to previous shapes of this system. Do not keep
> aliases, fallbacks, shims, deprecated routes, old config keys, old proto fields or migration code. When a
> decision changes the shape, **delete the old shape in the same change**: routes, clients, tests, docs, config
> keys. Data in the bucket is the one exception — WAL/manifest/log formats stay append-only and replayable,
> because the bucket is the repository; everything else is disposable. If you find yourself writing "still
> accepted for …" or "legacy", stop and remove the thing instead.

floe serves git over smart HTTP (v0/v2), receive-pack, upload-pack, bundle-uri, LFS and a browsing web UI,
written in Rust, from disposable hosts whose only durable state is an object-store bucket. The reference workload
it was built against is a 57 GiB / 73 M-object / 1.4 M-commit / 466 k-ref monorepo with LFS, served from
machines whose "disk" is 20 GiB of tmpfs, next to a long tail of small repositories. The design follows Cursor's
*Git at any scale* (Continuity); `README.md` tells the story, this file keeps the rules.

## 0. Document map (one home per fact — link, don't duplicate)

| Doc | Who / when to read it |
|---|---|
| `GOAL.md` | Everyone, first. What floe is for, the acceptance table, what we do not optimise for. |
| `AGENTS.md` (this) | Everyone. Constraints §1, WAL §2, principles §3, decisions §4, working rules §5. |
| `README.md` | The introduction: why (the Cursor lineage), what it does, how it works briefly, running it, invariants. |
| `docs/BUNDLE_URI_DESIGN.md` | Anyone touching bundles, the scheduler, base rebuilds, or big-repo clone/fetch UX. Design of record; normative config in §4. |
| `docs/ROUNDTRIPS.md` | **Anyone touching a protocol that talks to the bucket** (publish, sync, checkpoints, compaction/leases, bundles, remote reader, store backends). Round trips are the cost model; correct is not sufficient. |
| `docs/POLICY.md` | Anyone touching receive-pack authorization or writing a repo policy. Normative rule language. |
| `docs/LFS.md` | Anyone touching LFS (`lfs.rs`, `lfs_upstream.rs`) or importing a repository whose LFS history lives elsewhere. |
| `docs/INTEGRITY.md` | Anyone touching import, the maintainer's `fsck`/`repair` units, or seeing `connectivity: missing object` on a push. |
| `docs/EVENTS.md` | Anyone changing WAL-derived ref events, the webhook bridge, consumer semantics or event cursors. |
| `docs/GITHUB.md` | Anyone touching `crates/floe-server/src/github/*`, or pointing a GitHub-integrated app at floe for local development. The facade's trust boundary (it has none), URL conventions, the write primitive, known limits. |
| `docs/design/github-mirror.md` | Anyone touching upstream follow patterns/archive (D48), `floe-mirror` (D49), `floe-catalog` (D50, SigV4 D63) or the push-to-upstream seam (D51). Design of record; dated "as landed" notes where the code won. |
| `docs/design/admin-ui.md` | Anyone touching the config store (`floe_server::config_store`), `/api/v1/admin/*`, the SPA's `/_admin` area, `floe config show|set|history|rollback|import`, or adding a runtime config section (D60–D62). |
| `docs/CONTRACT.md` | When you touch a crate boundary. The cross-crate contract; *extend, don't rename*; code wins where they differ. |
| `docs/reference/cursor-git-at-any-scale.md` | The source design, verbatim. Read once before touching WAL/publish/sync/placement. |
| `docs/patches/README.md` | Git client patches (bundle filter matching) and the gate for advertising filtered bundle families together. |
| `web/API.md` | UI/SDK authors and anyone changing `web/*.rs`. Wire contract, caching rules, SSE envelope, tasks, prefix-first lanes. |
| `docs/design/code-intelligence.md` | Anyone touching `crates/floe-codeintel`, the `[codeintel]`/`[mcp]`/`[access]` config, `codeintel.proto` or the MCP endpoints. Design of record for D52–D58 until it moves to `docs/CODEINTEL.md` and `docs/MCP.md`; §13 is the milestone plan, `docs/design/spikes/` the M0 spike outcomes. |
| `web/sdk/README.md` | Users of `repos.js`. |
| `web/README.md` | Frontend engineers changing the React SPA, Vite build, SDK adapter, static assets, loading states. |
| `floe.example.toml` | Every **bootstrap** config key with its default and a comment (runtime sections live in the config store, D60; their schema is `GET /api/v1/admin/config/schema`). Change it with the code. |
| `floe.standalone.toml` | The one-machine shape: `floe-server --config floe.standalone.toml` → `http://floe.localhost:8080/` (loopback; TLS modes in D59). |
| `deploy/nginx.conf.example` | An optional nginx in front; documents the `X-Accel-Redirect` byte-offload contract. |
| `Containerfile`, `flake.nix` | An OCI image; a Nix package, image and devshell. |
| `deploy/helm/floe/README.md` | Anyone deploying to Kubernetes or changing the chart (D64): install examples, TLS modes, the maintain split. |

---

## 1. Constraints we design for

### 1.1 The machine: small, ephemeral, shared-nothing
- Assume an instance of **a few vCPU and tens of GiB of memory where the disk *is* that memory** (tmpfs).
  `cache.max_bytes` (default 20 GiB) is the budget for everything on disk. A monorepo's base pack alone can be
  30+ GB: **it cannot be materialized there, ever.** A host with a real SSD (`cache.mode = "disk"`) is the
  exception, placed by configuration, not the rule.
- **Instances are ephemeral and shared-nothing**: no stable identity, no node-to-node networking, no gossip.
  Explicit placement (D30) decides which hosts perform a repository's object work; refs-level reads remain
  available everywhere, including during a deploy.
- **CPU may be throttled between requests** on serverless platforms; background work there must be bounded
  (`wal.prefetch_max_bytes`).
- **Object store facts**: ~60–80 ms per GET, ~100 MB/s per connection (stripe for more), conditional GET ~15 ms;
  same-object overwrite is serialized (~1 write/s) — a single CAS'd object is a throughput cap.
- Operations that need real disk or hours of CPU (a full `repack -adb` of a monorepo, building its 30 GB weekly
  bundle) run on a host with an SSD using the same binary and the same WAL/lease protocol (`floe compact
  --base`, `floe import`, the `maintain` role with `maintenance.disk = "ssd"`).

### 1.2 What follows
- **Never fully materialize a big repo on a small instance.** Serve from the parts that fit: refs from the WAL
  (ref snapshot + log tail), pack *indexes* + bitmaps + commit-graph locally, pack *data* by range read from the
  bucket (remote reader) or a read-only bucket mount, recent small packs locally. Clone bytes come from static
  bundles, never through upload-pack.
- **An instance must become useful in seconds**, cold: refs in < 1 s (one manifest GET + snapshot + tail),
  objects for the web UI within the first request (remote reader), fetch remainders from local small packs.
- **Everything an instance computes that is immutable is cached for everyone**: in-process LRU and, where a
  second instance would otherwise recompute, the bucket itself (`cache/api/v1/*.json`). Wiping every instance
  loses nothing but warmth.
- **No silent waiting.** Long work is a *task* (id, log, lock, attachable stream) and is narrated to the
  client: SSE envelope for the web UI, sideband band-2 lines for git. "Cloning into… and then nothing" is a bug.

### 1.3 Security contract (`Config::validate` fails closed)
- Three auth modes (`server.auth.mode`): **`none`** (everyone is `anon` with write and admin — `validate` refuses unless `server.listen` is loopback),
  **`token`** (static tokens from the config, as `Authorization: Bearer` or an HTTP Basic password), **`oidc`**
  (any OpenID Connect issuer via discovery). In `oidc` mode `anonymous_read` must be false and an allowlist
  (`allowed_domains`/`allowed_emails`) must exist; three credentials are accepted — an ID token from the issuer
  (RS256/ES256, `iss`, `exp`, `aud` ∈ `audiences` ∪ {`oauth_client_id`}, `email_verified`), a **floe access
  token** (`wgt_…`, HMAC-signed with `session_secret`, minted at `/_auth/tokens` by a signed-in browser, stateless,
  `access_token_ttl`; rotating the secret revokes all), and the HMAC **session cookie** set by `/_auth/login` →
  issuer → `/_auth/callback`. Static `tokens` work in `oidc` mode too (robots). Every path ends in the same
  allowlist and `write_domains`.
- Open at the application (no credential): `/healthz`, `/readyz`, `/repos.js`, `/repos.mjs`, `/_auth/*` (the
  sign-in flow itself) and **`/services/public/*`** (data-free; today only `install.sh`; everything else
  under it 404; never reads repo data or takes a bearer — test `public_lane_serves_only_the_installer_without_auth`).
- **The server answers an invalid/expired credential with a real 401** — that is what makes git `erase` it from
  its helpers and ask again; the friendly 200 + in-band ERR is reserved for failures a retry cannot fix (account
  not allowed, verifier down). A 200 leaves git re-storing a dead token for its cache's lifetime.
- **An edge announces what it took over, per request, in `X-Floe-Capabilities`** (D39): `client-authorization`
  (the client's bearer travels in `X-Floe-Authorization`; `Authorization` is the hop's own credential and is
  never read as the client's) and `accel-redirect` (static bytes by `X-Accel-Redirect`, honoured only when
  `server.accel_redirect = true` **and the TCP peer is loopback**). Hit directly, nothing is assumed.
- **TLS (D59)**: `server.tls.mode = off | files | acme`. Whatever floe presents chains to a CA clients already
  trust, or there is no TLS — **there is no self-signed mode and nothing for a client to pin**. `validate` fails
  closed: `acme` needs `domains`, `email`, the Cloudflare token env var name and `storage_key_env`, takes only
  `challenge = "dns-01"`; keys of another mode are refused; a missing token or storage key is a startup error.
  Tokens, the account key and private keys are never logged; private keys are sealed (AES-256-GCM) in the
  bucket. The certificate status (with the last renewal error) is admin-only at `GET /api/v1/tls`.
- **The installer** (`crates/floe-server/src/setup.rs`, POSIX sh, idempotent): git ≥ 2.46 + curl, TLS always
  verified (never `-k`, never `sslCAInfo`); takes the token from `$FLOE_TOKEN`, an already stored one, or the terminal (no terminal:
  exit 2 with the two things to do); writes `~/.config/git/<host>-token` (0600) and the credential helper
  `<host>-credential-helper` (`get` → `authtype=Bearer`; `store` keeps what git hands it; `erase` on a 401 deletes
  the token and names `/_auth/tokens`); sets exactly `credential.https://<host>.helper` = `""` then ours,
  `transfer.bundleURI true`, `fetch.uriProtocols https`; unsets stale `fetch.bundleURI`/`extraHeader`; self-tests
  (`/api/v1/me`, or `ls-remote` of `?repo=`). Recipes come from one place (`setup::Recipes`,
  `/services/setup.json`); the Clone menu, API page, overview and git error help render them.
- **Bundle lists, two of them (D41)**: `bundles/list` is the *clone* list (fulls + chain; advertised in v2);
  **`bundles/catchup`** is the same list without the fulls and is what every recipe records in `fetch.bundleURI`
  — git's creationToken walk would otherwise download every full newer than the client's token.
  Blobless: `…/list?filter=blob:none` / `…/catchup?filter=blob:none`.
- **Host to host**: a front that forwards pushes to a broker presents `wal.push_broker_token` (or
  `FLOE_BROKER_TOKEN`); the broker lists it in `tokens` and its principal in `trusted_forwarders`, so the end
  user travels in `X-Floe-Principal`.
- Tests never write the user's global git config (private `GIT_CONFIG_GLOBAL`, `tests/lib-auth.sh`).

### 1.4 Requirements (the bar)
- Full git surface: smart HTTP v0/v2, ls-refs with prefixes, fetch with filter/shallow/deepen/sideband-all,
  receive-pack (atomic, delete, tags, push options, report-status-v2), bundle-uri (v2 command + static list),
  LFS batch/basic transfer, `<owner>/<repo>` namespaces. Every read on every instance is as fresh as a fetch:
  push acknowledged ⇒ the next request anywhere sees it.
- Cost must not scale with ref count on any hot path (O(1) `refs`, O(k) `resolve`, paged ref lists, prefix
  filtered advertisements) nor with pack size on a too-small instance.
- Clones of big repos are **static**: the server hands out URLs, the bucket/CDN moves the bytes. `bundles.require`
  rejects clones that skip bundle-uri, with the exact fix in the error text.
- Acceptance for the monorepo: cold instance serves `info/refs`/ls-refs/web `refs` in < 1 s; web UI renders
  main's tree/blob/commits without packs on disk; `git clone` (bundle-uri) + `git fetch` work against a tmpfs
  host; `clone --filter=blob:none --depth=1 --sparse --single-branch` in seconds (reference: 2075 s → 8 s).
  Bounded/CI clones must pass `-c transfer.bundleURI=false`: git downloads the advertised full bundle first
  otherwise (the server cannot see the filter at bundle-uri time).

---

## 2. The WAL — source of truth, and how we squeeze value out of it

### 2.1 Objects and the commit point (`repos/<owner>/<repo>/…`)
| Object | Role |
|---|---|
| `manifest.pb` | Tiny, **CAS-rewritten**: `head_seq`, live pack set `PackRef[]` (checksum, sizes, tier, has_rev/bitmap), log segments, checkpoint pointer, settings inline, `revision`, `updated_at`. **The linearization point.** Nothing is visible before its CAS; everything after is idempotent and replayable. |
| `log/<first_seq>.pb` | Immutable, uvarint-framed `LogEntry` frames: PUSH / REF_UPDATE (ref transaction + pack pointer), COMPACT (new pack, `supersedes[]`), CHECKPOINT, SETTINGS. Strictly increasing `seq`. One small object per publish batch. |
| `wal/<checksum>.pack/.idx/.rev/.bitmap/.commit-graph` | Immutable packs, content-addressed by pack checksum: push packs (tier 0), compaction outputs (tier 1), the base (tier 2, bitmap'd), plus the side-files a reader needs. |
| `checkpoints/<seq>/checkpoint.pb`, `refs.pb` | Folded state at `seq`: live pack set + full `RefSnapshot`. Cold start = snapshot + tail, never full replay. |
| `bundles/list.pb`, `bundles/<strategy>/…` | bundle-uri artefacts + CAS'd list. |
| `leases/<name>.pb` | CAS lease with TTL heartbeat: `compact`, `bundle:<strategy>`. The only cross-instance mutex. |
| `cache/api/v1/<sha1>.json` | Shared render cache of immutable web API answers. |
| `policy.json` | Per-repo push policy (rule language, not on the WAL). `docs/POLICY.md`. Missing = allow-all, except the built-in `archive-immutable` rule on `refs/archive/**` (D48). |
| `fsck.pb` | Last connectivity audit (`FsckReport`), written by the maintainer's `fsck` unit, consumed by `repair` (`docs/INTEGRITY.md`). |
| `events/cursor.json` | Durable acknowledged WAL sequence of the events bridge; advanced only after the webhook acknowledged (D32). |
| `catalog/cursor.json` | The catalog tail's own cursor (D50): last seq whose rows the Iceberg catalog committed. Unrelated to the bucket-root `meta/repos.pb` catalog. |
| `refs/archive/<unix-ts>/<ref>` (a ref, not an object) | Old tips that upstream follow kept when upstream rewrote or deleted a followed ref (D48); never overwritten, never followed, immutable to pushers (built-in policy rule `archive-immutable`). |
| bucket root: `mirror/github/state.json`, `mirror/github/http-cache.json` | The GitHub mirror's state (CAS, generation-guarded) and its disposable ETag cache (`PutMode::Overwrite`), D49. |
| bucket root: `catalog/epoch.json`, `catalog/inventory.json`, `leases/mirror-github.pb`, `leases/catalog-inventory.pb` | When the catalog was first enabled (create-once), the daily inventory snapshot's schedule (CAS), and the two fleet-wide leases (D49/D50). |
| bucket root: `tls/acme/<dir>/account.json`, `tls/acme/<set>/cert.json`, `tls/acme/<set>/status.json`, `leases/tls-acme-<set>.pb` | `server.tls.mode = "acme"` (D59): the ACME account (key sealed), the chain + sealed private key every instance revalidates by conditional GET, the shared renewal backoff, and the order lease. `<dir>` = hash of the directory URL, `<set>` = hash of directory + sorted domains. Not WAL. |
| `lfs/objects/<aa>/<bb>/<oid>` | LFS objects (sha256-addressed, immutable). Missing ones can be read through from `upstream.lfs` and persisted (`docs/LFS.md`). |
Schema `crates/floe-proto/proto/floe/v1/wal.proto`; GCS over gRPC, S3 (AWS SDK) and in-memory stores share
one contract suite (`crates/floe-store/tests/contract.rs`, incl. compose).

### 2.2 Write path
receive-pack (ours, `floe-git/src/receive.rs`) → index the pack locally (`git index-pack --stdin --fix-thin
--keep --rev-index --threads=0`, `--fsck-objects` when `wal.fsck_objects`) in a per-ingest scratch git dir (a
rejected push leaves nothing behind) → connectivity per config (`spawn_blocking`) → **push policy**
(`docs/POLICY.md`) → `pack PUT ∥ idx PUT ∥ log PUT` → **manifest CAS** (group commit per repo per instance,
`wal.batch_window`) → commit local ref txn → `ok` to the client. On 412: refetch, re-validate every old value
(moved ref ⇒ `ng`), re-seq, retry with jittered backoff. **Who writes (D28): the host that maintains a repo.**
Hosts that maintain nothing may forward receive-pack to a **push broker** (`wal.push_broker_url`) so one warm
writer batches the CAS; fallback to the local path if the broker is down. Publish is CAS-safe, so disjoint writer
sets are correct by construction; the broker is an optimization, never a dependency. Never ACK before the bucket
ACKs. **Upstream follow** (`floe_server::follow`, D33/D48) is the second writer shape: refs matching a repo's
`[upstream] follow` patterns are brought up to `upstream.git`'s by the maintaining host (a refs-level probe every
`maintenance.follow_interval`; Serve-level work only when something moved) through the same ingest → classify →
connectivity → `publish_push` path, `principal = upstream`; rewritten/deleted tips are kept under
`refs/archive/<unix-ts>/<ref>` in the same entry.

### 2.3 Read path — sync levels (`RepoHandle::sync_*`, `floe-wal/src/handle.rs`, `sync.rs`)
Every request: conditional GET of `manifest.pb` (skippable for `wal.freshness_ttl`) → 304 serve / 200 apply.
| Level | Brings | Used by |
|---|---|---|
| **Refs** | checkpoint `RefSnapshot` + every log entry's ref txn → `packed-refs`. No packs. | `info/refs`, `ls-refs`, `bundle-uri`, web `refs`/`resolve`/overview, read_log |
| **Serve** (`sync()`) | Refs + the pack set as the instance can hold it: tiers < 2 and the **history pack** (D18) local; a tier-2 base as side-files (idx/rev/bitmap/commit-graph layer) + `pack-<sha>.pack` linked into a read-only bucket mount (`cache.store_mount`) or **remote-served** without one; midx over history + base | upload-pack, receive-pack, bundle builds, prewarm |
| **Full** (`sync_full()`) | Refs + every live pack local (striped parallel range downloads); only for repos that fit | base rebuilds (`compact --base`), geometric compaction |
| **Objects** | Serve when the pack set fits `cache.max_bytes`, else Refs + **remote reader** → `ObjectAccess::{Local,Remote}` | web API object endpoints |
Long syncs register as tasks (`materialize`, `remote-index`) and stream progress; pack work runs on the **bulk
runtime** and never takes the refs phase's lock (D19). `check_fits` refuses to pull a pack set that cannot fit
(TooLarge → 503 / pkt ERR with the bundle-uri fix). Local disk is a bounded LRU (`cache.max_bytes`,
`evict_idle_after`) in budget mode, watermark eviction in disk mode (D25).

### 2.4 Getting the most out of the WAL (the strategies)
- **Refs-first everything.** Ref advertisement, peeled tags (`RefUpdate.new_peeled` recorded by the writer so
  replicas advertise `^{}` without objects), web `refs`/`resolve` — all from snapshot + tail, no objects.
- **Remote-served bases + gix fetch engine**: at Serve level with no store mount and a pack set that does not fit,
  tier-2 bases are remote-served (`PackPlan::Remote`: commit-graph layer local, nothing else); `git fetch` then
  runs the gix engine (`upload_gix.rs`): commit-graph walk, tree diff, base objects faulted by range, streamed.
- **Remote reader for too-large repos** (`floe-wal/src/remote.rs`): pack indexes local, data by 1 MiB range
  reads through a process-wide block LRU (`cache.remote_block_bytes`), incremental inflate, iterative delta
  resolution with a decoded-object LRU. The web API faults exactly the objects a git command will touch into the
  loose store (`web/objects.rs`) and runs unchanged `git` renderers. The same contract test runs with a 1-byte cache.
- **Side-files published with packs**: `.idx`, `.rev`, `.bitmap`, and for tier-2 bases a **split commit-graph
  layer** readers install as their chain base. Every pack writer produces `.rev` (`pack.writeReverseIndex`; git
  ≥ 2.47 on the server); a published pack ≥ 250 k objects without one gets it from the maintainer (`rev-index`
  unit) — without it git rebuilds the reverse index in memory on every `pack-objects` (2.85 s per fetch on a
  60 M-object repository).
- **Checkpoints fold the log** when `wal.snapshot_every_entries` OR `wal.checkpoint_interval` OR
  `wal.checkpoint_tail_bytes` fires — the checkpoint is the unit of serving state. Writing one is refs-level work,
  so each `maintain` host checkpoints its own repositories.
- **Provenance for free**: every push and repack is a log entry; `floe wal ls|show|materialize --at-seq`.
- **Never LIST on a hot path**; 404s are free; probe, don't list. Immutable objects get
  `Cache-Control: public, max-age=31536000, immutable` + strong ETag + Range everywhere (D10 static contract).

### 2.5 Compaction (WAL + git), leader by lease
- Assigned maintainers run **geometric** folding of fresh packs (tier 0 → tier 1, `git repack -d --geometric
  --write-midx`) under `leases/compact.pb`; the result is a COMPACT entry; followers download the new pack and
  drop superseded ones after in-flight readers finish. Triggers: `compaction.trigger_packs`, `trigger_bytes` —
  and at least **two** fresh packs (one pack folds into itself).
- **The base (tier 2, one pack + bitmap) is rebuilt only when the weekly bundle is built, and only on an ssd
  host** — the maintainer's `BaseRebuild` unit, followed by the weekly slot's compose
  (`docs/BUNDLE_URI_DESIGN.md §5`; `floe compact --base` is the manual form). The rebuild runs in a scratch copy
  under `<cache.dir>/_rebuild/` that survives the process and resumes after a restart iff the WAL head has not
  moved (§5a); the serving copy is never rewritten. Invariant that makes serving cheap: *everything newer than the
  base lives in small/medium packs that fit on every instance.*
- **A fold never touches the base or a history pack** (`--keep-pack`), **a base is rebuilt only by the weekly
  unit / `compact --base`**, and **a rebuild supersedes every other live pack** by the manifest, not by what git
  happened to delete.
- Superseded packs are retained `compaction.retention_superseded` (provenance window) then GC'd.

### 2.5b Self-healing by construction (D22)
Everything the maintainer produces — checkpoints, bundles per slot, compactions, retention — is a **pure function
of (config, WAL state)**. The maintainer does not run schedules; it computes the *desired state* every pass and
performs **one bounded unit of the most important missing work** (checkpoint → repair → missing weekly → missing
dailies oldest-first → missing hourlies → compaction → rev-index → fsck audit), as a task, under a lease. An outage
of any length leaves no permanent hole; a deleted or corrupt artefact is "missing" and rebuilt identically; config
changes take effect by re-planning; there are no one-off backfill scripts.

### 2.6 Bundles (bundle-uri): move clone bytes to static files — **the north star** (`docs/BUNDLE_URI_DESIGN.md`)
- Strategies (config, D21/D22): **weekly full** (for a big repo = base pack ∘ header via store-side compose — GCS
  natively, S3 by multipart `UploadPartCopy`; no disk, no index-pack), **daily incremental** chained on the previous
  daily, **hourly incremental** on the newest daily, each on a calendar slot (6-field UTC cron `schedule`; the fire
  time is the as-of instant), `creationToken = slot epoch`, main-only refs for `bundles.main_only` repos;
  `bundles/list.pb` CAS'd.
- Served as static objects (`/{o}/{r}.git/bundles/...`, ETag/Range, immutable) by floe or offloaded to an edge
  (`X-Accel-Redirect`, `deploy/nginx.conf.example`), or as presigned store URLs (`serve_via = "signed_url"`);
  advertised in the v2 capability list, at `bundles/list` and on band 2 of every narrated fetch. `bundles.require`
  refuses *unbounded* zero-have fetches of listed repos with the exact fix (D17).
- Builds run on the **maintainer** under `leases/bundle:<strategy>` (Serve sync first; unresolvable tips skipped
  with a notice); weekly full is a compose.
- **Blobless family** (design §6b): strategies with `filter = "blob:none"` — weekly-history = header ∘ the D18
  history pack, incrementals `--filter=blob:none` — advertised ONLY at `bundles/list?filter=blob:none`.

### 2.7 Tasks, progress, narration (`floe-wal/src/tasks.rs`, `crates/floe-server/src/sse.rs`, `smart.rs`)
Any long work = a task: unique id, per-instance log (`GET …/tasks`), `(repo, kind)` lock (a second start joins),
replayable packet stream (`GET …/tasks/{id}`). Packets: `notice`, `progress {label,done,total?,unit,percent?}`,
`task`, terminal `result` | `error`. Web: any JSON endpoint that cannot answer immediately streams the SSE
envelope when the client accepts `text/event-stream`; fast answers stay plain cacheable JSON. Git: v2 `fetch`
advertises `sideband-all` and narrates on band 2 (`remote: * …`). Startup: `cache.prewarm` repos run a `prewarm`
task; `/readyz` is 503 until done or `prewarm_ready_timeout`. `no-progress` is honoured.

---
## 3. Principles (what a PR is judged against)

One idea: **the bucket is the repository; everything else is a cache or a reader of the log.** Ten consequences.
A change that feels natural on a conventional git host (a table, a queue, a cache server, a webhook from the push
path, a bigger disk) is usually a violation here. A violation means the principle is wrong — amend it with a
decision in §4 — or the PR is; never "fix later".

| # | Principle | The tell in a PR | The question to answer |
|---|---|---|---|
| **I** | **No state outside the object store.** Disk and memory are caches. | A database, Redis, SQLite, a file that must survive a restart, an env var that encodes data. | "If every instance is wiped now, what is lost?" — must be "warmth". |
| **II** | **The manifest CAS is the only commit point.** Immutable objects are never overwritten (`PutMode::Overwrite` only on the manifest, bundle list, leases, fsck.pb, events/cursor, maintainer heartbeats, render cache, the mirror's HTTP cache, the `tls/acme/*` objects written under the `tls-acme` lease, and the code-intel pointers of D52: `codeintel/head.pb`, `codeintel/dir/head.pb`, `codeintel/tasks/*.json`, `codeintel/tables.json`). | A second "commit" (a flag file, a list update that makes data visible), an ACK before the bucket's. | "What does a client on another instance see between the PUT and the CAS?" — nothing new. |
| **III** | **Side effects are readers of the WAL, never steps of a write.** Events, mirrors, notifications tail the log from a durable cursor. | A webhook/HTTP call from `receive.rs`, `publish.rs`, `follow.rs`, `smart.rs`. | "If this side effect fails, does the push?" — no. "Is it replayable from the cursor?" — yes. |
| **IV** | **Every read revalidates; there is no eventually.** | A cache that outlives the manifest's generation, a TTL invented for a repo-scoped answer, a read that skips `sync_*`. | "After `push` returns `ok`, can any instance serve the old state?" (`cargo test -p floe-server --test sim`). |
| **V** | **Serve from the parts that fit; never a bigger box, never a hard-coded host.** | "Just download the pack", a path that assumes the full pack set is local, a hostname in `crates/` or `web/`. | "What happens to this code on a 20 GiB tmpfs with a 32 GB base pack? Which sync level does it need?" |
| **VI** | **Never block the async runtime; bulk bytes never share a lane with the control plane.** | `Command::new(...).output()` or `std::fs` big reads in an `async fn` outside `spawn_blocking`/the bulk runtime; a queued writer on `RepoHandle::rw` from an install path. | "On which thread does this run, and what holds `sync_mutex`/`rw` while it does?" (e2e `blocking_work_in_the_install_path_does_not_stall_requests`). |
| **VII** | **No LIST on a hot path; count the round trips.** | A `.list(` in request handling; a new GET "just to check"; a protocol change without a `docs/ROUNDTRIPS.md` row. | Before/after depth in the commit; the sim asserts request budgets (`FaultStore::stats().ops`). |
| **VIII** | **Standalone first; the edge announces, the app never assumes.** | Reading an `X-Floe-*` request header without checking `X-Floe-Capabilities`; a feature only testable behind nginx. | "Does this work on `floe.standalone.toml` with nothing in front?" |
| **IX** | **No silent waiting.** | A new op that blocks a handler, a loop without a task, a `sleep` a client would wait on. | "Where does the client see this taking time?" — a task kind, a progress packet or a band-2 line. |
| **X** | **Keep floe small.** Upstream `git` does git things; gix only where measured faster and correct; one config file, one auth story, one SDK; scope is `GOAL.md §4`. Proto append-only. | A new dependency without a why; a reimplementation of something `git` does; an alias/shim/"legacy" branch; a removed proto field. | "Which line of GOAL §4 is this for?" |

---

## 4. Decisions in force (append, never silently change)
- **D1** Rust, tokio + axum, HTTP/2 (h2c or ALPN), streaming both ways, gzip request bodies.
- **D2** gix in-process where it is correct and measured; upstream `git` for delta-compressing repack/bitmaps,
  bundle creation, and normal upload-pack. **`git.upload_pack_engine = "auto"` selects stock git wherever it can
  read the packs** (local copies or mount-linked bases); gix handles only a remote-served base. **The gix engine
  never emits thin packs** (`upload_gix.rs`): gix-pack's thin-pack path builds the base pack's whole (offset → oid)
  table per chunk per thread (60 M entries × 44 threads ⇒ 178 GB RSS) and is the only place gix writes an object id
  into a pack (a mid-pack refresh once paired offsets with another pack's table ⇒ a wrong id). The gix pack source
  is a frozen snapshot (`frozen_pack_source`). Reproducer: `floe-git/tests/upload_gix_scale.rs`.
- **D3** `ObjectStore` trait with CAS version tokens, conditional GET, range, compose; gcs/s3/memory backends.
  `compose` is native on GCS and a multipart `UploadPartCopy` on S3 (`compose_is_native` tells callers which);
  `accel_target` gives an edge a URL (+ bearer on GCS, presigned on S3) to fetch an object itself.
- **D4** protobuf on the wire and in the bucket; schema versioned, append-only.
- **D5** Repo identity `<owner>/<repo>[.git]`, prefix `repos/<o>/<r>/`, creation = CAS create of the manifest.
- **D6** Manifest CAS is the only commit point. **D7** No node identity, no elections; leases for exclusivity.
- **D8** `floe.toml` only (+ `FLOE__` env overrides) — for bootstrap keys since D60; runtime keys live in the config store. **D9** One binary, roles by config (`serve`,
  `maintain`, `events`; `maintain` includes compaction and bundles).
- **D10** One static-serving code path for every immutable byte (ETag/304/If-Range/Range/416/HEAD/immutable;
  UI assets precompressed at build; store objects never compressed at request time).
- **D11** Too-large repos are served, not refused: remote reader for the web API; clones via bundle-uri; refs from
  the WAL. Object work returns 503 when remote objects are disabled or the repository is excluded from this host's
  serving placement (D30).
- **D12** Auth is `none` | `token` | `oidc` (§1.3). `oidc` is generic OpenID Connect through discovery; the
  floe-issued access token (`wgt_…`, HMAC, stateless, `/_auth/tokens`) is the credential git uses, so no client
  needs a vendor CLI to mint tokens. An edge that wants to do auth itself uses `auth_request /_auth/check`
  (`deploy/nginx.conf.example`).
- **D13** Long work is a task and is narrated (§2.7); no endpoint may block silently.
- **D14** Web toolchain: pnpm + Vite only. The build fails unless the optimized SPA artefacts are present.
- **D15** Repo-scoped API lives at `/{o}/{r}/api/…` (refs, resolve, tree, blob, commits, commit, overview, ops,
  tasks, policy, settings) so the repository prefix determines placement. The browser lane is
  `/{o}/{r}/api-browser/…`; `/api/v1` is non-repository discovery, identity and owner listing. No aliases.
- **D16** Push authorization is a per-repo **rule language** at `repos/<o>/<r>/policy.json` (`docs/POLICY.md`).
  Envelope `version` + `groups` + `rules`; `protect` = AND. Empty/missing = anyone with write may move any ref.
- **D17** `bundles.require` refuses only **unbounded** zero-have fetches (no `deepen*`, no `filter`): that is a
  full clone and belongs to bundle-uri. Bounded zero-have fetches (CI's `--depth`/`--filter`) go to upload-pack.
  git never retries a failed bundle download and then falls back to a zero-have fetch; a principal that fetched
  the repo's `bundles/list` within the hour gets **one upload-pack full clone per 6 h** with a loud band-2
  WARNING; everyone else is refused with the truthful message.
- **D18** **History pack**: a tier-2 base is published with a derived `pack-objects --filter=blob:none` pack
  (commits + trees; `PackRef.kind = HISTORY`, `derived_from = <base>`) that every instance keeps as a real local
  pack; blob bytes are all that cross the mount / range reads. Superseded with its base.
- **D19** **The serving runtime is untouchable.** (1) control-plane store objects (manifest, log, checkpoints,
  leases, bundle list, policy, render cache) never share a transport or a permit with bulk bytes (packs, idx,
  side-files, bundles, LFS, ranged reads: their own pools under `store.gcs.bulk_concurrency`); (2) pack
  materialization never runs on the serving runtime (`sync::on_bulk_runtime`) and never queues as a writer on
  `RepoHandle::rw` — the refs phase needs only `sync_mutex`, pack removal is `try_write()` (a queued writer on a
  tokio RwLock blocks every new reader; one 24-minute clone once starved every info/refs for minutes).
- **D20** **One API, two lanes, one SDK**: `/{o}/{r}/api/…` accepts a bearer or the same-origin session;
  `/{o}/{r}/api-browser/…` is the cross-origin browser lane (`credentials: "include"`, CORS only for
  `server.cors_origins`, sign-in popup at `/api-browser/v1/authenticate`). The SDK **`repos.js`**
  (`web/sdk/repos.ts`; IIFE registers `window.repos`, ESM `repos.mjs`) maps the whole surface, picks the lane,
  does the popup dance and unwraps the SSE envelope; served at `/repos.js` (open, so a `<script>` tag can load it).
  **Dogfood rule**: the bundled UI is built on the SDK (`web/src/api.ts` is an adapter); fix the SDK, never fork.
- **D21** **No monthly bundle layer.** git's `creationToken` heuristic never walks past a full bundle; two layers
  of incrementals (daily, hourly) under one full (weekly) is the shape. Incrementals without `chain` list only
  their 2 newest per strategy (the second so a client that read the list a slot ago never 404s mid-chain);
  **`chain = true`** cuts a slot on the strategy's own previous bundle while that is newer than the newest base
  bundle — every slot since the base is wanted and kept, each exactly its delta. **Dailies are chained by
  default**; hourlies stay on the newest daily with 2 kept. `slots::base_for_incremental` is the one place that
  knows either rule.
- **D22** **Bundles are cut on calendar slots, main-only, as-of-slot, with a contiguous chain**
  (`docs/BUNDLE_URI_DESIGN.md` §3/§4): a 6-field UTC cron `schedule` whose fire time *is* the slot; missing full
  slots backfilled oldest-first (`backfill_max`); content is main as of the highest WAL seq with `created_at ≤
  slot`; `creationToken = slot epoch`; prerequisites = tips of the newest `base` bundle at or before the slot;
  retention = `keep` fulls + the chain under them; refs default to `HEAD` + `refs/heads/main` for
  `bundles.main_only` repos (+ `extra_refs`).
- **D23** **An edge is optional and load-bearing only for bytes.** An nginx in front may terminate TLS, cache one
  `auth_request` verdict per credential, route by `/<owner>/<repo>`, and offload static bytes by `X-Accel-Redirect`
  — only when it injects `X-Floe-Capabilities: accel-redirect`; the app's answer supplies `X-Floe-Store-Url` /
  `-Authorization` / `-Key` and the edge slices and caches. `deploy/nginx.conf.example` is the reference; nothing
  in `crates/` knows a hostname.
- **D24** **Per-repo settings live in the WAL.** `RepoSettings {toml, revision, author, updated_at, message}` — a
  TOML document restricted to `[bundles]`, `[maintenance]`, `[compaction]`, `[upstream]` (≤ 16 KiB) — is published
  as a `SETTINGS` log entry **and inline on `manifest.pb`** (every refs-level sync sees the effective config at no
  extra round trip). Effective config = host `floe.toml` ⊕ env ⊕ settings (`Config::with_settings`). Validated at
  publish; invalid = 400, nothing published. Surface: `GET|PUT|DELETE /{o}/{r}/api/settings`, `…/effective`,
  `…/history`; `floe repo settings show|set|clear|history`. Not in settings: auth, store, server, wal, cache,
  `upstream.token_env` (host-only). `GET …/effective` returns only those sections. PUT/DELETE of settings and of
  `policy.json` require an **admin** principal (`tokens[].admin`, or oidc `admin_emails`/`admin_domains`;
  `mode = none` is admin on loopback). Write is push, not admin. *2026-10-04:* the document may also carry
  `[repo]`, the repository's own metadata, never merged into the config (`floe_config::RepoMeta`): today
  `description` (one line, ≤ 512 chars), shown as `description` by `GET …/api/overview` and the web UI; the GitHub
  mirror writes `Mirror of <url>` there (D49).
- **D25** **Two cache modes.** `cache.mode = "budget" | "disk" | "auto"` (auto = disk when `maintenance.disk =
  "ssd"`); in **disk** mode `cache_budget_bytes()` is 0 = unlimited — every repo fully local, never the
  too-large/remote/link path, eviction only under disk pressure (`cache.disk_high_watermark`, idle-oldest first).
- **D26** **Routing is by repo prefix, nothing else.** Everything whose path starts with `/<owner>/<repo>` (git
  smart HTTP, `.git` suffix, bundles, LFS, `/{o}/{r}/api*`, `/{o}/{r}/settings`, the SPA pages) is one routing
  unit; an edge maps `^/<o>/<r>[./?]` → a host. "Which machine serves a repo" is decidable from the first path
  segments alone (the `Server` header shows it). **D27** Lanes are a segment *after* the repo prefix
  (`/api`, `/api-browser`); the prefix is still the only routing key.
- **D28** **The host that maintains a repo is its writer.** Forwarding receive-pack to a broker
  (`wal.push_broker_url` + `push_broker_token`) is for hosts that maintain nothing. A `maintain` host refuses a push
  for a repo it does not `assigned()` (band-2 line + `ng`). Publish is CAS-safe, so two writer hosts for two
  disjoint repo sets is correct by construction; the broker is an optimisation.
- **D29** **An edge's fallback for a placed repo is read-only and refs-level.** GETs (info/refs, API, UI, bundle
  list, bundles) may fall back to another host; git RPC and LFS writes have no fallback and get **503 +
  `Retry-After: 15`** — a replayed POST either hangs or warms the wrong host while the client sees nothing.
- **D30** **Placement is configured, not inferred.** `[placement] serve / serve_exclude / maintain /
  maintain_exclude` (globs). A host that does not **serve** a repo answers its object work with **503 +
  `Retry-After: 15`** and a pkt-line `ERR floe: <repo> is served by <host>; retry shortly` *before* any sync;
  refs-level reads stay available everywhere. **maintain** gates the maintainer loop.
  Test: `host_excluded_from_serving_a_repo_refuses_object_work_with_503`.
- **D31** **Draining must not stop serving.** *Phase 1* (SIGTERM): the maintenance loop starts no new unit and
  the running unit is **interrupted at once** (D22 redoes it; a unit too expensive to redo is made resumable —
  `BaseRebuild`) while the instance serves everything normally, `/readyz` 200; bounded 30 s. *Phase 2*:
  `/readyz` 503 + Retry-After, new fetch/push/LFS refused with 503 before any work, in-flight requests get
  `server.drain_timeout`, exit. Test `tests/drain.rs`.
- **D32** **Events are produced from the WAL by one small service, never by the push path** (`docs/EVENTS.md`).
  The **events bridge** (`roles=["events"]`) tails each repo's log from a durable per-repo cursor
  (`events/cursor.json`), converts committed PUSH/REF_UPDATE entries to `ref` events, POSTs each batch to
  `events.webhook_url` (JSON array; `X-Floe-Delivery` = sha1 of the body; `X-Floe-Signature: sha256=<HMAC>`
  when `webhook_secret` is set), advances the cursor: published iff durable, a crash loses nothing, lag =
  `head_seq − cursor`. Writers and the WAL crate contain no event code. Wake-ups: `POST /_events/notify` from a
  bucket notification (at-least-once) + a periodic sweep as backstop. Dedup key `(repo, seq, ref_name)`.
- **D33** **A repository's refs can follow an upstream git host, through the WAL, by its maintainer.**
  `[upstream] follow = ["refs/heads/main"]` (needs `upstream.git`; `upstream.token_env` names the env var with the
  token) makes the maintaining host fetch the delta every `maintenance.follow_interval` and publish it as an
  ordinary PUSH entry (`principal = upstream`), fast-forward only; policy is not evaluated (follow is
  configuration). Its own loop, never a unit of the priority loop. `floe mirror` is the same operation from a
  host without the repo local, pushing over HTTP.
- **D39** **floe is a standalone program; any deployment is packaging.** `floe-server --config x.toml` (a thin
  bin = `floe serve`; `floe` with no subcommand serves too) works on one machine against one bucket with
  nothing in front of it. (1) **TLS in-process** — `[server.tls]`; the modes are D59's (`off | files | acme`).
  *(Superseded 2026-10-04 by D59: the original `self_signed` mode — a certificate generated under
  `<cache.dir>/tls/`, published at `/services/public/ca.pem` and pinned for git by the installer — is gone.)*
  (2) **Everything an upstream might take over is announced by the upstream, per request, in
  `X-Floe-Capabilities`** — never assumed. (3) `cache.dir` defaults to `/tmp/floe`. (4) A missing `--config`
  file is fatal (exit 2); `--config /dev/null` is the explicit defaults+env form. (5) The credential helper and
  token file are host-derived (`<host>-credential-helper`, `<host>-token`) so two floe hosts coexist on one machine.
- **D41** **Bundles: chained dailies, a clone list and a catch-up list, the chain through the weekly.** Dailies
  are cut on their predecessor (`chain = true`, default), hourlies on the newest daily (2 kept); at the tie between
  Sunday's daily and the weekly the chain continues through its own link, so Monday's daily has the weekly's tips
  as prerequisites without being cut on it; retention keeps the chain under every kept full. `bundles/list` (fulls
  + chain) is for clones, **`bundles/catchup`** (no fulls) is what recipes record in `fetch.bundleURI`. Measured: a
  catch-up is exactly the slots missed; for a client fetching several times a day, upload-pack's thin pack is
  smaller than an hourly bundle — bundles pay off for fresh clones and far-behind clients.

- **D42 (2026-09-02)** **A GitHub Enterprise Server facade, for local development only, behind
  `[github] enabled`** (`docs/GITHUB.md`). `/api/v3/*` (REST), `/api/graphql` + `/api/v3/graphql` (both
  shapes a client emits; parsed, dispatch is a later phase) and `/login/oauth/*` are answered over the same
  repositories, so an application that already supports GitHub Enterprise Server needs no code to develop
  against floe: it points its per-install `baseUrl` at `http://<host>/api/v3`. Repositories are seeded by
  a plain `git push` to the existing smart HTTP and by nothing else. **The facade is its own trust
  boundary, and it has none**: every route bypasses `server.auth`, any credential resolves to one
  hardcoded user, and every permission answer is `admin` — so `Config::validate` refuses `github.enabled`
  unless `server.auth.mode = "none"` (already loopback-only) or `server.listen` is loopback, and the
  facade is never deployed. Writes are not a second write path: `github::write` builds objects with `git`
  plumbing in a scratch object directory over the serving copy's `objects/` (a refused write leaves nothing
  behind, as with receive-pack's per-ingest scratch dir), packs exactly the new objects, and goes through
  `ingest_pack` → `publish_push_synced`, so the bucket is the truth and the manifest CAS is still the only
  commit point. The WAL does not enforce fast-forward, so the facade does it (`merge-base --is-ancestor`,
  422 without `force`); a ref that moved under a write is a 409. No events are emitted from the write path
  (principle III): `github::events` is a hook point, and a webhook must come from a WAL reader.
  **(2026-09-03)** The three phases D42 planned have landed and two of its clauses have moved on.
  GraphQL is no longer "parsed, dispatch is a later phase": `github/graphql/` dispatches on field names
  against a hand-written arm per document, with no schema or executor, and an unserved field answers
  `NOT_IMPLEMENTED` at HTTP 200. And the facade now owns durable state git cannot hold — pull requests,
  as JSON under the repository's own prefix (`github/prs/index.json` + `github/prs/<n>.json`, CAS on
  both) and one branch-protection object (`github/protection.json`). That state is the facade's alone:
  it is not WAL, not replayed, and nothing outside `github/` reads it, so it does not widen the bucket
  contract §2 defines. Everything else in D42 stands — the trust boundary, the `validate` refusal, and
  writes going through `ingest_pack` → `publish_push_synced` and no second path, merges included.
  **(2026-09-03)** The facade now emits **outbound GitHub webhooks** (`docs/GITHUB.md` §11), and the "no
  events from the write path" clause is honoured rather than dropped: `push` / `create` / `delete` (and
  `pull_request.synchronize`) are rendered by a second sink on the **events bridge** (`crate::bridge`,
  `github/webhook.rs`) from log entries read at a durable cursor, so a delivery happens iff the entry is
  durable, a non-2xx keeps the cursor, and a dead consumer costs receive-pack nothing. `[github]` gains
  `webhook_url`, `webhook_secret`, `installation_id` (default 1) and `webhook_poll_interval` (default 1 s);
  the bridge runs on the default empty `roles`, so a standalone facade needs no roles setting. Two
  concessions, both scoped to this sink: a write on the same instance calls `Bridge::wake` so the typical
  push-to-delivery latency is ~20 ms rather than a poll interval (the floe-native bus keeps exactly
  `docs/EVENTS.md`'s two wake-ups), and a dev bucket's missing notifications make a productive sweep a debug
  line instead of a warning. The one thing not from the WAL is `pull_request` itself — PR state is the
  facade's own JSON, not log entries — so those deliveries are spawned from the handlers, best effort with a
  couple of retries, and never awaited by a request.

Decision identifiers are stable; gaps in the numbering are intentional.

---

**D43 — AWS runtime credentials (2026-09-08).** The S3 backend uses the AWS SDK credential chain when
neither configured static-key variable is set. This supports refreshing ECS task roles without storing
long-lived access keys. Explicit pairs still support local object stores; partial pairs fail closed.

**D45 — Facade runtime configuration (2026-09-09).** The floe image packages
`deploy/floe.toml`; facade behavior changes ship with the image. The deployment supplies only
the store bindings (bucket, region, public origin) and secrets through `FLOE__` overrides.

**D46 — Dynamic GitHub App integrations (2026-09-09).** Replaces D42's global GitHub callback/secret/
installation config with `github/integrations.json`, a CAS'd bucket registry managed by the private
`/api/v3/_dev/integrations` API. One app callback/secret spans registered installations, each selecting
all or named repos under one owner. Reads redact secrets; the bucket contains them. WAL deliveries use
separate cursors per installation generation/repo; changing selection replays retained history, and
failure never advances another target's cursor. App JWT issuer and synthetic installation tokens select
registered identity/listing, without introducing authentication. PR events remain best effort.
`docs/GITHUB.md` §11 and `docs/ROUNDTRIPS.md` own the contract and cost model.

**D47 — Durable checkpoint history (2026-09-17).** Each checkpoint records the log segments it folds
and its preceding checkpoint. The bridge traverses that immutable index when its cursor falls behind
`min_seq`, and fails without acknowledging if indexed history is unavailable. Keep those checkpoint/log
objects; unreferenced log objects are not evidence of a committed write. This adds no publish/checkpoint
round trips and no object-store LIST.

**D48 — Follow patterns and archive-on-rewrite (2026-10-04).** `[upstream] follow` takes git refspec patterns
(one `*` crossing `/`, `^` negatives; `refs/archive/` and `refs/follow/` are never followed;
`floe_config::refpattern`). With `on_rewrite = "archive"` (default), a non-fast-forward, a tag move, or a
deletion upstream publishes **one** PUSH entry that creates `refs/archive/<unix-ts>/<original-ref>` at the old tip
(`old_oid = ""`, never overwritten) and applies upstream's state, with `meta["follow.archived"]`. The round is one
atomic transaction: any ref that moved under it rejects the whole round, which the next round re-plans. An upstream
that advertises no refs at all never causes deletions. A round probes refs first and does Serve-level work only
when something moved. `"refuse"` is D33's behaviour. Follow still bypasses policy. Upstream tokens resolve through
`Config::upstream_token_env` (`upstream.token_env_by_host`, host-only, then `token_env`). Pushers cannot write
`refs/archive/**` on any repository: policy has a built-in `archive-immutable` rule (create/update/delete, no
bypass) that a policy file replaces only by defining a rule of that name (`docs/POLICY.md`). Supersedes D33's
"fast-forward only" clause; the rest of D33 stands. Design: `docs/design/github-mirror.md` §A.

**D49 — The GitHub mirror decides, follow moves bytes (2026-10-04).** `floe-mirror` runs on a `maintain` host
(`[github_mirror] enabled`, its own loop next to follow) or as `floe github sync`, under
`leases/mirror-github.pb`; it keeps `mirror/github/state.json` (CAS, generation-guarded) and a disposable
`mirror/github/http-cache.json` (`PutMode::Overwrite`, a cache) in the bucket, maps `Owner/Name` to the plain
`owner/name` (lowercased, no prefix; owner decision 2026-10-04, R1), creates repositories by the manifest CAS
(after recording the name it is about to create, `RepoEntry.claiming`), publishes a read-only `policy.json`
(create-only) and owns only the `[upstream]` table and `repo.description` (`Mirror of <url>`, the visible
marker) of repositories marked `upstream.source = "github:<id>"`, written with `publish_settings_if` (a CAS on
the settings revision). A repository that already holds the name and is not the mirror's (an own repository) is
never adopted or overwritten: the entry is `conflict` (state, `floe github status`, an inventory row) and is
retried every pass. A human edit of that table (settings author not `github-mirror`) detaches the repository. It
never deletes a floe repository (gone/excluded repositories are frozen with `follow = []`), never stores a token
(`token_env` + `upstream.token_env_by_host`), and never transfers git objects: a push seen at the forge nudges
`ops::start(.., "follow")` on the maintaining host only. GitLab/Gitea implement `Source`. Design:
`docs/design/github-mirror.md` §B.

**D50 — Iceberg audit tables are a WAL reader, behind a feature (2026-10-04).** `floe-catalog` (`--features catalog`
on `floe-cli`/`floe-server`, which turns on `floe-catalog/iceberg`) writes `ref_events`/`force_push_log` from the WAL
in its own loop on the events host (`floe_server::catalog_tail`), outside the webhook's catch-up: a bucket
notification only `try_send`s the repository to it, and its sweep runs on `events.sweep_interval`. It has its own
cursor `repos/<o>/<r>/catalog/cursor.json` (at-least-once, dedup `(repo, seq, ref_name)`), takes
`sync_runs`/`repo_inventory` changes from lossy telemetry (`AppState::recorder`, a `NoopRecorder` when off), and
writes a durable daily `repo_inventory` snapshot under `leases/catalog-inventory.pb` (schedule in
`catalog/inventory.json`). The catalog is never a source of truth and never holds git objects; its outage only adds
catalog lag. Git, sync, follow and the mirror never await it. `catalog.enabled` in a binary built without the
feature is a fatal startup error. Design: `docs/design/github-mirror.md` §C.

**D51 (reserved, 2026-10-04) — Push to an upstream is a WAL reader on the maintaining host.** Not built. Reserved
so the rules are on record first: a ref is either followed or pushed, never both (`validate` refuses an overlap of
`upstream.follow` and `upstream.push`); entries with `principal = upstream` are never pushed; a push is idempotent
(`ls-remote` first), never forced unless `push_force`, and `refs/archive/*` is never pushed. Seam:
`floe_server::push_back`, a per-repo cursor `repos/<o>/<r>/push/<remote>.json`. Design:
`docs/design/github-mirror.md` §E.

**D59 — TLS: off, provided files, or ACME DNS-01; no self-signed (2026-10-04).** Supersedes D39 (1).
`[server.tls] mode` is exactly one of: **`off`** (plain HTTP/1.1 + h2c: loopback development or behind an edge
that terminates TLS, D23), **`files`** (operator chain + key; reloaded on mtime/size change, polled every 5 s, or
`SIGHUP`; a bad pair keeps the old certificate), **`acme`** (RFC 8555 through **DNS-01 only** — HTTP-01 and
TLS-ALPN-01 would need every instance behind the name to answer the token, DNS-01 needs one writer and covers
wildcards; Cloudflare first, behind `floe_tls::dns::DnsProvider` for Route 53 / RFC 2136). The self-signed mode,
`/services/public/ca.pem` and the installer's CA pinning are removed: a client either sees a certificate from a CA
it trusts or plain HTTP. TLS stays **bootstrap config** (file + `FLOE__` env), never a bucket-stored config
document — the listener needs it before anything else. **ACME state is in the bucket** (principle I; §2.1 rows):
account and certificate under `tls/acme/`, private keys sealed with AES-256-GCM (`ring`, already in the tree;
random 96-bit nonces are safe at a few seals per renewal and need no nonce state) under a 32-byte key from
`storage_key_env`, bound to their object key as associated data. **One orderer**: the instance that wins
`leases/tls-acme-<set>.pb` (the existing CAS lease, heartbeat while ordering) orders or renews; everyone
revalidates `cert.json` by conditional GET every `poll_interval` (10 min; 10 s while nothing is loaded) and swaps
the new certificate into a rustls `ResolvesServerCert` — new handshakes get it, established connections keep
theirs, no restart. A `cert.json` whose recorded domains or directory differ from the config, or whose leaf SANs do
not cover every configured name (wildcard-aware), is refused: the current certificate stays, the refusal is logged
and counted as a failure. The Cloudflare token goes only to `https://`, or plain `http://` to exactly
`127.0.0.1`/`::1`/`localhost` (a mock). An issued certificate whose `cert.json` write fails (bucket outage) is served from
memory and only the write is retried with backoff — never a re-order, which would spend Let's Encrypt's
5-duplicates-a-week budget. `email`, `domains` and the env var names are trimmed once at parse (`Config::normalize`). The first order is a `tls-acme` task (D13) and `/readyz` is 503 until a certificate is loaded.
Propagation is checked against the zone's authoritative nameservers (or `resolvers`) with a timeout before the CA
is asked; challenge records are always deleted. Renewal starts `renew_before` ahead of expiry but never before two thirds
of the certificate's lifetime (a 60-day `renew_before` against a 6- or 45-day profile would otherwise make every
fresh certificate due and re-order every poll), and no order starts within 12 h of a successful one while a
certificate is loaded. Failures back off 5 min × 2ⁿ up to 6 h (≥ 1 h after
`rateLimited`), recorded in `tls/acme/<set>/status.json`, which every instance revalidates with `cert.json`, so
restarts and other instances honour it and show the same state; inside
`renew_before` a failing renewal logs a warning every pass. Crates: `instant-acme` (maintained, RFC 8555, used
over our reqwest), `hickory-resolver` (propagation), `x509-parser` (expiry/issuer). Surfaces: `GET /api/v1/tls`
(admin: domains, expiry, issuer, last renewal and error — the admin UI's overview), `instance.tls` on `/readyz`
and `/services/api/instance` (public facts only), `floe_tls_cert_not_after_seconds` and
`floe_tls_acme_orders_total{ok}`. Tests: config validation, seal round trip, resolver swap (with a real
handshake), file reload, a mocked Cloudflare API, and the whole flow against Pebble + pebble-challtestsrv in CI
(`acme-pebble`, both arches). Not done: key rotation for `storage_key_env` (rotating it means deleting `tls/acme/`
and ordering afresh), ARI (`renewalInfo`), EAB, providers other than Cloudflare.

**D63 — The Iceberg catalog client signs SigV4 itself (2026-10-04).** `[catalog] auth = "none" | "bearer" |
"sigv4"` (with `sigv4_service`, default `"s3"` = RustFS `/iceberg`, `"s3tables"` = AWS; `sigv4_region`, default
`s3_region`), validated fail closed. Because `iceberg-catalog-rest` 0.10.1 has no request hook, `auth = "sigv4"`
puts an in-process signing proxy (`floe_catalog::sigv4::SigningProxy`) between `RestCatalog` and the endpoint: an
axum server on a Unix socket in a `0700` temp directory, reached only through the reqwest client handed to
`RestCatalogBuilder::with_client`; it accepts only the configured host, signs with `aws-sigv4`
(`x-amz-content-sha256`, `X-Amz-Date`, `X-Amz-Security-Token`) and forwards. No TCP listener, no state (the socket
dies with the connection, so principle I holds). Credentials follow D43 (static env pair, else the AWS SDK chain,
refreshed before expiry) and the same source is the data-file `FileIO`'s only credential provider. The module is
self-contained for the shared Iceberg crate the code-intelligence design plans; when iceberg-rust's `AuthManager`
(0.11) lands, it replaces the proxy behind the same config. Design: `docs/design/github-mirror.md` §C.9.

**D60 — Bootstrap file, versioned config document (2026-10-04).** `floe.toml` + `FLOE__` env keep only what an
instance needs to start and to reach the config store (store, `[config_store]`, server/auth/TLS, cache, wal, git,
lfs, telemetry, maintenance, placement, compaction, bundles, upstream, the `[github]` facade). Runtime configuration —
`github_mirror`, `catalog`, `events` (`floe_config::RuntimeConfig`) — is one JSON document at
`<config_store.prefix>current.json` in `[config_store] bucket` (default: `config/` in the main bucket), written by CAS
with a monotonic revision; `history/<rev>.json` records every revision (author, message, timestamp, diff) and points
at the bucket's object version when the bucket is versioned (`history = auto|records|versions`). One home per key:
the file refuses runtime sections and `FLOE__` overrides of them are ignored; the document cannot carry bootstrap
keys. Publish validates with the same code as startup and fails closed (400, nothing written); rollback publishes
an old revision as a new one. Every instance revalidates with a conditional GET every `config_store.ttl`, off every
request path; the mirror applies changes live at its next pass boundary, `events`/`catalog`/`github_mirror.git_url`
report *restart required*. A config store outage never blocks git: an instance starts with the built-in runtime
defaults and keeps revalidating. Design: `docs/design/admin-ui.md`. Supersedes D49's "never stores a token" for
sealed tokens (D61); the rest of D49 stands.

**D61 — Secrets in the config document are write-only and sealed (2026-10-04).** A secret field is
`{env = "NAME"}` (read on every instance; nothing in the bucket), or `{sealed = "v1.<kid>.<b64>"}` — AES-256-GCM
(`ring`) under the 32-byte key in `FLOE_CONFIG_KEY` (`config_store.key_env`), the field path as associated data.
`{value = …}` is input only (sealed before writing; 400 without a key), `{redacted = true}` is what reads return and
means "keep" on write (D46's redaction rule). Resolution goes through one process-wide alias per secret field
(`floe_config::secret::env_var`), refreshed on every apply and read at every use (a token entered later reaches
follow/LFS without a restart; unresolved = anonymous). **Env names are host facts**: every `{env}` and `*_env` in the
document must be allowed by the bootstrap `[config_store] allowed_env` (default `FLOE_SECRET_*` + the documented
names; `key_env`, `FLOE__*`, `AWS_*` and the store/auth variables only when listed exactly; the catalog's SigV4
signing-key fields may also name the AWS defaults, because SigV4 sends only the key id, a host-bound signature and
a session token that is useless without the secret), checked in
`Config::with_runtime`, i.e. at publish (400) and at every apply. Test endpoints never send a caller-named env var,
send the stored credential only to the applied URL, follow no redirect and echo no body (`docs/design/admin-ui.md`
§5.3).

**D62 — The admin API and the SPA's admin area (2026-10-04).** `/api/v1/admin/*` (and `/api-browser/v1/admin/*`):
config get/put/validate/schema/history/revisions/rollback, overview, mirror status/preview/test/sync/pause/resume,
catalog status/test. Every route, reads included, needs an admin principal (D24's rule); `GET /api/v1/me` reports
`admin`. Every publish is audited (history record + log line + a lossy `sync_runs` row `kind = "config"` with the
catalog on). Per-repo pause is a config change (exact `owner/name` in `github_mirror.exclude`, the mirror's frozen
state). `repos.js` maps it as `repos.admin.*` (dogfood rule); the bundled UI's admin area lives at `/_admin` and
renders the runtime sections from `GET …/config/schema`. TLS is never editable there (D59): the overview shows
`GET /api/v1/tls` read-only.

**D52 — Code intelligence is a derived index, in scope as a feature-gated capability (2026-10-04).** Git is
the content truth; the `code.*` Iceberg tables (append-only, keyed by blob sha / chunk hash, format v2) are the
durable truth of extracted facts and embeddings; everything served is an immutable, content-addressed artifact
(`codeintel/shards`, `codeintel/vec`, `codeintel/dir`) made visible by a per-repo CAS'd
`repos/<o>/<r>/codeintel/head.pb`. `head.pb`, `codeintel/dir/head.pb`, `codeintel/tasks/*.json` and
`codeintel/tables.json` join principle II's Overwrite list; per-generation `commits/<commit>/<generation>.pb`
records are immutable, and artifact liveness follows generation retirement, never write time. GOAL §4 gains
"agent-facing code navigation and search over hosted repositories, as derived, rebuildable artifacts". Everything
is behind cargo features (`codeintel`, `mcp`), off by default; the default build is unchanged, and a
`[codeintel]`/`[mcp]` key that needs a feature the binary lacks is a fatal config error naming the build flag.
`[codeintel]` and `[mcp]` are runtime sections of the config document (D60, restart-only); `[access]` is a
bootstrap host default overridable per repository (D24).
The extraction core is the pure library `crates/floe-codeintel` (bytes in, records out: no store, no tokio, no
axum). Design of record: `docs/design/code-intelligence.md`.

**D53 — Indexing is a maintainer unit, not a write step (2026-10-04).** A reader of the WAL's ref state whose
work set is the diff between the D22 desired state (tracked tips, current extractor, intact artifacts, empty
requests, catalog caught up) and `head.pb`, the same function the planner uses; placed by D30, one lease per repo;
no code in receive, publish or follow. Embedding is a separate unit with its own lease and rate limit.

**D54 — Per-repo read authorization is one function (2026-10-04).** `[access] read` per-repo settings (a D24
extension, with a per-repo subset of `[codeintel]`) evaluated by `policy::authorize_read`, shared by git, the web
API and MCP; MCP is never more permissive than `git clone`; cross-repo retrieval applies the readable-repo mask
inside postings and vector scans, never after top-k. Until `authorize_read` ships (M5), `Config::validate` accepts
only the default `read = ["authenticated"]`: a rule nothing enforces is refused, not silently ignored.

**D55 — `/api/v1/mcp` and `/{o}/{r}/mcp` are stateless MCP 2026-07-28 endpoints; floe is an OAuth resource
server (2026-10-04).** The global endpoint lives under D15's non-repository prefix so it shadows no owner; both
are exact routes of one service, and the per-repo route passes its validated repo to the handler as a request
extension (rmcp hands the request's `http::request::Parts` to the handler in `RequestContext.extensions`; verified
in the M0 spike, `docs/design/spikes/rmcp-request-parts.md`). rmcp pinned exactly; PRM at the well-known paths,
the per-repo endpoint advertising and accepting its own resource; audience-bound IdP access tokens, `wgt_` tokens
of the HMAC kind `mcp` (refused on git and web), and scoped static tokens; ID tokens refused. The edge may route
`/api/v1/mcp` by `Mcp-Param-Repo` (from `x-mcp-header: "Repo"` on the root-level `repo` property) and by nothing
else; routing is an optimisation, never a correctness dependency.

**D56 — Freshness before durability, for deterministic facts only (2026-10-04).** A nav shard may be served
before its Iceberg rows commit, because it is a pure function of git and the extractor version; the rows are then
owed, tracked per ref as `catalog_commit`, and re-derived from the published shards until the marker commits;
embeddings must commit to Iceberg before any artifact derived from them is published.

**D57 — Agent state is explicit, signed and re-authorized (2026-10-04).** The snapshot handle pins (repo,
commit, generation, purge epoch) with an HMAC under a key derived from `server.auth.mcp_handle_secret`
(a bootstrap key, so never in the config document; default `server.auth.session_secret`; one of the two, ≥ 32
bytes, is required when MCP is on), lives 24 h, and is
re-checked against `authorize_read` on every call; continuation cursors are HMAC'd positions; there are no MCP
sessions; MCP tasks live in the bucket and are durable before `CreateTaskResult` is returned (a `reindex` task is
durable as a record and as a queued `head.pb` request; queued tasks are judged by their request, running ones by
heartbeat).

**D58 — SQL visibility is by marker rows (2026-10-04).** `code.blobs` then `code.commits_indexed` are committed
last; the latter records each table's snapshot id, its generation and its purge epoch; purges hide earlier epochs
only; `views.sql` (shipped, tested against DuckDB) gives exactly-once, commit-consistent reads; catalog
credentials are an admin privilege, one table bucket per tenant.

**D64 — The Helm chart is a thin client of the `common` library chart (2026-10-06).** `deploy/helm/floe` renders
every Kubernetes object through `common` (`oci://ghcr.io/technicaldomain/helm`, the owner's library): each template
is one `common.*` call with an empty override, `values.yaml` is the library's values plus `floeToml`, the bootstrap
file (runtime sections stay in the config document, D60), and secrets arrive as env vars from Secrets. What the
library cannot express is added to the library (2.5.0: PodDisruptionBudget, topology spread, pod-spec fields, probe
scheme and per-probe kind, ConfigMap and PVC mounts, monitor endpoint fields), not re-implemented in the chart. One
release is one Deployment: a separate `maintain` host (D9/D30) is a second release of the same chart against the
same bucket. Published by the release workflow to `oci://ghcr.io/kharkevich-engineering-lab/charts/floe` with
chart version = `appVersion` = the release (set at packaging, never committed); CI lints, renders every `ci/`
values file through kubeconform and installs the dev values into kind against RustFS.

**D65 — The web UI wears the Kharkevich Engineering Lab design system (2026-10-08).** The SPA's tokens (colour,
type, radii, shadow), theme key (`kharkevich-theme`, dark default, `html.light`), self-hosted Inter and the Lab mark
come from `hub.kharkevich.com` (`styles/main.css`, brand page) — copied, never re-invented; product name **floe**.
Shared primitives live in `web/src/components/ui.tsx`; pages use tokens only. The admin area leads every section
with its status and primary action, groups settings into steps, folds Advanced and shows conditional fields from
schema hints the server emits (`x-floe.advanced`, `order`, `when`; section `description` and `groups`) — the hints
are presentation only and change no API shape. Help text in the schema is plain language; config keys and decision
numbers stay available behind a key toggle, never as the label. Details: `web/README.md` § Design system.

## 5. Working rules

- **No backwards compatibility (pre-1.0, banner at top):** change the shape and delete the old one in the same
  commit — no aliases, shims, deprecated routes/keys/fields. Only bucket formats (WAL, manifest, log, checkpoint
  protos) stay append-only/replayable.
- Keep this file current: append decisions with a number and a date; never delete history, replace it with the
  decision that superseded it.
- **Never block the async runtime**: no blocking git/fs work (repack, midx, commit-graph, gix reopen, large
  reads/copies) on a tokio worker — `spawn_blocking`; every `refresh()` on an async path is `refresh_async()`. Pack
  materialization runs on the **bulk runtime** (`sync::on_bulk_runtime`). The runtime watchdog logs "async runtime
  stalled" with `inflight` and `tasks_running`: `inflight = 0` at a late tick ⇒ the platform paused the process,
  `inflight > 0` ⇒ a real stall — look at `lock_wait_max_ms` and `floe_lock_wait_seconds{lock}`. Bulk bytes never
  share a transport with the control plane (`store.gcs.bulk_clients`, `bulk_concurrency`).
- **Correct is not sufficient.** Every protocol change (publish, sync, leases, checkpoints, bundles) is also
  judged on critical-path round trips against the bucket — read `docs/ROUNDTRIPS.md`, update its budget table, put
  before/after depth in the commit, keep verification on the failure path, assert request budgets in the sim.
- **Standalone first (D39):** a feature must work with floe hit directly (no edge, in-process TLS, bytes
  streamed by floe). Anything an edge takes over is announced per request in `X-Floe-Capabilities`; never
  infer an edge from config, never hardcode a hostname in `crates/` or `web/`.
- **S3 and GCS are both first class.** Every store feature has both implementations and runs in the contract
  suite (`just test-s3` against rustfs, `just test-gcs <bucket>`); "GCS only" is a bug.
- **Use the rig before prod** (`just dev-store` → `floe-server --config floe.standalone.toml`). Per-repo
  settings (D24) with minute-scale slots compress a week of bundle behaviour into 30 minutes.
- No new auth paths (§1.3). No LIST on hot paths. No unbounded buffering of packs in memory. No full
  materialization above `cache.max_bytes` in budget mode. No silent long operations (make it a task, narrate it).
- Every immutable response: `immutable` + strong ETag + Range; every ref-dependent response: SWR + ETag.
- Before changing the wire/store formats: proto is append-only; manifests/log entries must stay replayable by
  old readers within the retention window.
- Web: pnpm + Vite, `pnpm run build` must pass oxlint/tsc. Config: `floe.example.toml` documents every key;
  change it with the code.
- Test tiers: `just test` (fast, < 1 min; includes D48 `--test follow` and `--test policy`), `just e2e`, `just fmt-check`, `just warnings`,
  `just clippy` (the `[workspace.lints]` set, `-D warnings`), `just clippy-catalog` + `just test-catalog-lib`
  (the `--features catalog` build the release ships), `just clippy-codeintel` + `just test-codeintel` (the `--features mcp` build, D52), `just ci` = all of
  them, including the **simulation
  suite** `just sim` = `cargo test -p floe-server --test sim` (fault links per instance over one truth store: crash,
  partition, stale, lost response, orphan scenarios + randomized seeds `FLOE_SIM_SEEDS`/`FLOE_SIM_SEED`);
  `just test-slow` (ignored benches); `tests/e2e.sh` against a running server (`FLOE_E2E_BASE_URL`,
  `FLOE_TOKEN`). Never `cargo test --workspace --no-fail-fast` in a session; wrap ad-hoc cargo in `timeout`.
- Known flaky (find the cause, not the assertion): `fetch_from_front_that_serves_the_base_remotely` (~1 in 3
  under the full e2e suite: base published without `has_commit_graph`) and
  `sim::base_rebuild_resumes_after_a_kill_between_any_two_phases` (~1 in 4–7; fails even with `just sim`'s
  one-test-at-a-time, "copying the serving copy to the scratch dir: No such file", #14), and
  `sim::sim_cache_pressure_keeps_pinned_repos_and_refuses_too_large` (a cold refs read > 1 s on CI runners, #12).
  The PR gate skips the two sim tests; the nightly runs them.
- **CI runs what a change needs** (`.github/workflows/ci.yml`; a `changes` job reads the PR's file list). Push
  to `main` and a manual run run everything. On a pull request:

  | Job | Runs when the PR changes |
  |---|---|
  | warnings + test, Git/editor contracts + `just sim` (both arches) | anything but docs (`docs/**`, `**/*.md`, `site/**`, `LICENSE`, `NOTICE`) |
  | catalog feature (both arches), rustfmt, MSRV (`just msrv`, x86_64) | `crates/**`, `Cargo.*`, `rust-toolchain.toml`, `.cargo/**`, `clippy.toml`, `justfile`, `.github/workflows/**` |
  | ACME against Pebble (both arches) | `crates/floe-{tls,store,config,proto}/**`, `Cargo.*`, `rust-toolchain.toml`, `.cargo/**`, `.github/workflows/**` |
  | release container: amd64 | what the Containerfile copies: `crates/**`, `Cargo.*`, `rust-toolchain.toml`, `web/**` (not `*.md`), `deploy/**`, plus `Containerfile`, `.dockerignore`, `compose.yaml`, `scripts/**`, `.github/workflows/**` |
  | release container: + arm64 | the build recipe: `Containerfile`, `.dockerignore`, `compose.yaml`, `deploy/**`, `Cargo.lock`, `rust-toolchain.toml`, `web/package.json`, `web/pnpm-lock.yaml`, `scripts/**`, `.github/workflows/**` |
  | cargo-deny (`deny.toml`) | `Cargo.lock`, `deny.toml` |
  | actionlint | `.github/**` |
  | Helm chart: lint, kubeconform, kind install (x86_64) | `deploy/helm/**`, `.github/workflows/**` (and `deploy/helm/**` alone runs nothing else) |

  `CI result` aggregates them: green when every job succeeded or was skipped by its filter, and is the one CI
  check branch protection requires. **`main` is protected**: a merge needs `CI result` and `Conventional commit
  title validation` green on a branch up to date with `main`; no review is required (the owner merges their own
  PRs), admins can bypass, force pushes and deletion are refused. (`CI result` joins the required checks once
  this workflow is on `main`: `echo '{"strict":true,"checks":[{"context":"CI result","app_id":15368},{"context":"Conventional commit title validation","app_id":15368}]}' | gh api -X PATCH repos/kharkevich-engineering-lab/floe/branches/main/protection/required_status_checks --input -`.) Rust and BuildKit
  caches are written from `main` only (a PR push saving ~5 GB evicted the next run's cache from the 10 GB
  budget). The **nightly** workflow (03:17 and 15:47 UTC) runs `just test-slow`, the sim with random seeds and
  `just deny`, and opens or refreshes one `nightly` issue when it fails. The MSRV is `rust-version` in
  `Cargo.toml` (1.94.1, the floor of the locked aws-sdk graph); the toolchain pin is `rust-toolchain.toml`.
