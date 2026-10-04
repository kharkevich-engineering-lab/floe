# Admin GUI and the versioned config store

Context: **the design of record for moving floe's runtime configuration out of `floe.toml` into a versioned
document in the bucket, edited from an admin area of the bundled React SPA.** Read `GOAL.md`, `AGENTS.md` (§3
principle I, D8, D12, D15/D20, D24, D46, D49/D50) and `docs/design/github-mirror.md` first. Where this document
and the code disagree after landing, the code wins and this file gets a dated note.

Status: **proposed** (2026-10-04). Decisions D60–D62 are added to `AGENTS.md` §4 with it.

The owner's direction: *"most of the configuration (including mirroring) happens in the executable config file,
but I want a GUI to configure it, like a normal React app"*, and *"we can store config in a dedicated bucket with
versioning (or without)"*.

---

## 0. Summary

| # | What | Where |
|---|---|---|
| 1 | `floe.toml` + `FLOE__` env shrink to **bootstrap**: what an instance needs to start and to reach the config store | §1, `floe-config` |
| 2 | Runtime configuration (`[github_mirror]`, `[catalog]`, `[events]`) is one JSON **config document** in a dedicated location: `[config_store] bucket`, default `config/` in the main bucket | §1, §2 |
| 3 | Every change is a CAS with a monotonic revision; history and rollback work with and without bucket versioning | §2 |
| 4 | Every instance revalidates with a conditional GET every `config_store.ttl`; the mirror applies changes live, the rest reports *restart required* | §4 |
| 5 | Secrets (GitHub token, webhook secret) are write-only in the GUI, redacted on read, sealed at rest with `FLOE_CONFIG_KEY`; env references stay | §5 |
| 6 | `/api/v1/admin/*` (admin principal only), mapped by `repos.js` (`repos.admin.*`) | §6, §7 |
| 7 | An admin area in the SPA at `/_admin`: overview, mirroring, catalog, per-repo settings/policy, config history | §8 |
| 8 | `floe config show|set|history|rollback|import|validate`; the moved keys are deleted from the file schema | §9 |

The one idea: **the bucket is the repository, and now also the configuration.** A wiped instance loses nothing
but warmth; it reads the document again (principle I). The file keeps only what the instance needs before it can
read the bucket.

---

## 1. The split: bootstrap file vs. config document

### 1.1 Bootstrap (stays in `floe.toml` + `FLOE__` env, D8)

Everything a process needs **before** it can read the config store, and everything that is a fact about **this
host** rather than about the fleet:

| Section | Why it stays |
|---|---|
| `[store]` (+ `s3`, `gcs`) | How to reach the bucket (and therefore the config store) |
| `[config_store]` (new) | Where the document lives, revalidation TTL, history mode, the key's env var |
| `[server]` incl. `[server.auth]`, `[server.tls]` | Listen/TLS/auth must be right before the first request; a document that could lock every admin out (auth) or take the listener down (TLS) is not something a GUI should be able to publish. TLS (being reworked to `off`/`files`/`acme` in a separate change) stays here; the admin overview only *shows* its status. |
| `[cache]`, `[wal]`, `[git]`, `[lfs]`, `[telemetry]` | Process shape: disk budget, runtimes, binaries, logging |
| `[maintenance]`, `[placement]` | Host facts (D30: a fleet-wide document cannot say "this host"; the env-group rule for `[placement]` exists because a shared value once excluded a host's own repository) |
| `[compaction]`, `[bundles]`, `[upstream]` | Host defaults; per repository they are already GUI-editable through D24 settings (page 4) |
| `[github]` | The dev-only facade; its validity is tied to `server.auth.mode` (D42) |

### 1.2 Runtime (moves to the config document)

| Section | Keys | Live? (§4.2) |
|---|---|---|
| `github_mirror` | every key of D49 (`enabled`, `api_url`, `git_url`, sources `users`/`orgs`/`starred`/`repos`, `include`/`exclude`, skips, `include_private` + acknowledgement, `lfs`, `follow`/`on_rewrite`/`follow_interval`, `read_only`, limits, `interval`, `gone_after`, `lease_ttl`) and the credential **`token`** (a secret, §5; replaces `token_env`) | live, except `git_url` |
| `catalog` | every key of D50 (connection, warehouse/namespace, the auth env references, flush tuning, backfill) | restart |
| `events` | `webhook_url`, `webhook_secret` (a secret, §5), `sweep_interval` | restart |

Later sections (`codeintel`, `mcp`, fleet-wide `[repo_defaults]` layered under per-repo settings) join the document
the same way: a struct in `floe_config::RuntimeConfig`, a row in the live table, nothing else.

**Not moved, on purpose:** follow/maintenance defaults and placement. Their per-repository form already lives in
the WAL (D24) and is edited on page 4; their host form is a host fact. A fleet-wide defaults layer
(`[repo_defaults]`, merged between the host file and the repository's settings) is the natural next section and is
listed under §12.

### 1.3 The file schema

`Config::parse` refuses a file that contains `[github_mirror]`, `[catalog]` or `[events]`:

```
floe.toml: [github_mirror] is runtime configuration and lives in the config store (D60);
publish it with `floe config import floe.toml` (or the admin GUI) and delete the section
```

`FLOE__GITHUB_MIRROR__*`, `FLOE__CATALOG__*`, `FLOE__EVENTS__*` are ignored with a WARN like any key unknown to
the file (and reported by `floe config check --strict`, exit 3). No alias, no fallback (pre-1.0 rule).

The Rust structs (`GithubMirrorConfig`, `CatalogConfig`, `EventsConfig`) stay where they are and keep their
validation; they move from "sections of the file" to "sections of `RuntimeConfig`", and `Config` keeps fields of
those types as the **effective** values (§3) so every consumer (`floe-mirror`, the bridge, the catalog writer,
`Config::upstream_token_env`) reads them exactly as before.

### 1.4 `[config_store]`

```toml
[config_store]
# bucket = "floe-config"     # a dedicated bucket (same backend and credentials as [store]); unset = the main bucket
prefix = "config/"           # key prefix inside that bucket
ttl = "15s"                  # revalidation period (one conditional GET); 0 = only at startup
history = "auto"             # auto | records | versions (§2.3)
key_env = "FLOE_CONFIG_KEY"  # env var holding the 32-byte base64 sealing key (§5)
```

A dedicated bucket lets an operator give the config its own IAM (who may read sealed secrets, who may write) and
its own versioning/retention policy without touching the repository bucket.

---

## 2. The config store

### 2.1 Objects (under `<prefix>`)

| Key | Written | Role |
|---|---|---|
| `current.json` | CAS (`Create` for revision 1, `Update(version)` after) | The **commit point**: `{revision, updated_at, author, message, rolled_back_from?, document, diff}` |
| `history/<revision:020>.json` | `Create`, immutable | The record of every committed revision (§2.3) |
| `instances/<instance>.json` | `Overwrite` (a heartbeat, principle II's allowed list) | Which instances run which revision, restart-required keys, the last apply error (§4.4) |

`mirror/github/sync-request.json` (bucket root of the main store, `Overwrite`) carries the GUI's "sync now" (§8).

### 2.2 Publish (CAS, monotonic revision)

1. GET `current.json` → `(version, record)` or absent (revision 0).
2. A caller that names `base_revision` (the GUI always does; it is the `ETag` of `GET …/config`) gets **409** when
   it differs — the editor shows the newer revision and a diff, nothing is merged silently. The CLI without
   `--base` retries on a lost race.
3. **Heal history**: make sure `history/<record.revision>` exists (a `Create`; "already exists" is success). This is
   what makes step 6's best-effort write sufficient: a crash between the CAS and the history write is repaired by
   the next publish.
4. Resolve and validate the new document (§3.2, §5.2): fail closed, **400** with `errors[]`, nothing written.
5. CAS `current.json` with `revision + 1`. A precondition failure is a lost race → 409 (or retry, step 2).
6. Write `history/<revision+1>` (best effort).

Round trips: 1 GET + 1 conditional PUT on the commit path, +2 `Create`s for history. An admin-rate path; nothing
here is on a git path.

### 2.3 History with and without bucket versioning

`history = "auto"` asks the store once at startup (`ObjectStore::object_versioning()`: S3
`GetBucketVersioning`, GCS bucket `versioning.enabled`, the memory store's switch):

- **`versions`** (bucket versioning on): `history/<rev>.json` is a small *metadata* record — revision, author,
  message, timestamp, diff — plus `object_version`, the version of `current.json` that revision wrote. Reading or
  rolling back revision *n* fetches that object version (`ObjectStore::get_version`: GCS generation, S3 version id
  resolved from the ETag by `ListObjectVersions`). The bucket keeps the bodies; floe keeps the index.
- **`records`** (no versioning, or forced): `history/<rev>.json` carries the whole record, document included. floe
  owns the history.

Either way the history *index* is floe's, because author, message and diff are not something object versions carry,
and because listing is then `GET history/<rev>` for `rev = current … current-n+1` — no LIST. `history = "versions"`
on a store without versioning is a startup error (fail closed); `auto` falls back to `records`.

Retention: in `records` mode history is kept forever (a document is a few KiB). In `versions` mode the bucket's
lifecycle rules decide; a revision whose object version was expired answers **410 Gone** on read/rollback.

### 2.4 Rollback

`POST …/config/rollback {revision, base_revision, message}` publishes revision *n*'s document as a **new** revision
(`rolled_back_from = n`). Nothing is rewritten; history only grows. Sealed values are carried as they are; if the
sealing key changed since, the rollback fails validation with "re-enter the secret" (§5.3).

### 2.5 Diff

Every record carries `diff`: the flattened JSON paths that changed between the previous and the new **redacted**
document (`{path, op: added|removed|changed, old, new}`). A secret appears as `"(secret)"` on both sides, so a
rotated token shows as *changed* without leaking either value.

---

## 3. Precedence

### 3.1 The rule: one home per key

| Key class | Source, in order (later wins) |
|---|---|
| Bootstrap | built-in default → `floe.toml` → `FLOE__` env |
| Runtime | built-in default → **config document** |
| Per repository (D24) | effective host config (both rows above) → repository settings in the WAL |

The file and the env **cannot set a runtime key at all** (§1.3), and the document cannot set a bootstrap key
(`deny_unknown_fields` on `RuntimeConfig`). There is no override ladder to reason about, and the GUI never shows a
value that is not the one running. We considered "env > file > store" (an operator can always pin) and rejected it:
a pinned key makes the GUI lie, and the one thing an operator legitimately needs to keep out of the bucket — a
secret — has its own mechanism (env references, §5.1).

### 3.2 Validation on publish (fail closed)

A document is published only when **all** of these pass, else **400** with every error and nothing written:

1. It parses as `RuntimeConfig` (`deny_unknown_fields`, typed durations/sizes, enums).
2. Every secret resolves: a `value` can be sealed (the key is configured), a `redacted` placeholder has a stored
   value to keep, a `sealed` value opens with this instance's key.
3. `RuntimeConfig::validate` — the same checks as before the move (`GithubMirrorConfig::check`,
   `CatalogConfig::validate`, the `events.webhook_url` scheme), minus the host-role rule (the document is
   fleet-wide; the mirror simply runs on `maintain` hosts).
4. The effective config (this instance's bootstrap ⊕ the document) passes `Config::validate`.

`POST …/config/validate` runs the same pipeline without writing and also returns the diff against the current
revision and the keys that would need a restart — the GUI calls it as you type.

Errors carry a `path` when the message names one (`github_mirror.include` …), so the form puts them next to the
field.

---

## 4. Propagation

### 4.1 Revalidation

At startup `AppState::new` reads `current.json` once (bounded: 3 s) and builds the effective config from it
before anything else — the bridge and the catalog writer see the document's values from the first second. If the
store cannot answer in time the instance **starts anyway** with the built-in runtime defaults (mirror off, no
webhook, no catalog) and records the error: a config outage never blocks git.

A background task then revalidates every `config_store.ttl` with a **conditional GET** of `current.json` (304 ≈
15 ms, the manifest's cost model, `docs/ROUNDTRIPS.md`). Not on any request path. A store error keeps the last
applied document and is reported on the overview.

### 4.2 Apply: live where safe, "restart required" elsewhere

A new revision is unsealed with this instance's key, merged over the bootstrap, validated, and published on a
`tokio::sync::watch` channel. If any step fails the instance keeps the previous document and reports
`apply_error` (e.g. "cannot unseal github_mirror.token: FLOE_CONFIG_KEY is not set on this instance").

| Consumer | Live? | How |
|---|---|---|
| GitHub mirror loop | **yes** | The supervisor (`floe_server::mirror::run_loop`) watches the channel; a change to `github_mirror` stops the loop at its next pass boundary (`floe_mirror::run_loop_until`), releases the lease and restarts it with the new section. Enabling/disabling works the same way. A pass in flight is never interrupted. |
| GitHub token (mirror, follow and LFS read-through on mirror repositories) | **yes** | Resolved through one alias (`FLOE_CONFIG_GITHUB_MIRROR_TOKEN`) in `floe_config::secret`, updated on apply |
| `github_mirror.git_url` | restart | Follow scopes the token to that host from the registry's config |
| `events.*` | restart | The bridge's sink set is built once per process |
| `catalog.*` | restart | The Iceberg writer and the catalog tail are built once per process |

"Restart required" is computed per instance as the set of restart-only paths whose value differs from the
document the process started with; it is shown on the overview per instance and returned by PUT/validate.

### 4.3 What never happens

- No request waits for the config store.
- No git path reads `current.json`.
- An invalid document never reaches an instance (validated at publish) — and if one did (written by hand into the
  bucket), every instance would refuse it and keep running.

### 4.4 Instances

Each instance writes `instances/<id>.json` (`Overwrite`) after every apply and at most every minute: instance id,
version, roles, applied revision, restart-required keys, apply error, `seen_at`. The overview lists them
(a LIST on an admin page, never a hot path, principle VII); entries older than a day are deleted by the lister.

---

## 5. Secrets

### 5.1 Shape

A secret field (`github_mirror.token`, `events.webhook_secret`) is one of:

| Form | Stored? | Meaning |
|---|---|---|
| `{"env": "FLOE_GITHUB_TOKEN"}` | as is | Read from that variable on every instance. Keeps tokens out of the bucket entirely. The default for `github_mirror.token`. |
| `{"value": "ghp_…"}` | **never** | Input only: the server seals it before writing |
| `{"sealed": "v1.<kid>.<b64>"}` | yes | AES-256-GCM ciphertext |
| `{"redacted": true}` | never | Input only: "keep the stored value" (what a GET returns for a sealed value) |

GET always returns `{"redacted": true}` (plus `"kind": "sealed"`) for a sealed value, so an admin can see a secret
is set and replace it, never read it back (D46's rule). Env references are not secret and are shown.

### 5.2 Sealing at rest

`FLOE_CONFIG_KEY` (the name is `config_store.key_env`) holds 32 random bytes, base64 (`openssl rand -base64 32`),
on every instance and on any host running `floe config` with secrets. Sealing is AES-256-GCM (`ring`, already in the
dependency graph through rustls), a random 96-bit nonce per value, the **field path as associated data** (a sealed
token cannot be moved into another field), and a key id (`kid` = first 8 hex of sha256 of the key) so a wrong key
fails with a clear message instead of a decryption error.

Why encrypt instead of relying on bucket IAM: the config bucket is readable by every instance and by whoever
operates it; a sealed value additionally needs the key from the instance's environment, i.e. the same trust as an
env-var token today. Why not KMS: floe runs against S3, GCS and a laptop's rustfs; a KMS per cloud is a dependency
per cloud. An operator who wants KMS keeps the token in env (`{"env": …}`) populated by their secret manager.

Without a key, publishing a `value` is **400** ("set FLOE_CONFIG_KEY or use an env reference"). Never stored in
plain text.

### 5.3 Rotation

Rotating `FLOE_CONFIG_KEY` means re-entering sealed secrets (the GUI shows them as "cannot be opened with this
instance's key" after a rollout). A two-key window (`FLOE_CONFIG_KEY_PREVIOUS`) is deferred (§12).

---

## 6. Authorization and audit

- Every `/api/v1/admin/*` route — reads included — requires an **admin** principal (D24's rule: `tokens[].admin`,
  oidc `admin_emails`/`admin_domains`, `mode = none` on loopback). Non-admin: 403; no credential: 401.
- `GET /api/v1/me` gains `admin: bool` so the SPA shows the Admin link only to admins.
- Every publish is audited three ways: the history record (author = principal, message, timestamp, diff), an INFO
  `config published` log line with the same fields, and — with the catalog feature on — a `sync_runs` row
  (`kind = "config"`, `source = "admin"`, detail = `revision N by X: message`). That row is lossy telemetry like
  every `sync_runs` row; the history record is the audit of record.

---

## 7. API surface (`web/API.md` §4, `repos.js` `repos.admin`)

Both lanes: `/api/v1/admin/…` (bearer or same-origin session) and `/api-browser/v1/admin/…`. All answers
`Cache-Control: no-store`.

| Route | SDK | |
|---|---|---|
| `GET /api/v1/admin/config` | `admin.config.get()` | `{revision, updated_at, author, message, document (redacted), history_mode, store}`; `ETag: "<revision>"` |
| `PUT /api/v1/admin/config` | `admin.config.put(doc, {base_revision, message})` | 200 `{revision, diff, restart_required}`, 400 `{error, errors[]}`, 409 `{error, revision}` |
| `POST /api/v1/admin/config/validate` | `admin.config.validate(doc)` | `{ok, errors[], diff, restart_required}` |
| `GET /api/v1/admin/config/schema` | `admin.config.schema()` | JSON Schema (draft 2020-12) with `x-floe` annotations: `format` (`duration`, `bytesize`, `secret`, `glob`), `live`, `group` |
| `GET /api/v1/admin/config/history?before=&n=` | `admin.config.history()` | newest first, ≤ 50 |
| `GET /api/v1/admin/config/revisions/{n}` | `admin.config.revision(n)` | one record, redacted document; 410 when expired |
| `POST /api/v1/admin/config/rollback` | `admin.config.rollback(n, {base_revision, message})` | as PUT |
| `GET /api/v1/admin/overview` | `admin.overview()` | revision, this instance, instances seen, mirror, catalog |
| `GET /api/v1/tls` (owned by the TLS change, D59) | `tls()` | read-only certificate status; the overview treats 404 as "not available in this build" |
| `GET /api/v1/admin/mirror` | `admin.mirror.status()` | state.json entries, lease holder, last pass, counts |
| `POST /api/v1/admin/mirror/preview` | `admin.mirror.preview(section, {discover})` | selection of known repositories under candidate include/exclude; `discover: true` = a real dry-run pass against the forge |
| `POST /api/v1/admin/mirror/test` | `admin.mirror.test(token?)` | `GET {api_url}/user` with the stored or candidate token: login, scopes, rate limit |
| `POST /api/v1/admin/mirror/sync` | `admin.mirror.sync()` | writes `sync-request.json`; the loop's supervisor restarts the loop (a pass within ~10 s) |
| `POST /api/v1/admin/mirror/pause` / `resume` | `admin.mirror.pause(full_name)` | a config change: adds/removes the exact `owner/name` in `github_mirror.exclude` (frozen = data kept, follow stopped, D49) |
| `GET /api/v1/admin/catalog` | `admin.catalog.status()` | compiled, enabled, writer running, uri/namespace |
| `POST /api/v1/admin/catalog/test` | `admin.catalog.test(section?)` | Iceberg REST `GET /v1/config` with the configured auth |

Pause is a config change on purpose: it is versioned, audited, survives every instance, and reuses the mirror's
existing frozen state instead of inventing a second one.

---

## 8. The UI (`web/src/admin/*`, route `/_admin/*`)

Same stack and styling as the rest of the SPA (React 19, react-router, `styles.css` tokens, `Box`, Suspense +
`useData`), built on `repos.admin.*` (dogfood rule). Lazy-loaded chunk; an "Admin" top-nav link for admins.

1. **Overview** — current revision (who/when/why), this instance (applied revision, restart required, apply
   error), instances seen (stale ones greyed), mirror (enabled, lease holder, last pass, statuses), catalog (compiled,
   enabled, running), **TLS certificate status read-only** from the TLS change's admin-only `GET /api/v1/tls`
   (`mode, domains, not_before, not_after, issuer, fingerprint, source, last_renewal_at, last_error,
   next_attempt_at`); a 404 (that change not merged or not in this build) renders a placeholder. No TLS form,
   ever: TLS is bootstrap (D59).
2. **GitHub mirroring** — the `github_mirror` section as a form rendered from the JSON Schema; the token as a
   write-only field with **Test credential**; sources; include/exclude with a **live preview** (debounced
   `mirror/preview` against known repositories, plus "Discover now" = dry-run pass); schedule; the mirrored
   repositories table (status, conflicts, last seen, links) with **Sync now** and per-repo **Pause/Resume**.
3. **Catalog** — the `catalog` section form (auth fields are plain env-reference fields, so the SigV4 change adds
   fields to the schema and the form renders them without UI work), **Test connection**, writer status.
4. **Repositories** — pick a repository; per-repo settings (D24) as a form over the common `[upstream]` /
   `[maintenance]` keys with a raw TOML view, and push policy (D16) as a rule list with a raw JSON view; both
   validate through the existing endpoints before saving.
5. **History** — revisions with author/message/time, a diff viewer, **Roll back to this revision**.

Cross-cutting: inline validation errors from `validate`; an unsaved-changes guard (`beforeunload` + in-app
navigation prompt); a sticky save bar with message + diff preview; empty/loading/error states (Suspense skeletons,
error boxes with retry); accessibility basics (labels bound to inputs, `aria-invalid` + `aria-describedby` for
errors, `role="status"` for save results, keyboard reachable controls, visible focus).

The schema→form mapping is a pure module (`admin/schema-form.ts`) with a node test (`node --test`, type stripping,
no new dependency) that runs in `pnpm run build`.

---

## 9. CLI and migration

```
floe config check|dump                  # unchanged; dump prints the bootstrap only
floe config show [--revision N] [--json]
floe config set FILE|- [--message M] [--base N]     # a whole document (TOML or JSON)
floe config validate FILE|-
floe config history [-n 20]
floe config rollback N [--message M]
floe config import OLD_FLOE_TOML [--message M]      # one shot: [github_mirror]/[catalog]/[events] → the store
```

The CLI talks to the config store directly with the bootstrap's bucket credentials (like `floe settings`), author
`cli:$USER`. `import` maps `github_mirror.token_env = "X"` to `token = {env = "X"}` and an `events.webhook_secret`
literal to a sealed value (needs `FLOE_CONFIG_KEY`, else it refuses and suggests an env reference), publishes, and
prints the sections to delete from the file. The keys are deleted from the file schema in the same change (§1.3).

---

## 10. Tests

| Where | What |
|---|---|
| `floe-config` | runtime sections refused in the file; env overrides of runtime keys ignored; `RuntimeConfig` round trip; secret serde; alias registry |
| `floe-store` | `object_versioning`/`get_version` on the memory store (versioned and not); prefixed forwarding |
| `floe-server::config_store` (unit, memory store) | publish/CAS/monotonic revision; 409 on stale base; history healing after a lost history write; records and versions modes; rollback; seal/unseal (wrong key, wrong path, tamper); redaction + `redacted` keep; invalid document → nothing written; diff; restart-required set |
| `tests/admin_config.rs` (HTTP) | 401/403/200 per principal on every route; PUT invalid = 400 and revision unchanged; 409; schema covers every document key; mirror pause = exclude entry; live apply via revalidate on a second instance |
| `web/src/admin/schema-form.test.ts` | schema → fields mapping (types, formats, secrets, groups), value coercion, error path matching |

---

## 11. Risks

| # | Risk | Mitigation |
|---|---|---|
| R1 | A bad document stops the mirror fleet-wide | Validated at publish (same code as startup); rollback is one click; an instance that cannot apply keeps the previous revision |
| R2 | Lost `FLOE_CONFIG_KEY` | Sealed values are unreadable; re-enter them (the GUI says which). Env references avoid the problem |
| R3 | Config store unreachable at startup | Start with defaults and keep revalidating; restart-only sections (catalog, events) need a restart once it is back — shown on the overview |
| R4 | `versions` mode history depends on bucket lifecycle | 410 on expired revisions; `records` is always available by config |
| R5 | Two admins edit at once | `base_revision` CAS → 409 with the newer revision; never a silent merge |
| R6 | Process-wide secret alias is global state | It is an in-memory cache of the bucket + env, rebuilt on every apply; one alias per secret field |
| R7 | Admin reads are admin-only, so a read-only operator sees nothing | Deliberate (the document names infrastructure); revisit with a `config_read` role if asked |

## 12. Deferred

- Live reload of the events bridge and the catalog writer (today: restart required).
- `[repo_defaults]`: fleet-wide defaults for the D24 sections, between host file and repository settings.
- Key rotation window (`FLOE_CONFIG_KEY_PREVIOUS`).
- `codeintel` / `mcp` sections when those features exist.
