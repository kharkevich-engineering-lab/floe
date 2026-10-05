//! `floe.toml` — the **bootstrap** configuration (D8, D60). Environment overrides use
//! `FLOE__SECTION__KEY=value` (double underscore = nesting), applied after
//! the file is parsed. `PORT` (a serverless host) overrides `server.listen` port.
//! Runtime sections (`[github_mirror]`, `[catalog]`, `[events]`) live in the
//! versioned config document in the bucket ([`runtime`], `docs/design/admin-ui.md`).

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, time::Duration};

pub mod refpattern;
pub mod runtime;
pub mod secret;

pub use runtime::RuntimeConfig;
pub use secret::Secret;

use anyhow::{Context, Result};
pub use bytesize::ByteSize;
use serde::{Deserialize, Serialize};
pub use std::str::FromStr;

mod tls;
pub use tls::{
    AcmeConfig, CloudflareConfig, DNS01, DnsProvider, LETSENCRYPT_PRODUCTION, LETSENCRYPT_STAGING,
    TlsConfig, TlsMode, valid_cert_name,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: ServerConfig,
    pub store: StoreConfig,
    pub cache: CacheConfig,
    pub wal: WalConfig,
    pub compaction: CompactionConfig,
    pub bundles: BundlesConfig,
    pub maintenance: MaintenanceConfig,
    pub placement: PlacementConfig,
    pub lfs: LfsConfig,
    pub git: GitConfig,
    pub upstream: UpstreamConfig,
    /// Links to the systems around a repository (per repo via settings).
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    pub events: EventsConfig,
    pub github: GithubConfig,
    pub github_mirror: GithubMirrorConfig,
    /// Iceberg audit tables (`docs/design/github-mirror.md` §C, D50).
    pub catalog: CatalogConfig,
    /// D60: where the runtime config document lives.
    pub config_store: ConfigStoreConfig,
}

/// `[config_store]` (D60, `docs/design/admin-ui.md` §1.4): where the versioned
/// runtime config document lives and how instances follow it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConfigStoreConfig {
    /// A dedicated bucket (same backend and credentials as `[store]`); unset = the main bucket.
    pub bucket: Option<String>,
    /// Key prefix inside that bucket (`config/`).
    pub prefix: String,
    /// Revalidation period: one conditional GET of `current.json`. `0` = only at startup.
    #[serde(with = "humantime_serde")]
    pub ttl: Duration,
    /// `auto` | `records` | `versions` — where history bodies live (§2.3).
    pub history: HistoryMode,
    /// Env var holding the 32-byte base64 key that seals secrets (D61).
    pub key_env: String,
}

impl Default for ConfigStoreConfig {
    fn default() -> Self {
        ConfigStoreConfig {
            bucket: None,
            prefix: "config/".into(),
            ttl: Duration::from_secs(15),
            history: HistoryMode::Auto,
            key_env: "FLOE_CONFIG_KEY".into(),
        }
    }
}

/// How the config store keeps history (`docs/design/admin-ui.md` §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    /// `versions` when the bucket has object versioning, else `records`.
    #[default]
    Auto,
    /// floe writes every revision's body into `history/<rev>.json`.
    Records,
    /// History bodies are the bucket's object versions of `current.json`.
    Versions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub http2: bool,
    pub max_concurrent_requests: usize,
    /// Per-repo cap on concurrent upload-pack/receive-pack processes.
    pub max_concurrent_per_repo: usize,
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    /// Graceful drain after SIGTERM: new object work (fetch/push/LFS) is
    /// refused with 503 + Retry-After and `/readyz` turns 503 at once; in-flight
    /// requests and the running maintenance unit get this long to finish
    /// before they are interrupted. Keep it below the process supervisor's stop
    /// grace; longer SSD-host jobs should be resumable rather than relying on drain.
    #[serde(with = "humantime_serde")]
    pub drain_timeout: Duration,
    /// Max size of a single pushed pack accepted over HTTP.
    pub max_push_bytes: ByteSize,
    /// Roles this instance performs. a serverless host: fronts get `["serve"]`, the
    /// single maintenance instance `["maintain"]` (checkpoint / bundle / compact
    /// loops over every repo; `compact` and `bundle` are its sub-roles). Empty = all.
    pub roles: Vec<Role>,
    pub auth: AuthConfig,
    /// Public base URL used when rendering absolute URIs (bundle lists, LFS).
    pub public_url: Option<String>,
    /// Create a repo on the first receive-pack push if it does not exist.
    pub auto_create_on_push: bool,
    /// Honour `X-Floe-Capabilities: accel-redirect` from an nginx edge
    /// (`deploy/nginx.conf.example`): static objects (bundles, LFS) are answered with
    /// `X-Accel-Redirect: /_store/` + `X-Floe-Store-Url` (and `-Authorization`) and no
    /// body, so nginx streams (and caches) the bytes itself. Only turn it on behind an
    /// edge that strips the capability header from clients: the answer carries a store
    /// credential. Off by default. Even when on, accel is honoured only for loopback
    /// peers (the example nginx talks to `127.0.0.1`) so a client on a public bind
    /// cannot spoof the capability header and steal the store credential.
    pub accel_redirect: bool,
    /// Browser origins allowed to call `/api/*` cross-origin with credentials
    /// (the `repos.js` browser lane from other sites). Exact origins or one
    /// leading `*.` wildcard per entry, e.g. `["https://*.docs.example.com"]`.
    /// Empty (default) = no cross-origin lane and no CORS headers. Non-empty:
    /// CORS with credentials for matching origins only, and state-changing
    /// methods require a matching `Origin` when one is sent. Browser identity
    /// is the app session cookie.
    pub cors_origins: Vec<String>,
    /// TLS terminated by floe itself (D39, D59): `off` | `files` | `acme` (DNS-01). A reverse
    /// proxy may terminate TLS and use `mode = "off"` (h2c); a standalone host serves
    /// `https://` directly so git, browsers and the SDK see one origin with no edge.
    pub tls: TlsConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Serve,
    Compact,
    Bundle,
    /// Background maintenance loop: checkpoint-if-due (refs-level), bundles-if-
    /// due, geometric compaction for repos whose pack set fits. Implies
    /// `Compact` + `Bundle`.
    Maintain,
    /// The events bridge (`docs/EVENTS.md`): tails every repo's WAL from a
    /// per-repo cursor and publishes `ref` events to the bus sinks (webhook,
    /// pubsub). Woken by `POST /_events/notify` (GCS notifications via Pub/Sub
    /// push) and a periodic sweep. A small separate service in production.
    Events,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    pub mode: AuthMode,
    /// Allow unauthenticated read (upload-pack, bundles, web UI) when mode != none.
    pub anonymous_read: bool,
    /// Static tokens (`token` mode, and accepted in `oidc` mode too — for robots): token → principal.
    /// Presented as `Authorization: Bearer <token>` or as the password of HTTP Basic.
    pub tokens: Vec<StaticToken>,
    /// OIDC issuer (`oidc` mode). Discovery at `<issuer>/.well-known/openid-configuration`
    /// supplies the JWKS, authorization and token endpoints. Any compliant provider works
    /// (Google, Microsoft Entra, Okta, Auth0, Keycloak, Dex, GitLab, ...).
    pub issuer: String,
    /// Email domains accepted by `oidc` (the `email` claim, `email_verified` required).
    pub allowed_domains: Vec<String>,
    /// Individual identities accepted by `oidc` (exact `email` match).
    pub allowed_emails: Vec<String>,
    /// Accepted `aud` values of ID tokens sent as `Authorization: Bearer` (CLI/CI clients that
    /// mint ID tokens themselves). `oauth_client_id` is always accepted. Empty = only the
    /// web client.
    pub audiences: Vec<String>,
    /// Domains permitted to write. If unset, all allowed domains may write.
    pub write_domains: Option<Vec<String>>,
    /// Principals allowed to forward an end-user identity (`X-Floe-Principal`) to this
    /// server — the host that fronts a push broker. Names as they authenticate here (a static
    /// token's `principal`, or an email).
    pub trusted_forwarders: Vec<String>,
    /// OIDC emails that may PUT/DELETE settings and `policy.json`. Empty = none via email.
    #[serde(default)]
    pub admin_emails: Vec<String>,
    /// OIDC email domains that may PUT/DELETE settings and `policy.json`. Empty = none via domain.
    #[serde(default)]
    pub admin_domains: Vec<String>,
    /// HMAC key for the browser session cookie and for floe-issued access tokens
    /// (`oidc` mode). Unset = cookie sessions and issued tokens off. When set, must be ≥ 32
    /// bytes; shared by every host that answers a browser.
    pub session_secret: Option<String>,
    /// Session cookie lifetime (default 30 d). Sliding: a response to a request whose
    /// session is older than a quarter of this re-issues the cookie.
    #[serde(with = "humantime_serde")]
    pub session_ttl: Duration,
    /// Lifetime of access tokens minted at `/_auth/tokens` for git and scripts (default 90 d).
    /// Tokens are stateless (HMAC over principal + expiry); rotating `session_secret` revokes
    /// every one of them.
    #[serde(with = "humantime_serde")]
    pub access_token_ttl: Duration,
    /// OAuth client for `/_auth/login` → issuer → `/_auth/callback` (authorization code flow).
    /// Pair with `oauth_client_secret`; both or neither.
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
}

/// Prefix of access tokens floe mints itself (`/_auth/tokens`): recognisable in logs and
/// secret scanners, never confused with an ID token.
pub const ACCESS_TOKEN_PREFIX: &str = "wgt_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Everyone is `anon` with write **and admin**. `Config::validate` refuses this
    /// unless `server.listen` is loopback.
    #[default]
    None,
    /// Static tokens from the config (`tokens`), bearer or basic.
    Token,
    /// `OpenID` Connect: browser sign-in through the issuer, ID tokens as bearers, plus
    /// floe-issued access tokens for git — and `tokens` for robots.
    Oidc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticToken {
    pub principal: String,
    /// Read from env var if set, else literal.
    pub token: String,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default = "default_true")]
    pub write: bool,
    /// Mutate per-repo settings and `policy.json`. Default false: write is push, not admin.
    #[serde(default)]
    pub admin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StoreConfig {
    pub backend: StoreBackend,
    pub bucket: String,
    /// Global key prefix inside the bucket (no leading slash; trailing slash added).
    pub prefix: String,
    pub gcs: GcsConfig,
    pub s3: S3Config,
    pub max_retries: u32,
    /// Objects larger than this use resumable/multipart upload.
    pub multipart_threshold: ByteSize,
    pub multipart_part_size: ByteSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StoreBackend {
    #[default]
    Gcs,
    S3,
    /// Tests only.
    Memory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GcsConfig {
    /// gRPC endpoint.
    pub endpoint: String,
    pub direct_connectivity: bool,
    /// Service account for signed URLs; None = ADC/IAM signBlob.
    pub signing_service_account: Option<String>,
    /// Separate data clients (own channels) for bulk traffic — pack/idx/side-
    /// file/bundle/LFS bytes and ranged reads — so the control plane
    /// (manifest, log, checkpoint, lease GETs/PUTs) never queues behind a
    /// multi-GB download on a shared HTTP/2 connection.
    #[serde(default = "default_bulk_clients")]
    pub bulk_clients: usize,
    /// Max concurrent bulk requests per process (stripes + range reads).
    #[serde(default = "default_bulk_concurrency")]
    pub bulk_concurrency: usize,
}

fn default_bulk_clients() -> usize {
    4
}
fn default_bulk_concurrency() -> usize {
    32
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub access_key_env: String,
    pub secret_key_env: String,
    pub force_path_style: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    /// Local directory holding materialized repositories.
    pub dir: PathBuf,
    /// `budget` (tmpfs: `max_bytes` caps everything on disk; too-large
    /// repos are served remotely/linked) or `disk` (the SSD host, D25: the cache
    /// dir is a real disk — no budget, every repo fully local, eviction only on
    /// disk pressure at `disk_high_watermark`). `auto` = `disk` when
    /// `maintenance.disk = "ssd"`, else `budget`.
    pub mode: CacheMode,
    /// Budget for everything on disk in `budget` mode. Ignored in `disk` mode.
    pub max_bytes: ByteSize,
    /// `disk` mode: evict idle repos (oldest first) when the filesystem holding
    /// `dir` is fuller than this fraction (0 = never evict on pressure).
    pub disk_high_watermark: f64,
    #[serde(with = "humantime_serde")]
    pub evict_idle_after: Duration,
    /// Repos to warm at startup ("owner/name"): refs, then objects (packs when
    /// they fit, otherwise the remote pack indexes) and the default branch's
    /// root tree, so the first request on a fresh instance is not the one that
    /// pays for it. Each runs as a `prewarm` task.
    pub prewarm: Vec<String>,
    pub prewarm_parallelism: usize,
    /// `/readyz` answers 503 until every prewarm finished or this much time
    /// passed (0 = never block readiness). With a serverless host startup probe on
    /// /readyz, traffic only reaches an instance that is already warm.
    #[serde(with = "humantime_serde")]
    pub prewarm_ready_timeout: Duration,
    /// Max entries in the ref advertisement cache (per rendered ls-refs / v0 advert).
    pub ref_advert_entries: usize,
    /// Max entries in the object-info (size/has) cache.
    pub object_info_entries: usize,
    /// Max entries in the bundle list render cache.
    pub bundle_list_entries: usize,
    /// Process-wide LRU of pack data blocks (1 MiB range reads) used when a
    /// repo's pack set does not fit `max_bytes` and objects are read straight
    /// from the object store.
    pub remote_block_bytes: ByteSize,
    /// Per-repo LRU of decoded objects (delta bases) for the remote reader.
    pub remote_object_bytes: ByteSize,
    /// Mirror rendered sha-addressed web API responses into the object store
    /// (`repos/<o>/<r>/cache/api/...`) so every instance shares one render
    /// cache. Only consulted when objects are read remotely (local renders are
    /// cheaper than a store round trip).
    pub shared_render_cache: bool,
    /// Read-only mount of the object-store bucket (gcsfuse, s3fs, rclone, etc.),
    /// e.g. `/mnt/store`. When set, the "Serve" sync level
    /// never copies tier-2 base packs onto tmpfs: their side-files
    /// (idx/rev/bitmap/commit-graph) are downloaded and `pack-<sha>.pack` is a
    /// symlink to `<store_mount>/<store.prefix>repos/<o>/<r>/wal/<sha>.pack`,
    /// so stock git can still read any base object (slowly) while everything
    /// hot (refs, recent packs, indexes, commit-graph) is local.
    pub store_mount: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_excessive_bools, reason = "config flags")]
pub struct WalConfig {
    /// Coalesce concurrent publishes to one repo within this window into one index CAS.
    #[serde(with = "humantime_serde")]
    pub batch_window: Duration,
    pub max_batch: usize,
    /// Route receive-pack writes through a single stateless broker when set.
    pub push_broker_url: Option<String>,
    /// Static token this host presents to the broker (`Authorization: Bearer`); the broker
    /// lists it in `server.auth.tokens` and its principal in `trusted_forwarders`.
    /// `FLOE_BROKER_TOKEN` in the environment overrides it.
    pub push_broker_token: Option<String>,
    /// Maximum receive-pack body retained for a safe broker-to-local fallback.
    pub push_broker_buffer_bytes: ByteSize,
    /// Checkpoint (ref snapshot + pack set, log folded) when this many entries
    /// accumulated since the last one (0 = never by count).
    pub snapshot_every_entries: u64,
    /// ... or when the last checkpoint is older than this (0 = never by age).
    /// Cold readers load checkpoint + tail, so the tail stays short.
    #[serde(with = "humantime_serde")]
    pub checkpoint_interval: Duration,
    /// ... or when the log tail after the checkpoint exceeds this many bytes
    /// (0 = never by size).
    pub checkpoint_tail_bytes: ByteSize,
    pub cas_max_retries: u32,
    /// Verify pushed objects (fsck-level) before publish.
    pub fsck_objects: bool,
    /// Require every pushed ref tip to be connected to existing objects + the pack.
    pub check_connectivity: bool,
    /// Skip the index freshness GET if the last check was younger than this (0 = always check).
    #[serde(with = "humantime_serde")]
    pub freshness_ttl: Duration,
    /// After a refs-only sync (info/refs, ls-refs, bundle-uri, web refs) on a
    /// copy whose packs are not yet reconciled, start downloading the packs in
    /// the background so the first fetch does not pay for it.
    pub prefetch_packs: bool,
    /// Upper bound on what `prefetch_packs` pulls unasked: a pack set whose serving copy would
    /// put more than this on local disk is materialized only when a request needs it (fetch,
    /// push, web object work — narrated, CPU allocated), never because someone opened the
    /// repository page. 2026-08-22: an overview view made a front download acme/large's single
    /// 11.9 GB pack in the background for 5 min (nothing asked for its objects) while the runtime
    /// watchdog fired 11×. 0 = no bound.
    pub prefetch_max_bytes: ByteSize,
    /// When the live pack set exceeds `cache.max_bytes`, serve object reads
    /// (web API) straight from the store: pack indexes local, pack data by
    /// range read. Off => such repos answer 503 for object work.
    pub remote_objects: bool,
}

/// The `maintain` role's loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MaintenanceConfig {
    /// Pause between passes over the assigned repositories.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Checkpoint repos whose trigger fired (see `wal.snapshot_every_entries`,
    /// `wal.checkpoint_interval`, `wal.checkpoint_tail_bytes`).
    pub checkpoints: bool,
    /// Declared capacity: the largest pack set (bytes) this host can hold as a
    /// local copy for a unit that needs one (full repack, incremental cut on a
    /// local copy). 0 = use `cache.max_bytes`. A unit whose need exceeds it is
    /// plan state `wrong-host` — visible, never an error.
    pub max_pack_bytes: ByteSize,
    /// `tmpfs` (a serverless host: disk is memory) | `ssd` (a VM: can repack bases).
    #[serde(default)]
    pub disk: MaintainerDisk,
    /// Heartbeat object name (`maintain/<host>.pb`): who maintains what and
    /// whether that host is alive. Default: the instance id.
    #[serde(default)]
    pub host: Option<String>,
    /// Connectivity audit cadence: `git fsck --connectivity-only` over a complete
    /// local copy, result at `repos/<o>/<r>/fsck.pb` (missing objects →
    /// `floe_repo_missing_objects{repo}` and the `repair` unit). Lowest
    /// priority unit; only on hosts that hold the whole pack set. 0 = off.
    #[serde(with = "humantime_serde")]
    pub fsck_interval: Duration,
    /// How often the upstream-follow loop (`upstream.follow`, ingress, its own
    /// loop next to the priority loop so a long unit never delays it) fetches
    /// each assigned repository's followed refs. 0 = off on this host.
    #[serde(with = "humantime_serde")]
    pub follow_interval: Duration,
}

/// D25: how the local cache is bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    /// Follow `maintenance.disk`: `ssd` ⇒ `disk`, else `budget`.
    #[default]
    Auto,
    /// `cache.max_bytes` is the budget (tmpfs).
    Budget,
    /// No budget: real disk, full materialization, watermark eviction.
    Disk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MaintainerDisk {
    #[default]
    Tmpfs,
    Ssd,
}

fn default_all_repos() -> Vec<String> {
    vec!["*".into()]
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        MaintenanceConfig {
            interval: Duration::from_mins(1),
            checkpoints: true,
            max_pack_bytes: ByteSize::b(0),
            disk: MaintainerDisk::Tmpfs,
            host: None,
            fsck_interval: Duration::from_hours(7 * 24),
            follow_interval: Duration::from_secs(30),
        }
    }
}

/// Which repositories this host attends to. Two independent roles, each a
/// glob list minus an exclude list (`owner/name` | `owner/*` | `*`):
/// * **serve**: object work — `git-upload-pack`, `git-receive-pack`, LFS transfer. A
///   host that does not serve a repo answers those with 503 + `Retry-After` (+ a band-3
///   line naming the host that does) *before* any sync or materialize; refs-level
///   reads (info/refs, the API via the remote reader, UI, bundle list) stay available
///   everywhere, so the edge's read-only fallback (D29) works.
/// * **maintain**: the maintainer loop's units (checkpoints, bundles, compaction,
///   fsck/repair) — only on hosts with the `maintain` role.
///
/// Placement is by rule, not by capacity: a repo is either this host's or not.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PlacementConfig {
    pub serve: Vec<String>,
    pub serve_exclude: Vec<String>,
    pub maintain: Vec<String>,
    pub maintain_exclude: Vec<String>,
}

impl Default for PlacementConfig {
    fn default() -> Self {
        PlacementConfig {
            serve: default_all_repos(),
            serve_exclude: Vec::new(),
            maintain: default_all_repos(),
            maintain_exclude: Vec::new(),
        }
    }
}

impl PlacementConfig {
    /// Object work for `owner/name` happens on this host.
    pub fn serves(&self, owner: &str, name: &str) -> bool {
        repo_listed(&self.serve, owner, name) && !repo_listed(&self.serve_exclude, owner, name)
    }
    /// Maintenance units for `owner/name` run on this host (given the role).
    pub fn maintains(&self, owner: &str, name: &str) -> bool {
        repo_listed(&self.maintain, owner, name)
            && !repo_listed(&self.maintain_exclude, owner, name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CompactionConfig {
    pub enabled: bool,
    /// Geometric factor between tiers.
    pub factor: u32,
    /// Compact when this many fresh (tier 0) packs exist.
    pub trigger_packs: usize,
    /// Or when fresh pack bytes exceed this.
    pub trigger_bytes: ByteSize,
    #[serde(with = "humantime_serde")]
    pub lease_ttl: Duration,
    /// Keep superseded packs and old index generations for this long (provenance/rewind).
    #[serde(with = "humantime_serde")]
    pub retention_superseded: Duration,
    /// Use upstream git for delta compression (`git repack`); gix does not delta-compress.
    pub engine: RepackEngine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RepackEngine {
    #[default]
    Git,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_excessive_bools, reason = "config flags")]
pub struct BundlesConfig {
    pub enabled: bool,
    pub strategy: Vec<BundleStrategy>,
    /// Minimum-size gate for **incremental** slots: a slot whose content
    /// (commits on the bundle's refs since its base bundle's tips) has fewer
    /// commits than this is not built (plan state `too-small`; the next slot of
    /// the strategy is built on the same base, so nothing is lost). Fulls are
    /// never gated. 0 = no gate. Per-strategy `min_commits` overrides.
    #[serde(default = "default_min_commits")]
    pub min_commits: u64,
    /// Optional second guard: skip incrementals whose pack would be smaller
    /// than this (0 = off).
    #[serde(default)]
    pub min_bytes: ByteSize,
    pub serve_via: BundleServe,
    #[serde(with = "humantime_serde")]
    pub signed_url_ttl: Duration,
    /// Advertise `bundle-uri` in protocol v2 capabilities.
    pub advertise: bool,
    /// Put the filtered families INTO the plain `bundles/list` and the v2
    /// advertisement (with their `bundle.<id>.filter` lines) instead of only
    /// at `bundles/list?filter=…`. Only for clients whose git matches
    /// `bundle.<id>.filter` against the clone's filter (a patched Git client
    /// patch, `docs/patches/`): stock git ignores the key and a full clone
    /// would swallow the blobless bundles (design §6b). Default false.
    #[serde(default)]
    pub advertise_filtered: bool,
    /// Repositories (`owner/name`, or `owner/*`) whose clones **must** go
    /// through bundle-uri: a fetch with zero `have`s gets a pkt ERR / band-3
    /// message with the exact fix instead of an impossible full pack. Fetches
    /// with haves proceed normally.
    #[serde(default)]
    pub require: Vec<String>,
    /// Repositories (`owner/name` | `owner/*`) whose bundle URIs are always
    /// signed store URLs regardless of `serve_via`: clone bytes of the biggest
    /// repos bypass the fronts entirely.
    #[serde(default)]
    pub signed_url_for: Vec<String>,
    /// Default ref set of a bundle when a strategy has no `refs`: `HEAD` +
    /// `refs/heads/main` when true (branches are tiny per-fetch deltas on top
    /// of main; bundling every branch makes rebased branches *slower* to
    /// fetch, not faster), `refs/heads/*` + `refs/tags/*` + `HEAD` when false.
    #[serde(default = "default_true")]
    pub main_only: bool,
    /// Extra ref globs added to every bundle's default ref set (e.g. `refs/tags/v*`).
    #[serde(default)]
    pub extra_refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BundleServe {
    #[default]
    Proxy,
    SignedUrl,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleStrategy {
    pub name: String,
    pub kind: BundleKind,
    /// Cron expression (6 or 7 fields, `cron` crate syntax) or "@hourly"/"@daily"/"@weekly".
    pub schedule: String,
    /// For incremental: name of the strategy this one is based on.
    #[serde(default)]
    pub base: Option<String>,
    /// Full strategies only: how many newest fulls stay listed (>= 1). Incrementals have
    /// no knob — always the 2 newest whose base is kept (`floe_bundle::slots::INCREMENTALS_KEPT`,
    /// D21 amended 2026-08-22); setting `keep` on one is a configuration error.
    #[serde(default)]
    pub keep: usize,
    /// Ref globs included in the bundle. Default: see `bundles.main_only`.
    #[serde(default)]
    pub refs: Vec<String>,
    /// Backfill horizon: how many missing slots (oldest first) one maintainer
    /// pass may build for this strategy (0 = unlimited). Keeps a long outage
    /// from turning into hours of catch-up in one pass.
    #[serde(default)]
    pub backfill_max: usize,
    /// Override of `bundles.min_commits` for this strategy (None = inherit).
    #[serde(default)]
    pub min_commits: Option<u64>,
    /// Object filter of the bundles this strategy builds (`"blob:none"` is the
    /// only supported value): a **blobless family** for `--filter=blob:none`
    /// clones. A full strategy with a filter composes the D18 history pack
    /// (commits + trees) under a `@filter=blob:none` header; incrementals pack
    /// with `--filter=blob:none`. Whole chains share one filter. Filtered
    /// bundles are advertised only at `bundles/list?filter=blob:none` — never
    /// in the protocol-advertised list: git (2.47 … master) does not match
    /// `bundle.<id>.filter` against the clone's filter, so a full clone would
    /// swallow them and end up with promisor packs it cannot complete.
    #[serde(default)]
    pub filter: Option<String>,
    /// Incrementals only. `false` (default, D21): every slot is cut on its **base** (a daily on the
    /// weekly, an hourly on the newest daily), so the newest one subsumes the older ones and only the
    /// 2 newest are listed — a fresh clone is 5 downloads, a catch-up ≤ 2, bytes overlap.
    /// `true`: a slot is cut on this strategy's **own previous bundle** when that one is newer than
    /// the newest base bundle at or before the slot (dailies chain from the weekly, hourlies restart
    /// from each daily); every slot since the base is listed (≤ 7 dailies, ≤ 24 hourlies) and each
    /// carries exactly its delta — more downloads, no overlapping bytes, a catch-up is exactly the
    /// slots missed. `floe_bundle::slots` is the one place that knows either rule.
    #[serde(default)]
    pub chain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleKind {
    Full,
    Incremental,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LfsConfig {
    pub enabled: bool,
    pub serve_via: BundleServe,
    #[serde(with = "humantime_serde")]
    pub signed_url_ttl: Duration,
    pub max_object_bytes: ByteSize,
}

/// Where a repository's history lives when it is not (all) here: a repository
/// imported from GitHub keeps its LFS objects there, and a hole in the imported
/// pack set (the original large-repository measurements: 1,952 blobs) is repaired from there. Per
/// repository via D24 settings (`[upstream]`); one token for both.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct UpstreamConfig {
    /// Git remote URL (`https://github.com/acme/monorepo.git`): source for the
    /// maintainer's `repair` unit (fetches exactly the objects `fsck` found
    /// missing; GitHub serves blob/tree wants by SHA).
    pub git: Option<String>,
    /// LFS endpoint (`https://github.com/acme/monorepo.git/info/lfs`): read-through
    /// for LFS objects this store lacks — batch `upload` reports them present
    /// (no actions) so pushes proceed; `download`/GET stream through and persist
    /// after a complete sha256-verified read. Unset = missing objects are 404.
    pub lfs: Option<String>,
    /// Name of an environment variable on the maintaining host that holds the token
    /// (settings live in the bucket, so never the token itself); sent as HTTP Basic
    /// `x-access-token:<token>` (GitHub). Unset = unauthenticated. Host-only; see
    /// [`Config::upstream_token_env`] for the resolution order.
    pub token_env: Option<String>,
    /// Ref patterns kept equal to the upstream's (D48, [`refpattern`]: git refspec
    /// sources, `refs/heads/*`, `^refs/heads/x/*`): the host that maintains the
    /// repository (D28: its writer) fetches the delta from `git` and publishes it
    /// through the WAL as an ordinary push. Empty = follow off; checked before, and
    /// instead of, `RefPatterns::parse`, so `follow = []` is always valid.
    pub follow: Vec<String>,
    /// What follow does when upstream rewrites (non-fast-forward, or any tag move)
    /// or deletes a followed ref.
    pub on_rewrite: OnRewrite,
    /// Desired symbolic target of HEAD (`refs/heads/main`): follow retargets HEAD
    /// when the WAL's differs and the target exists. Unset = follow never touches HEAD.
    pub head: Option<String>,
    /// Per-repository minimum pause between follow rounds (the loop still ticks
    /// every `maintenance.follow_interval`; a repository whose last round is younger
    /// than this is skipped). Unset = every tick.
    #[serde(default, with = "humantime_serde::option")]
    pub follow_interval: Option<Duration>,
    /// Opaque provenance label (`github:123456789`): who manages this `[upstream]`.
    /// Follow only logs it.
    pub source: Option<String>,
    /// Host-only: env var name per upstream host (`"github.com" = "FLOE_GITHUB_TOKEN"`);
    /// beats `token_env` for URLs on that host. Refused in settings and stripped from
    /// the public effective config, exactly like `token_env`.
    pub token_env_by_host: BTreeMap<String, String>,
}

/// D48: what follow does when upstream rewrites or deletes a followed ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnRewrite {
    /// Keep the old tip under `refs/archive/<unix-ts>/<ref>`, then apply upstream's
    /// state, in one WAL entry. Nothing is lost.
    #[default]
    Archive,
    /// D33 behaviour: refuse non-fast-forwards and leave refs deleted upstream as
    /// they are; logged every round.
    Refuse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_excessive_bools, reason = "config flags")]
pub struct GitConfig {
    /// Path to the upstream git binary (repack, bundle, optional upload-pack engine).
    pub binary: PathBuf,
    pub upload_pack_engine: UploadPackEngine,
    pub allow_filter: bool,
    pub allow_any_sha1_in_want: bool,
    /// Default object format for new repos.
    pub object_format: ObjectFormat,
    /// Maintain a split commit-graph chain per local repo: tier-2 packs that
    /// publish a commit-graph layer install it as the chain base; every other
    /// installed pack is folded in incrementally (`commit-graph write --split`).
    /// History walks then never touch base pack data.
    #[serde(default = "default_true")]
    pub commit_graph: bool,
    /// Compute changed-path Bloom filters for incremental layers (`git log --
    /// path` speed-up). Diffs new commits against parent trees, so it needs
    /// the parents' tree data reachable (local or mounted base pack).
    #[serde(default)]
    pub commit_graph_changed_paths: bool,
    /// At a base rebuild, also publish a **history pack** (commits + trees of
    /// the base, `pack-objects --filter=blob:none`) that instances keep as a
    /// real local pack next to a linked/remote base (D18).
    #[serde(default = "default_true")]
    pub history_pack: bool,
    /// Refuse a v2 `fetch` asking for more than this many objects (0 = no bound). A blobless clone
    /// without `--sparse`/`--no-checkout` makes git fetch every blob of HEAD's tree in one lazy
    /// request right after cloning (a large repository: 1.47 M wants, 49 GB RSS, > 12 min on the SSD host); the
    /// refusal names the fix. Ordinary fetches want a handful of tips; a `--sparse` checkout fetches
    /// its cone's blobs, a few thousand at most. Set it per host above the largest honest request.
    #[serde(default)]
    pub max_wants: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UploadPackEngine {
    /// In-process gitoxide engine (`floe-git/src/upload_gix.rs`): commit-graph walks,
    /// tree-diff enumeration, streaming, base objects faulted by range when the base is
    /// remote-served. Best for commit fetches and filtered/shallow clones.
    Gix,
    /// `git upload-pack --stateless-rpc` subprocess: delta reuse + bitmaps; best for
    /// many-blob wants (a partial clone's lazy checkout) and full unfiltered clones.
    Git,
    /// Per request: gix for commit wants, git when the wants are blobs/trees (lazy
    /// checkout). Default since 2026-08-20 (the original A/B/C measurements).
    #[default]
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFormat {
    #[default]
    Sha1,
    Sha256,
}

/// Events (`docs/EVENTS.md`): the bridge (`Role::Events`) tails every repo's
/// WAL from a durable cursor and publishes `ref` events to the webhook. Only the
/// bridge reads this section; nothing on the push path does.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EventsConfig {
    /// Where ref events go: each batch is `POST`ed as a JSON array (`docs/EVENTS.md`).
    /// Unset = the events role has nothing to do.
    pub webhook_url: Option<String>,
    /// Shared secret for `X-Floe-Signature: sha256=<HMAC-SHA256 of the body>`. Unset = unsigned.
    /// A [`Secret`] (D61): an env reference or a sealed value.
    pub webhook_secret: Option<Secret>,
    /// Catch-all sweep over every repo (a `list` + one conditional manifest
    /// GET per repo), the backstop behind store notifications; the bridge
    /// warns when a sweep finds unpublished entries. `0` = off.
    #[serde(with = "humantime_serde")]
    pub sweep_interval: Duration,
}

impl EventsConfig {
    /// The section's own checks.
    pub fn check(&self) -> Result<()> {
        if let Some(u) = &self.webhook_url {
            anyhow::ensure!(
                u.starts_with("http://") || u.starts_with("https://"),
                "events.webhook_url must be an http(s) URL"
            );
        }
        Ok(())
    }
}

impl Default for EventsConfig {
    fn default() -> Self {
        EventsConfig {
            webhook_url: None,
            webhook_secret: None,
            sweep_interval: Duration::from_mins(5),
        }
    }
}

/// `docs/GITHUB.md` — the GitHub Enterprise Server facade: `/api/v3/*`,
/// `/api/graphql` and `/login/oauth/*` answered from the WAL so an octokit
/// pointed at this host reads and writes real repositories in the bucket.
///
/// **Local development only.** The facade is auth-free by construction: it
/// accepts any bearer, reports one hardcoded user with admin on everything and
/// never consults `server.auth`. `Config::validate` therefore requires
/// `server.auth.mode = "none"` and, with the facade on, lets that mode bind
/// beyond loopback: the network the process sits on is the trust boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GithubConfig {
    /// Mount the facade. Off everywhere but a developer's machine.
    pub enabled: bool,
    /// How often the bridge polls for ref changes while the github sink is on.
    /// A push on this instance wakes the bridge directly, so this only covers
    /// writes by another process (and stores with no notifications, which is
    /// every dev bucket). `0` = only `events.sweep_interval`.
    #[serde(with = "humantime_serde")]
    pub webhook_poll_interval: Duration,
}

impl Default for GithubConfig {
    fn default() -> Self {
        GithubConfig {
            enabled: false,
            webhook_poll_interval: Duration::from_secs(1),
        }
    }
}

/// D49 — the GitHub mirror (`floe-mirror`, `docs/design/github-mirror.md` §B):
/// discovers repositories on GitHub, creates the same `<owner>/<name>` in floe
/// (lowercased) with an `[upstream]` table and a `repo.description` naming the
/// source, and lets follow (D48) move the bytes. Host-level only (not
/// a settings section); runs on a `maintain` host under `leases/mirror-github.pb`.
/// `[github]` is the facade (D42), hence the name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_excessive_bools, reason = "independent config switches")]
pub struct GithubMirrorConfig {
    /// Run the mirror on this host (needs the `maintain` role).
    pub enabled: bool,
    /// REST base (`https://ghe.example.com/api/v3` for GHES).
    pub api_url: String,
    /// Base of clone/LFS URLs, and the `upstream.token_env_by_host` key.
    pub git_url: String,
    /// The credential (D61): a PAT (classic `repo`, or fine-grained Contents:read +
    /// Metadata:read) as an env reference (`{ env = "FLOE_GITHUB_TOKEN" }`, the
    /// default; nothing in the bucket) or a sealed value entered in the admin GUI.
    pub token: Secret,
    /// Derived, never configured: the env var name the token resolves through
    /// ([`secret::env_var`]); [`secret::GITHUB_MIRROR_TOKEN_ALIAS`] once a config
    /// document is applied. Read at every pass, so rotation needs no restart.
    #[serde(skip)]
    pub token_env: String,
    /// Discovery/reconcile cadence. `0` = only at startup (and `floe github sync --once`).
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Owners whose repositories are mirrored; `"@me"` = the token's user, private included.
    pub users: Vec<String>,
    /// Organisations (all repository types the token can see).
    pub orgs: Vec<String>,
    /// Users whose stars are mirrored (`"@me"` allowed).
    pub starred: Vec<String>,
    /// Explicit `"owner/name"`: bypass `include` and the archived/fork skips.
    pub repos: Vec<String>,
    /// Globs over `owner/name` (case-insensitive; `*` stops at `/`, so each has one `/`).
    pub include: Vec<String>,
    /// Same syntax; wins over everything, explicit `repos` included.
    pub exclude: Vec<String>,
    /// Do not start mirroring archived repositories (mirrored ones are kept).
    pub skip_archived: bool,
    /// Do not start mirroring forks.
    pub skip_forks: bool,
    /// `false` = public repositories only.
    pub include_private: bool,
    /// The operator's acknowledgement that floe has no per-repository read ACL:
    /// every reader of this floe reads every mirrored private repository.
    /// Required with `include_private` in every auth mode: the bucket is the
    /// fleet's, so this host's auth mode does not bound who reads it.
    pub private_visible_to_all_readers: bool,
    /// Write `upstream.lfs` so LFS objects read through (`docs/LFS.md`).
    pub lfs: bool,
    /// Patterns written into each repository's `upstream.follow`.
    pub follow: Vec<String>,
    /// Written into `upstream.on_rewrite`.
    pub on_rewrite: OnRewrite,
    /// Written into `upstream.follow_interval` (a backstop; pushes are nudged).
    #[serde(with = "humantime_serde")]
    pub follow_interval: Duration,
    /// Publish a deny-all `policy.json` at creation, so only follow moves refs.
    pub read_only: bool,
    /// Larger repositories (GitHub `size`) are `too-large`: never auto-created,
    /// logged with the `floe import` handoff recipe. `0` = no limit.
    pub max_repo_size: ByteSize,
    /// Bound on creations per pass.
    pub max_new_per_pass: usize,
    /// Stop a pass (incomplete) when `x-ratelimit-remaining` drops below this.
    pub min_rate_remaining: u32,
    /// How long a repository must be missing (404/403) before it is `gone`/`forbidden`.
    #[serde(with = "humantime_serde")]
    pub gone_after: Duration,
    /// TTL of `leases/mirror-github.pb`; heartbeat every `lease_ttl / 3`.
    #[serde(with = "humantime_serde")]
    pub lease_ttl: Duration,
    /// Set at load by [`Config::derive_mirror_token_env`], never from TOML: the
    /// mirror runs here or `token_env` is set in this process, so follow and LFS
    /// read-through on mirror-managed repositories (`upstream.source =
    /// "github:…"` on `git_url`'s host) authenticate with `token_env`.
    #[serde(skip)]
    pub use_token: bool,
}

impl Default for GithubMirrorConfig {
    fn default() -> Self {
        GithubMirrorConfig {
            enabled: false,
            api_url: "https://api.github.com".into(),
            git_url: "https://github.com".into(),
            token: Secret::Env("FLOE_GITHUB_TOKEN".into()),
            token_env: "FLOE_GITHUB_TOKEN".into(),
            interval: Duration::from_mins(5),
            users: Vec::new(),
            orgs: Vec::new(),
            starred: Vec::new(),
            repos: Vec::new(),
            include: vec!["*/*".into()],
            exclude: Vec::new(),
            skip_archived: true,
            skip_forks: true,
            include_private: true,
            private_visible_to_all_readers: false,
            lfs: true,
            follow: vec!["refs/heads/*".into(), "refs/tags/*".into()],
            on_rewrite: OnRewrite::Archive,
            follow_interval: Duration::from_mins(10),
            read_only: true,
            max_repo_size: ByteSize::gib(2),
            max_new_per_pass: 20,
            min_rate_remaining: 200,
            gone_after: Duration::from_hours(24),
            lease_ttl: Duration::from_mins(2),
            use_token: false,
        }
    }
}

/// `[catalog]` (`docs/design/github-mirror.md` §C, D50): Iceberg audit tables
/// (`ref_events`, `force_push_log`, `sync_runs`, `repo_inventory`) behind an
/// Iceberg REST catalog. Derived copies, never a source of truth: the WAL keeps
/// every ref event, and a catalog outage only adds catalog lag. The section
/// parses in every build; `enabled = true` needs a binary built with
/// `--features catalog` (the server refuses to start otherwise).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_excessive_bools)] // independent config switches
pub struct CatalogConfig {
    pub enabled: bool,
    /// Iceberg REST catalog base URL (`RustFS` S3 Tables: its Iceberg REST endpoint).
    pub uri: Option<String>,
    /// Warehouse identifier (S3 Tables: the table bucket the endpoint expects).
    pub warehouse: Option<String>,
    /// Iceberg namespace; created when absent (`create_tables`).
    pub namespace: String,
    /// How floe authenticates to the REST catalog (D63): `none`, `bearer`
    /// (`token_env` or `credential_env`), or `sigv4` (every request signed
    /// with the data-file credentials; `RustFS` and AWS S3 Tables).
    pub auth: CatalogAuth,
    /// `SigV4` signing name: `s3` for `RustFS`'s `/iceberg` endpoint, `s3tables`
    /// for AWS S3 Tables (and `RustFS`'s `/_iceberg` alias).
    pub sigv4_service: String,
    /// `SigV4` region; unset = `s3_region`.
    pub sigv4_region: Option<String>,
    /// Env var holding a bearer token for the catalog (`auth = "bearer"`).
    /// Never the value itself.
    pub token_env: Option<String>,
    /// Env var holding `client_id:client_secret` (REST `OAuth2` client
    /// credentials, `auth = "bearer"`).
    pub credential_env: Option<String>,
    /// `FileIO` endpoint for data files when the catalog does not vend credentials.
    pub s3_endpoint: Option<String>,
    pub s3_region: String,
    /// Env var *names* for the data-file credentials (D43: both set = static
    /// keys, plus `AWS_SESSION_TOKEN` when set; neither = the AWS SDK default
    /// chain; one alone is an error). With `auth = "sigv4"` the same
    /// credentials sign the catalog requests.
    pub s3_access_key_env: String,
    pub s3_secret_key_env: String,
    /// Path-style addressing (`RustFS`/`MinIO`).
    pub s3_path_style: bool,
    /// Max age of buffered rows before a commit.
    #[serde(with = "humantime_serde")]
    pub flush_interval: Duration,
    /// Commit a table once this many rows are buffered for it.
    pub flush_rows: usize,
    /// Bound on buffered rows (all tables, in flight included). Beyond it durable
    /// appends fail fast (the lag stays in the WAL) and telemetry is dropped.
    pub max_buffer_rows: usize,
    /// Bound on one commit and one connect attempt: a durable append not
    /// committed within `flush_interval` + this fails; its cursor stays.
    #[serde(with = "humantime_serde")]
    pub commit_timeout: Duration,
    /// Cold cursor of a repository that predates the catalog: `false` = start at
    /// its head, `true` = its retained log start (full history).
    pub backfill: bool,
    /// Create the namespace and tables when missing; `false` = fail instead.
    pub create_tables: bool,
}

/// `[catalog] auth` (D63).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogAuth {
    /// No `Authorization` header (the `iceberg-rest` fixture).
    #[default]
    None,
    /// `Authorization: Bearer`, from `token_env` or the `OAuth2` client
    /// credentials in `credential_env` (exactly one of them).
    Bearer,
    /// AWS Signature V4 on every catalog request (`sigv4_service`,
    /// `sigv4_region`), with the data-file credentials (`s3_*_env`, D43).
    Sigv4,
}

impl CatalogConfig {
    /// The region `SigV4` signs for: `sigv4_region`, else `s3_region`.
    pub fn sigv4_region(&self) -> &str {
        self.sigv4_region
            .as_deref()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or(&self.s3_region)
    }
}

impl Default for CatalogConfig {
    fn default() -> Self {
        CatalogConfig {
            enabled: false,
            uri: None,
            warehouse: None,
            namespace: "floe".into(),
            auth: CatalogAuth::None,
            sigv4_service: "s3".into(),
            sigv4_region: None,
            token_env: None,
            credential_env: None,
            s3_endpoint: None,
            s3_region: "us-east-1".into(),
            s3_access_key_env: "AWS_ACCESS_KEY_ID".into(),
            s3_secret_key_env: "AWS_SECRET_ACCESS_KEY".into(),
            s3_path_style: true,
            flush_interval: Duration::from_secs(30),
            flush_rows: 5000,
            max_buffer_rows: 100_000,
            commit_timeout: Duration::from_mins(1),
            backfill: false,
            create_tables: true,
        }
    }
}

impl GithubMirrorConfig {
    /// The section's own checks (D60: fleet-wide, no host-role rule; the loop
    /// runs on `maintain` hosts). `floe github sync` runs them too.
    pub fn check(&self) -> Result<()> {
        for (key, list) in [("include", &self.include), ("exclude", &self.exclude)] {
            for g in list {
                anyhow::ensure!(
                    g.matches('/').count() == 1 && !g.starts_with('/') && !g.ends_with('/'),
                    "github_mirror.{key} entry {g:?} must be an owner/name glob with exactly one '/' (`*/*`, `acme/*`, `*/name`)"
                );
            }
        }
        for r in &self.repos {
            anyhow::ensure!(
                r.matches('/').count() == 1 && !r.contains('*') && !r.starts_with('/') && !r.ends_with('/'),
                "github_mirror.repos entry {r:?} must be \"owner/name\""
            );
        }
        // The token travels to both: https only, as for `upstream.git`
        // (plain http only to the loopback, for tests).
        for (key, u) in [("api_url", &self.api_url), ("git_url", &self.git_url)] {
            anyhow::ensure!(
                (u.starts_with("https://")
                    || u.starts_with("http://localhost")
                    || u.starts_with("http://127.0.0.1"))
                    && !u.ends_with('/'),
                "github_mirror.{key} must be an https:// URL without a trailing '/', got {u:?}"
            );
        }
        match &self.token {
            Secret::Env(name) => anyhow::ensure!(
                !name.trim().is_empty(),
                "github_mirror.token must name an environment variable ({{ env = \"FLOE_GITHUB_TOKEN\" }}) or hold a value"
            ),
            Secret::Value(v) => anyhow::ensure!(
                !v.trim().is_empty(),
                "github_mirror.token: an empty value; enter the token or use an env reference"
            ),
            Secret::Sealed(_) | Secret::Redacted(_) => {}
        }
        anyhow::ensure!(
            !self.follow.is_empty(),
            "github_mirror.follow must list at least one ref pattern"
        );
        refpattern::RefPatterns::parse(&self.follow)?;
        anyhow::ensure!(
            !self.lease_ttl.is_zero(),
            "github_mirror.lease_ttl must be > 0"
        );
        if self.include_private {
            anyhow::ensure!(
                self.private_visible_to_all_readers,
                "github_mirror.include_private: floe has no per-repository read ACL, so every reader would read every mirrored private repository; set github_mirror.private_visible_to_all_readers = true to accept that, or include_private = false"
            );
        }
        Ok(())
    }
}

impl CatalogConfig {
    /// The section's own checks.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let set = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
        anyhow::ensure!(
            set(&self.uri),
            "catalog.uri must be set when catalog.enabled"
        );
        anyhow::ensure!(
            set(&self.warehouse),
            "catalog.warehouse must be set when catalog.enabled"
        );
        anyhow::ensure!(
            !self.namespace.trim().is_empty(),
            "catalog.namespace must not be empty"
        );
        self.validate_auth()?;
        anyhow::ensure!(self.flush_rows > 0, "catalog.flush_rows must be > 0");
        anyhow::ensure!(
            self.max_buffer_rows >= self.flush_rows,
            "catalog.max_buffer_rows ({}) must be >= catalog.flush_rows ({})",
            self.max_buffer_rows,
            self.flush_rows
        );
        anyhow::ensure!(
            !self.flush_interval.is_zero() && !self.commit_timeout.is_zero(),
            "catalog.flush_interval and catalog.commit_timeout must be > 0"
        );
        Ok(())
    }

    /// `auth` and the keys it reads, fail closed: a key the mode does not use
    /// is an error rather than silently ignored.
    fn validate_auth(&self) -> Result<()> {
        let set = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
        let uri = self.uri.as_deref().unwrap_or_default();
        let (scheme_ok, rest) = match uri.split_once("://") {
            Some(("http" | "https", rest)) => (true, rest),
            _ => (false, ""),
        };
        anyhow::ensure!(
            scheme_ok && !rest.is_empty() && !rest.contains(['@', '?', '#']),
            "catalog.uri must be an http(s) URL without credentials, a query or a fragment (got {uri:?})"
        );
        match self.auth {
            CatalogAuth::None => anyhow::ensure!(
                !set(&self.token_env) && !set(&self.credential_env),
                "catalog.token_env / catalog.credential_env need catalog.auth = \"bearer\""
            ),
            CatalogAuth::Bearer => anyhow::ensure!(
                set(&self.token_env) != set(&self.credential_env),
                "catalog.auth = \"bearer\" needs exactly one of catalog.token_env and catalog.credential_env"
            ),
            CatalogAuth::Sigv4 => {
                anyhow::ensure!(
                    !set(&self.token_env) && !set(&self.credential_env),
                    "catalog.auth = \"sigv4\" signs with the s3_*_env credentials; unset catalog.token_env and catalog.credential_env"
                );
                let name_ok = |v: &str| {
                    !v.is_empty()
                        && v.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                };
                anyhow::ensure!(
                    name_ok(&self.sigv4_service),
                    "catalog.sigv4_service must be a signing name like \"s3\" or \"s3tables\" (got {:?})",
                    self.sigv4_service
                );
                anyhow::ensure!(
                    name_ok(self.sigv4_region()),
                    "catalog.sigv4_region (or catalog.s3_region) must be a region like \"us-east-1\" (got {:?})",
                    self.sigv4_region()
                );
                anyhow::ensure!(
                    !self.s3_access_key_env.trim().is_empty()
                        && !self.s3_secret_key_env.trim().is_empty(),
                    "catalog.s3_access_key_env and catalog.s3_secret_key_env must name env vars"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TelemetryConfig {
    /// `json` (Cloud Logging) or `pretty`.
    pub log_format: LogFormat,
    pub log_filter: String,
    /// Prometheus scrape endpoint on the main listener (`/metrics`).
    pub metrics: bool,
    /// GCP project id for Cloud Logging trace correlation.
    /// Falls back to env `GOOGLE_CLOUD_PROJECT` then the metadata server.
    pub trace_project: Option<String>,
    /// A wait on a per-repository lock or a store permit on a request path (`RepoHandle::rw`,
    /// `sync_mutex`, `pack_mutex`, the GCS bulk permits) longer than this is logged as a WARN
    /// `lock wait` line with `lock`, `repo`, `wait_ms` (+ the request's id from the span); every
    /// wait that was not immediately satisfied lands in `floe_lock_wait_seconds{lock}`. D19's
    /// incident (a queued writer starving readers for 60–680 s) is what this makes visible.
    #[serde(with = "humantime_serde")]
    pub lock_wait_warn: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

fn default_min_commits() -> u64 {
    25
}
fn default_true() -> bool {
    true
}

/// D24: the top-level sections a repository's settings may override.
pub const SETTINGS_SECTIONS: &[&str] = &["bundles", "maintenance", "compaction", "upstream"];
/// The settings document's one section that is not configuration: `[repo]`,
/// the repository's own metadata ([`RepoMeta`]). Never merged into a [`Config`].
pub const REPO_META_SECTION: &str = "repo";
/// Longest `repo.description`, in characters.
pub const DESCRIPTION_MAX_CHARS: usize = 512;

/// `[repo]` in a repository's settings document: metadata about the
/// repository itself, shown by the API (`overview.description`) and the web UI.
/// The GitHub mirror (D49) sets `description` to `Mirror of <url>` and keeps it
/// current. Rides on the same CAS'd, WAL-logged SETTINGS entry as the overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoMeta {
    /// One line of free text (no control characters), at most
    /// [`DESCRIPTION_MAX_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl RepoMeta {
    /// The `[repo]` table of a settings document (default when absent).
    pub fn from_settings(settings_toml: &str) -> Result<RepoMeta> {
        if settings_toml.trim().is_empty() {
            return Ok(RepoMeta::default());
        }
        let mut doc: toml::Table = settings_toml.parse().context("settings: parsing TOML")?;
        Self::from_value(doc.remove(REPO_META_SECTION))
    }

    fn from_value(v: Option<toml::Value>) -> Result<RepoMeta> {
        let Some(v) = v else {
            return Ok(RepoMeta::default());
        };
        let meta: RepoMeta = v.try_into().context("settings: [repo]")?;
        if let Some(d) = &meta.description {
            anyhow::ensure!(
                d.chars().count() <= DESCRIPTION_MAX_CHARS,
                "settings: repo.description is longer than {DESCRIPTION_MAX_CHARS} characters"
            );
            anyhow::ensure!(
                !d.chars().any(char::is_control),
                "settings: repo.description must be one line without control characters"
            );
        }
        Ok(meta)
    }
}
/// D24: maximum size of a settings document.
pub const SETTINGS_MAX_BYTES: usize = 16 * 1024;

impl Config {
    /// D24: the effective configuration for one repository = this (the host's
    /// floe.toml ⊕ env) with the repository's settings TOML merged on top.
    /// Only [`SETTINGS_SECTIONS`] may appear; the result is validated like a
    /// config file. Empty settings = `self` unchanged.
    pub fn with_settings(&self, settings_toml: &str) -> Result<Config> {
        fn merge(into: &mut toml::Table, from: &toml::Table) {
            for (k, v) in from {
                match (into.get_mut(k), v) {
                    (Some(toml::Value::Table(a)), toml::Value::Table(b)) => merge(a, b),
                    _ => {
                        into.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if settings_toml.trim().is_empty() {
            return Ok(self.clone());
        }
        anyhow::ensure!(
            settings_toml.len() <= SETTINGS_MAX_BYTES,
            "settings document larger than {SETTINGS_MAX_BYTES} bytes"
        );
        let mut overrides: toml::Table = settings_toml.parse().context("settings: parsing TOML")?;
        // `[repo]` is metadata, not configuration: checked, never merged.
        RepoMeta::from_value(overrides.remove(REPO_META_SECTION))?;
        for k in overrides.keys() {
            anyhow::ensure!(
                SETTINGS_SECTIONS.contains(&k.as_str()),
                "settings: section [{k}] may not be set per repository (allowed: {}, and [{REPO_META_SECTION}])",
                SETTINGS_SECTIONS.join(", ")
            );
        }
        if let Some(toml::Value::Table(u)) = overrides.get("upstream") {
            for key in ["token_env", "token_env_by_host"] {
                anyhow::ensure!(
                    !u.contains_key(key),
                    "settings: upstream.{key} is host-only (it names env vars on the maintaining host)"
                );
            }
        }
        let mut doc: toml::Table = toml::Table::try_from(self).context("serializing config")?;
        merge(&mut doc, &overrides);
        let mut cfg: Config = doc.try_into().context("settings: applying")?;
        // Load-time state, not TOML: carried over the round trip.
        cfg.github_mirror.use_token = self.github_mirror.use_token;
        cfg.github_mirror.token_env.clone_from(&self.github_mirror.token_env);
        cfg.validate()
            .context("settings: validating the effective config")?;
        Ok(cfg)
    }

    /// The effective config a reader may see: only [`SETTINGS_SECTIONS`], and
    /// never `upstream.token_env` / `token_env_by_host` (those names are host-only).
    pub fn public_settings_toml(&self) -> Result<String> {
        let mut doc: toml::Table = toml::Table::try_from(self).context("serializing config")?;
        doc.retain(|k, _| SETTINGS_SECTIONS.contains(&k));
        if let Some(toml::Value::Table(u)) = doc.get_mut("upstream") {
            u.remove("token_env");
            u.remove("token_env_by_host");
        }
        toml::to_string_pretty(&doc).context("encoding settings")
    }

    /// The env var naming the token for an upstream URL: `upstream.token_env_by_host`
    /// for the URL's host (`host:port` first, then the bare host), else the
    /// mirror's token for a mirror-managed repository on the mirror's host
    /// (D49), else `upstream.token_env`, else none (unauthenticated). The one place every
    /// upstream caller (follow, LFS read-through, `repair`) resolves it.
    ///
    /// The authority ends at the first `/`, `?` or `#`, as git's and curl's URL
    /// parsers end it. A URL with userinfo gets no token at all: `validate` refuses
    /// one, and git and curl disagree on which `@` ends it, so no host-scoped token
    /// could be sent to the host it is scoped to with certainty.
    pub fn upstream_token_env(&self, url: &str) -> Option<&str> {
        let authority = upstream_authority(url);
        if authority.contains('@') {
            return None;
        }
        let host = authority.split(':').next().unwrap_or_default();
        let by_host = &self.upstream.token_env_by_host;
        by_host
            .get(authority)
            .or_else(|| by_host.get(host))
            .or_else(|| self.mirror_token_env(authority, host))
            .or(self.upstream.token_env.as_ref())
            .map(String::as_str)
    }

    /// D25: the effective on-disk budget — `cache.max_bytes` in budget mode,
    /// **0 = unlimited** in disk mode (the SSD host: `maintenance.disk = "ssd"` or
    /// `cache.mode = "disk"`). Every fits/plan decision goes through this.
    pub fn cache_budget_bytes(&self) -> u64 {
        if self.cache_is_disk() {
            0
        } else {
            self.cache.max_bytes.as_u64()
        }
    }
    /// Whether the cache is a real disk (no budget; watermark eviction).
    pub fn cache_is_disk(&self) -> bool {
        match self.cache.mode {
            CacheMode::Disk => true,
            CacheMode::Budget => false,
            CacheMode::Auto => self.maintenance.disk == MaintainerDisk::Ssd,
        }
    }
    /// Bundle strategies form chains of calendar slots (`docs/BUNDLE_URI_DESIGN.md` §4):
    /// every `schedule` is a 6-field UTC cron (or an `@alias`) that parses; an
    /// incremental names a `base` that exists and whose chain ends in a full
    /// strategy; each chain has exactly one full root; `keep >= 1` on fulls.
    fn validate_bundle_strategies(&self) -> Result<()> {
        use std::collections::HashMap;
        let strategies = &self.bundles.strategy;
        let by_name: HashMap<&str, &BundleStrategy> =
            strategies.iter().map(|s| (s.name.as_str(), s)).collect();
        anyhow::ensure!(
            by_name.len() == strategies.len(),
            "bundles.strategy: duplicate strategy names"
        );
        for s in strategies {
            let expr = s.schedule.trim();
            let fields = expr.split_whitespace().count();
            anyhow::ensure!(
                expr.starts_with('@') || fields == 6 || fields == 7,
                "bundles.strategy {}: schedule {:?} must be a 6-field UTC cron (sec min hour dom mon dow) or @hourly/@daily/@weekly",
                s.name,
                s.schedule
            );
            cron::Schedule::from_str(expr).map_err(|e| {
                anyhow::anyhow!(
                    "bundles.strategy {}: schedule {:?} does not parse: {e}",
                    s.name,
                    s.schedule
                )
            })?;
            if let Some(f) = &s.filter {
                anyhow::ensure!(
                    f == "blob:none",
                    "bundles.strategy {}: filter {f:?} is not supported (only \"blob:none\")",
                    s.name
                );
            }
            match s.kind {
                BundleKind::Full => {
                    anyhow::ensure!(
                        s.base.is_none(),
                        "bundles.strategy {}: a full strategy has no base",
                        s.name
                    );
                    anyhow::ensure!(
                        !s.chain,
                        "bundles.strategy {}: `chain` is an incremental knob",
                        s.name
                    );
                    anyhow::ensure!(
                        s.keep >= 1,
                        "bundles.strategy {}: keep must be >= 1 on a full strategy",
                        s.name
                    );
                }
                BundleKind::Incremental => {
                    anyhow::ensure!(
                        s.keep == 0,
                        "bundles.strategy {}: `keep` is not a knob on an incremental strategy — the 2 newest whose base is kept are always listed (D21, 2026-08-22); remove it",
                        s.name
                    );
                    let mut cur = s;
                    let mut hops = 0;
                    loop {
                        let base = cur.base.as_deref().ok_or_else(|| {
                            anyhow::anyhow!("bundles.strategy {}: incremental needs base", cur.name)
                        })?;
                        let b = by_name.get(base).ok_or_else(|| {
                            anyhow::anyhow!(
                                "bundles.strategy {}: base {base} is not a strategy",
                                cur.name
                            )
                        })?;
                        anyhow::ensure!(
                            b.filter == s.filter,
                            "bundles.strategy {}: filter {:?} differs from its base {}'s {:?} (a chain shares one filter)",
                            s.name,
                            s.filter,
                            b.name,
                            b.filter
                        );
                        if b.kind == BundleKind::Full {
                            break;
                        }
                        cur = b;
                        hops += 1;
                        anyhow::ensure!(
                            hops < 16,
                            "bundles.strategy {}: base chain has a cycle",
                            s.name
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

/// `owner/name` matches an entry of `list` (`owner/name`, `owner/*`, `*`; `.git` tolerated).
/// The `placement` table as the env overrides alone set it (None when no
/// `FLOE__PLACEMENT__*` variable was present).
fn env_placement_overrides(doc: &toml::Table, vars_seen: &[String]) -> Option<toml::Value> {
    let keys: Vec<String> = vars_seen
        .iter()
        .filter_map(|k| k.strip_prefix("FLOE__PLACEMENT__"))
        .map(str::to_ascii_lowercase)
        .collect();
    if keys.is_empty() {
        return None;
    }
    let table = doc.get("placement")?.as_table()?;
    let mut out = toml::Table::new();
    for k in keys {
        if let Some(v) = table.get(&k) {
            out.insert(k, v.clone());
        }
    }
    Some(toml::Value::Table(out))
}

pub fn repo_listed(list: &[String], owner: &str, name: &str) -> bool {
    list.iter().any(|r| {
        let r = r.trim().trim_end_matches(".git");
        r == format!("{owner}/{name}") || r == format!("{owner}/*") || r == "*"
    })
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            http2: true,
            max_concurrent_requests: 512,
            max_concurrent_per_repo: 64,
            request_timeout: Duration::from_hours(1),
            drain_timeout: Duration::from_secs(20),
            max_push_bytes: ByteSize::gib(64),
            roles: vec![],
            auth: AuthConfig::default(),
            public_url: None,
            auto_create_on_push: false,
            accel_redirect: false,
            cors_origins: vec![],
            tls: TlsConfig::default(),
        }
    }
}
impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            mode: AuthMode::None,
            anonymous_read: true,
            tokens: vec![],
            issuer: "https://accounts.google.com".into(),
            allowed_domains: vec![],
            allowed_emails: vec![],
            audiences: vec![],
            write_domains: None,
            trusted_forwarders: vec![],
            admin_emails: vec![],
            admin_domains: vec![],
            session_secret: None,
            session_ttl: Duration::from_hours(30 * 24),
            access_token_ttl: Duration::from_hours(90 * 24),
            oauth_client_id: None,
            oauth_client_secret: None,
        }
    }
}
impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            backend: StoreBackend::Gcs,
            bucket: "floe".into(),
            prefix: String::new(),
            gcs: GcsConfig::default(),
            s3: S3Config::default(),
            max_retries: 8,
            multipart_threshold: ByteSize::mib(64),
            multipart_part_size: ByteSize::mib(32),
        }
    }
}
impl Default for GcsConfig {
    fn default() -> Self {
        GcsConfig {
            endpoint: "https://storage.googleapis.com".into(),
            direct_connectivity: true,
            signing_service_account: None,
            bulk_clients: 4,
            bulk_concurrency: 32,
        }
    }
}
impl Default for S3Config {
    fn default() -> Self {
        S3Config {
            endpoint: "http://127.0.0.1:9000".into(),
            region: "us-east-1".into(),
            access_key_env: "AWS_ACCESS_KEY_ID".into(),
            secret_key_env: "AWS_SECRET_ACCESS_KEY".into(),
            force_path_style: true,
        }
    }
}
impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            dir: PathBuf::from("/tmp/floe"),
            mode: CacheMode::Auto,
            max_bytes: ByteSize::gib(20),
            disk_high_watermark: 0.9,
            evict_idle_after: Duration::from_hours(6),
            prewarm: vec![],
            prewarm_parallelism: 2,
            prewarm_ready_timeout: Duration::ZERO,
            ref_advert_entries: 256,
            object_info_entries: 4096,
            bundle_list_entries: 128,
            remote_block_bytes: ByteSize::gib(1),
            remote_object_bytes: ByteSize::mib(256),
            shared_render_cache: true,
            store_mount: None,
        }
    }
}
impl Default for WalConfig {
    fn default() -> Self {
        WalConfig {
            batch_window: Duration::from_millis(5),
            max_batch: 64,
            push_broker_url: None,
            push_broker_token: None,
            push_broker_buffer_bytes: ByteSize::mib(64),
            snapshot_every_entries: 256,
            checkpoint_interval: Duration::from_hours(1),
            checkpoint_tail_bytes: ByteSize::mib(8),
            cas_max_retries: 16,
            fsck_objects: true,
            check_connectivity: true,
            freshness_ttl: Duration::ZERO,
            prefetch_packs: true,
            prefetch_max_bytes: ByteSize::gib(1),
            remote_objects: true,
        }
    }
}
impl Default for CompactionConfig {
    fn default() -> Self {
        CompactionConfig {
            enabled: true,
            factor: 2,
            trigger_packs: 16,
            trigger_bytes: ByteSize::gib(1),
            lease_ttl: Duration::from_mins(10),
            retention_superseded: Duration::from_hours(7 * 24),
            engine: RepackEngine::Git,
        }
    }
}
impl Default for BundlesConfig {
    fn default() -> Self {
        BundlesConfig {
            enabled: true,
            strategy: vec![
                BundleStrategy {
                    name: "weekly".into(),
                    kind: BundleKind::Full,
                    // Sunday 23:00 UTC (slot = fire time; backfilled when missed).
                    schedule: "0 0 23 * * Sun".into(),
                    base: None,
                    keep: 2,
                    refs: vec![],
                    backfill_max: 0,
                    min_commits: None,
                    filter: None,
                    chain: false,
                },
                BundleStrategy {
                    name: "daily".into(),
                    kind: BundleKind::Incremental,
                    schedule: "0 0 23 * * *".into(),
                    base: Some("weekly".into()),
                    keep: 0,
                    refs: vec![],
                    backfill_max: 0,
                    min_commits: None,
                    filter: None,
                    chain: true,
                },
                BundleStrategy {
                    name: "hourly".into(),
                    kind: BundleKind::Incremental,
                    schedule: "@hourly".into(),
                    base: Some("daily".into()),
                    keep: 0,
                    refs: vec![],
                    backfill_max: 0,
                    min_commits: None,
                    filter: None,
                    chain: false,
                },
            ],
            serve_via: BundleServe::Proxy,
            signed_url_ttl: Duration::from_hours(1),
            advertise: true,
            advertise_filtered: false,
            require: Vec::new(),
            signed_url_for: Vec::new(),
            main_only: true,
            extra_refs: Vec::new(),
            min_commits: 25,
            min_bytes: ByteSize::b(0),
        }
    }
}
impl Default for LfsConfig {
    fn default() -> Self {
        LfsConfig {
            enabled: true,
            serve_via: BundleServe::Proxy,
            signed_url_ttl: Duration::from_hours(1),
            max_object_bytes: ByteSize::gib(16),
        }
    }
}
impl Default for GitConfig {
    fn default() -> Self {
        GitConfig {
            binary: PathBuf::from("git"),
            upload_pack_engine: UploadPackEngine::Auto,
            allow_filter: true,
            allow_any_sha1_in_want: false,
            object_format: ObjectFormat::Sha1,
            commit_graph: true,
            commit_graph_changed_paths: false,
            history_pack: true,
            max_wants: 0,
        }
    }
}
impl Default for TelemetryConfig {
    fn default() -> Self {
        TelemetryConfig {
            log_format: LogFormat::Json,
            log_filter: "info,floe=debug".into(),
            metrics: true,
            trace_project: None,
            lock_wait_warn: Duration::from_secs(1),
        }
    }
}

impl Config {
    pub fn parse(toml_text: &str) -> Result<Config> {
        let table: toml::Table = toml_text.parse().context("parsing floe.toml")?;
        for section in runtime::RUNTIME_SECTIONS {
            anyhow::ensure!(
                !table.contains_key(*section),
                "floe.toml: [{section}] is runtime configuration and lives in the config store (D60); publish it with `floe config import <this file>` (or the admin GUI at /_admin) and delete the section"
            );
        }
        let mut cfg: Config = table.try_into().context("parsing floe.toml")?;
        cfg.apply_env(std::env::vars())?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &std::path::Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg = Self::parse(&text)?;
        cfg.derive_mirror_token_env(|k| secret::env_var(k).is_some());
        Ok(cfg)
    }

    /// D49: follow and LFS read-through on the mirror's repositories authenticate
    /// with the mirror's token ([`GithubMirrorConfig::use_token`]) when the mirror
    /// is enabled here **or** the variable is set in this process (a serving-only
    /// host sharing the fleet's floe.toml). Only repositories the mirror manages
    /// (`upstream.source = "github:…"`) get it: every other upstream on that host,
    /// own repositories included, resolves exactly as before.
    pub fn derive_mirror_token_env(&mut self, is_set: impl Fn(&str) -> bool) {
        let m = &self.github_mirror;
        let on = !m.token_env.is_empty() && (m.enabled || is_set(&m.token_env));
        self.github_mirror.use_token = on;
    }

    /// The mirror's token env var for an upstream on `authority` (`host` without
    /// the port), when this effective config is a mirror-managed repository's.
    fn mirror_token_env(&self, authority: &str, host: &str) -> Option<&String> {
        let m = &self.github_mirror;
        let managed = self
            .upstream
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("github:"));
        let mirror_host = upstream_authority(&m.git_url);
        (m.use_token
            && managed
            && !m.token_env.is_empty()
            && !mirror_host.is_empty()
            && (mirror_host == authority || mirror_host == host))
            .then_some(&m.token_env)
    }

    /// Apply `FLOE__a__b=v` overrides (values parsed as TOML values, falling back to string)
    /// and a serverless host's `PORT`.
    ///
    /// Config and image are released independently (the ssd-host host follows
    /// the serving image's version): an override for a key **unknown to this build**
    /// (or with an unparsable value) is **ignored with a WARN**, never a
    /// startup failure (2026-08-21: `unknown field disk_high_watermark`
    /// crash-looped a host running the previous image). The ignored keys are
    /// returned by [`apply_env_report`].
    pub fn apply_env(&mut self, vars: impl Iterator<Item = (String, String)>) -> Result<()> {
        let ignored = self.apply_env_report(vars)?;
        for (k, why) in &ignored {
            tracing::warn!(key = %k, reason = %why, "ignoring {k}: unknown in this build");
        }
        Ok(())
    }

    /// [`apply_env`] returning the `(key, reason)` pairs it had to ignore.
    pub fn apply_env_report(
        &mut self,
        vars: impl Iterator<Item = (String, String)>,
    ) -> Result<Vec<(String, String)>> {
        let mut vars_seen: Vec<String> = Vec::new();
        let mut doc: toml::Table = toml::Table::try_from(&*self).context("serializing config")?;
        let mut touched = false;
        let mut ignored = Vec::new();
        let mut port_override = None;
        for (k, v) in vars {
            if k == "PORT" {
                port_override = v.parse::<u16>().ok();
                continue;
            }
            let Some(rest) = k.strip_prefix("FLOE__") else {
                continue;
            };
            vars_seen.push(k.clone());
            let path: Vec<String> = rest.split("__").map(str::to_ascii_lowercase).collect();
            if path.is_empty() || path.iter().any(String::is_empty) {
                continue;
            }
            // D60: one home per key — runtime keys live in the config store only.
            if path
                .first()
                .is_some_and(|h| runtime::RUNTIME_SECTIONS.contains(&h.as_str()))
            {
                ignored.push((
                    k,
                    "runtime key: it lives in the config store (D60; `floe config set` or /_admin)".to_string(),
                ));
                continue;
            }
            let value: toml::Value = v
                .parse::<toml::Value>()
                .unwrap_or(toml::Value::String(v.clone()));
            // Apply into a copy and type-check it alone: a bad/unknown key is
            // dropped (WARN) instead of failing every other override with it.
            let mut trial = doc.clone();
            let bad = {
                fn set(
                    cur: &mut toml::Table,
                    path: &[String],
                    value: toml::Value,
                ) -> std::result::Result<(), String> {
                    let Some((head, rest)) = path.split_first() else {
                        return Ok(());
                    };
                    if rest.is_empty() {
                        cur.insert(head.clone(), value);
                        return Ok(());
                    }
                    let next = cur
                        .entry(head.clone())
                        .or_insert_with(|| toml::Value::Table(toml::Table::default()))
                        .as_table_mut()
                        .ok_or_else(|| format!("{head} is not a table"))?;
                    set(next, rest, value)
                }
                match set(&mut trial, &path, value) {
                    Err(why) => Some(why),
                    Ok(()) => trial.clone().try_into::<Config>().err().map(|e| {
                        e.to_string()
                            .lines()
                            .next()
                            .unwrap_or("invalid")
                            .to_string()
                    }),
                }
            };
            if let Some(why) = bad {
                ignored.push((k, why));
            } else {
                doc = trial;
                touched = true;
            }
        }
        // `[placement]` is a host fact set as a GROUP: any FLOE__PLACEMENT__* override
        // replaces the whole section (unset keys = the section's defaults), never
        // merges with the baked file's. 2026-08-21 07:00Z: the image's toml carried
        // the serverless host shape (serve_exclude = ["acme/monorepo"]); the SSD host's env set only
        // MAINTAIN* and inherited the exclude — it refused its own repo for 12 min.
        if let Some(toml::Value::Table(env_placement)) = env_placement_overrides(&doc, &vars_seen) {
            let mut fresh: toml::Table =
                toml::Table::try_from(PlacementConfig::default()).context("placement defaults")?;
            for (k, v) in env_placement {
                fresh.insert(k, v);
            }
            doc.insert("placement".into(), toml::Value::Table(fresh));
            touched = true;
        }
        if touched {
            *self = doc.try_into().context("applying FLOE__ env overrides")?;
        }
        if let Some(port) = port_override {
            self.server.listen.set_port(port);
            // Standalone / `dev server`: public_url is the origin the browser hits. Keep its
            // port in lockstep with PORT. A real public_url is left alone.
            if let Some(u) = self.server.public_url.as_mut()
                && origin_is_loopback(u)
            {
                *u = rewrite_origin_port(u, port);
            }
        }
        Ok(ignored)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.store.bucket.is_empty(), "store.bucket must be set");
        self.catalog.validate()?;
        self.server.tls.validate()?;
        if let Some(u) = &self.server.public_url {
            anyhow::ensure!(
                u.starts_with("https://") || u.starts_with("http://"),
                "server.public_url must be an http(s) origin (got {u})"
            );
        }
        self.validate_bundle_strategies()?;
        // You cannot maintain what you refuse to serve: a host with the serve role
        // whose maintain rules name a repository its serve rules exclude is a config
        // error (the SSD host 2026-08-21 07:00Z: maintain = ["acme/monorepo"], inherited
        // serve_exclude = ["acme/monorepo"] → refused its own repo). Checked on the
        // literal names in `maintain` (globs are not expanded).
        if self.has_role(Role::Serve) {
            for m in &self.placement.maintain {
                if let Some((o, n)) = m.split_once('/')
                    && !n.contains('*')
                    && !o.contains('*')
                    && !self.placement.serves(o, n)
                    && self.placement.maintains(o, n)
                {
                    anyhow::bail!(
                        "placement: this host maintains {m} but its serve rules exclude it (serve = {:?}, serve_exclude = {:?}) — you cannot maintain what you refuse to serve",
                        self.placement.serve,
                        self.placement.serve_exclude
                    );
                }
            }
        }
        for (key, u) in [
            ("upstream.git", &self.upstream.git),
            ("upstream.lfs", &self.upstream.lfs),
        ] {
            if let Some(u) = u {
                anyhow::ensure!(
                    u.starts_with("https://")
                        || u.starts_with("http://localhost")
                        || u.starts_with("http://127.0.0.1"),
                    "{key} must be an https:// URL (got {u})"
                );
                anyhow::ensure!(!u.ends_with('/'), "{key} must not end with '/' (got {u})");
                // Credentials in the URL would land in logs, task output, the
                // Settings tab and the WAL entry's meta; tokens go through
                // `token_env` / `token_env_by_host`. `?`/`#` would let the host
                // a token is scoped to differ from the one git connects to.
                anyhow::ensure!(
                    !upstream_authority(u).contains('@')
                        && !u.contains(['?', '#'])
                        && !u.chars().any(char::is_whitespace),
                    "{key} must not carry credentials, a query or a fragment (got {}); put the token in an env var named by upstream.token_env / token_env_by_host",
                    redact_userinfo(u)
                );
            }
        }
        if !self.upstream.follow.is_empty() {
            anyhow::ensure!(
                self.upstream.git.is_some(),
                "upstream.follow needs upstream.git (the host to follow)"
            );
            refpattern::RefPatterns::parse(&self.upstream.follow)?;
        }
        if let Some(h) = &self.upstream.head {
            anyhow::ensure!(
                h.starts_with("refs/heads/")
                    && !h.contains('*')
                    && refpattern::check_refspec_pattern(h).is_ok(),
                "upstream.head must be a branch ref (refs/heads/main), got {h:?}"
            );
        }
        for (host, var) in &self.upstream.token_env_by_host {
            anyhow::ensure!(
                !host.is_empty()
                    && !host.contains('/')
                    && !host.contains('@')
                    && !host.chars().any(char::is_whitespace),
                "upstream.token_env_by_host keys are bare hostnames (github.com), got {host:?}"
            );
            anyhow::ensure!(
                !var.is_empty(),
                "upstream.token_env_by_host.{host:?} names no environment variable"
            );
        }
        for o in &self.server.cors_origins {
            let host = o
                .strip_prefix("https://")
                .or_else(|| o.strip_prefix("http://localhost").map(|_| "localhost"))
                .filter(|h| !h.is_empty() && !h.contains('/'));
            anyhow::ensure!(
                host.is_some()
                    && o.matches('*').count() <= 1
                    && (!o.contains('*') || o.contains("://*.")),
                "server.cors_origins entries must be https origins (or http://localhost[:port]) with at most one leading `*.` wildcard, got {o:?}"
            );
        }
        // The GitHub facade is its own trust boundary and has none: it accepts
        // any credential and is admin on every repository. Mounting it on a
        // host that is not already open to its network is a way to hand the
        // bucket away, so it fails closed the way `auth.mode = none` does.
        // With the facade on, the network the process is bound to *is* the
        // trust boundary (a developer's compose network), so `auth.mode = none`
        // is the only honest mode and the loopback rule below yields to it.
        if self.github.enabled {
            anyhow::ensure!(
                self.server.auth.mode == AuthMode::None,
                "github.enabled is auth-free (any bearer is accepted, every repository is admin): it needs server.auth.mode = \"none\" (auth.mode is {:?})",
                self.server.auth.mode
            );
        }
        // Security contract: identity modes fail closed.
        let a = &self.server.auth;
        if a.mode == AuthMode::None && !self.github.enabled {
            anyhow::ensure!(
                self.server.listen.ip().is_loopback(),
                "server.auth.mode = none is loopback-only (listen is {}); use token or oidc for a public bind",
                self.server.listen
            );
        }
        if a.mode == AuthMode::Token {
            anyhow::ensure!(
                !a.tokens.is_empty(),
                "server.auth.tokens must list at least one token in token mode"
            );
        }
        for t in &a.tokens {
            anyhow::ensure!(
                !t.principal.is_empty(),
                "server.auth.tokens[].principal must not be empty"
            );
            anyhow::ensure!(
                !t.token.is_empty() || t.token_env.as_deref().is_some_and(|v| !v.is_empty()),
                "server.auth.tokens[] for {:?} needs `token` or `token_env`",
                t.principal
            );
        }
        if let Some(secret) = a.session_secret.as_deref().filter(|s| !s.is_empty()) {
            anyhow::ensure!(
                secret.len() >= 32,
                "server.auth.session_secret must be at least 32 bytes when set"
            );
        }
        if a.mode == AuthMode::Oidc {
            anyhow::ensure!(
                !a.anonymous_read,
                "server.auth.anonymous_read must be false in oidc mode"
            );
            anyhow::ensure!(
                !a.allowed_domains.is_empty() || !a.allowed_emails.is_empty(),
                "server.auth.allowed_domains (or allowed_emails) must be set in oidc mode"
            );
            anyhow::ensure!(
                a.issuer.starts_with("https://") && !a.issuer.ends_with('/'),
                "server.auth.issuer must be an https URL without a trailing slash (got {:?})",
                a.issuer
            );
            let oauth_id = a.oauth_client_id.as_deref().is_some_and(|v| !v.is_empty());
            let oauth_secret = a
                .oauth_client_secret
                .as_deref()
                .is_some_and(|v| !v.is_empty());
            anyhow::ensure!(
                oauth_id == oauth_secret,
                "server.auth.oauth_client_id and oauth_client_secret must be set together (browser sign-in) or both left unset"
            );
            anyhow::ensure!(
                oauth_id || !a.audiences.is_empty(),
                "oidc mode needs a way in: server.auth.oauth_client_id/oauth_client_secret (browser sign-in + issued tokens) and/or server.auth.audiences (ID tokens minted by clients)"
            );
            anyhow::ensure!(
                !oauth_id || a.session_secret.as_deref().is_some_and(|s| !s.is_empty()),
                "server.auth.session_secret is required with oauth_client_id (it signs sessions and access tokens)"
            );
        }
        anyhow::ensure!(
            self.compaction.factor >= 2,
            "compaction.factor must be >= 2"
        );
        anyhow::ensure!(self.wal.max_batch >= 1, "wal.max_batch must be >= 1");
        let names: std::collections::HashSet<&str> = self
            .bundles
            .strategy
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        anyhow::ensure!(
            names.len() == self.bundles.strategy.len(),
            "bundle strategy names must be unique"
        );
        for s in &self.bundles.strategy {
            match (s.kind, &s.base) {
                (BundleKind::Incremental, None) => {
                    anyhow::bail!("bundle strategy {} is incremental but has no base", s.name)
                }
                (BundleKind::Incremental, Some(b)) => {
                    anyhow::ensure!(
                        names.contains(b.as_str()),
                        "bundle strategy {} base {b} does not exist",
                        s.name
                    );
                }
                (BundleKind::Full, Some(_)) => {
                    anyhow::bail!("bundle strategy {} is full but has a base", s.name)
                }
                _ => {}
            }
            if matches!(s.kind, BundleKind::Full) {
                anyhow::ensure!(
                    s.keep >= 1,
                    "bundle strategy {}: keep must be >= 1 on a full strategy",
                    s.name
                );
            }
        }
        self.events.check()?;
        // D60: the document is fleet-wide, so there is no host-role rule here;
        // the mirror's loop simply runs on `maintain` hosts.
        if self.github_mirror.enabled {
            self.github_mirror.check()?;
        }
        Ok(())
    }

    /// Store prefix normalized to either "" or "something/".
    pub fn store_prefix(&self) -> String {
        let p = self.store.prefix.trim_matches('/');
        if p.is_empty() {
            String::new()
        } else {
            format!("{p}/")
        }
    }

    /// Whether `owner/name` is listed in `bundles.require` (exact or `owner/*`).
    pub fn bundles_required(&self, owner: &str, name: &str) -> bool {
        repo_listed(&self.bundles.require, owner, name)
    }

    /// How `owner/name`'s bundle URIs are served: `bundles.serve_via`, or
    /// signed URLs when listed in `bundles.signed_url_for`.
    pub fn bundle_serve_via(&self, owner: &str, name: &str) -> BundleServe {
        if repo_listed(&self.bundles.signed_url_for, owner, name) {
            BundleServe::SignedUrl
        } else {
            self.bundles.serve_via
        }
    }

    /// Whether this process terminates TLS itself (D39 standalone shape).
    pub fn tls_enabled(&self) -> bool {
        self.server.tls.mode != TlsMode::Off
    }

    pub fn has_role(&self, role: Role) -> bool {
        self.server.roles.is_empty()
            || self.server.roles.contains(&role)
            || (matches!(role, Role::Compact | Role::Bundle)
                && self.server.roles.contains(&Role::Maintain))
    }
}

fn origin_host(origin: &str) -> &str {
    let rest = origin
        .trim_end_matches('/')
        .split_once("://")
        .map_or(origin, |(_, r)| r);
    if let Some(inside) = rest.strip_prefix('[') {
        return inside.split_once(']').map_or(inside, |(h, _)| h);
    }
    rest.split([':', '/']).next().unwrap_or(rest)
}

fn origin_is_loopback(origin: &str) -> bool {
    let host = origin_host(origin);
    host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1" || host == "::1"
}

fn rewrite_origin_port(origin: &str, port: u16) -> String {
    let origin = origin.trim_end_matches('/');
    let Some((scheme, rest)) = origin.split_once("://") else {
        return origin.to_string();
    };
    let host = if rest.starts_with('[') {
        rest.split_once(']')
            .map_or_else(|| rest.to_string(), |(h, _)| format!("{h}]"))
    } else {
        rest.split([':', '/']).next().unwrap_or(rest).to_string()
    };
    let default = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    if port == default {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    }
}

/// The authority of an upstream URL (`host[:port]`, with any `user@`): after
/// `scheme://`, up to the first `/`, `?` or `#` (as git and curl end it).
pub fn upstream_authority(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or_default()
}

/// `url` with any userinfo replaced by `***` (for error messages).
fn redact_userinfo(url: &str) -> String {
    let authority = upstream_authority(url);
    match authority.rsplit_once('@') {
        Some((_, host)) => url.replacen(authority, &format!("***@{host}"), 1),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_validate() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.server.listen.port(), 8080);
        assert_eq!(c.bundles.strategy.len(), 3);
        c.validate().unwrap();
        // Round trip through TOML (the bootstrap: runtime sections live in the config store, D60).
        let mut doc = toml::Table::try_from(&c).unwrap();
        for section in runtime::RUNTIME_SECTIONS {
            doc.remove(*section);
        }
        let text = toml::to_string(&doc).unwrap();
        let back = Config::parse(&text).unwrap();
        assert_eq!(back.store.bucket, c.store.bucket);
    }

    #[test]
    fn env_overrides() {
        let mut c = Config::default();
        c.apply_env(
            vec![
                ("FLOE__STORE__BACKEND".to_string(), "s3".to_string()),
                (
                    "FLOE__STORE__S3__ENDPOINT".to_string(),
                    "http://rustfs:9000".to_string(),
                ),
                ("FLOE__WAL__MAX_BATCH".to_string(), "7".to_string()),
                ("PORT".to_string(), "9090".to_string()),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(c.store.backend, StoreBackend::S3);
        assert_eq!(c.store.s3.endpoint, "http://rustfs:9000");
        assert_eq!(c.wal.max_batch, 7);
        assert_eq!(c.server.listen.port(), 9090);
    }

    #[test]
    fn port_rewrites_loopback_public_url_only() {
        let mut c = Config::default();
        c.server.public_url = Some("https://floe.localhost:8888".into());
        c.apply_env(vec![("PORT".to_string(), "8080".to_string())].into_iter())
            .unwrap();
        assert_eq!(c.server.listen.port(), 8080);
        assert_eq!(
            c.server.public_url.as_deref(),
            Some("https://floe.localhost:8080")
        );

        let mut prod = Config::default();
        prod.server.public_url = Some("https://git.example.com".into());
        prod.apply_env(vec![("PORT".to_string(), "8080".to_string())].into_iter())
            .unwrap();
        assert_eq!(
            prod.server.public_url.as_deref(),
            Some("https://git.example.com")
        );
    }

    /// `[placement]` is set as a group: one PLACEMENT env key replaces the whole
    /// section (unset keys = defaults), never merges with the file's values.
    /// The SSD host 2026-08-21 07:00Z: the baked toml's `serve_exclude = ["acme/monorepo"]`
    /// leaked under an env that set only `MAINTAIN*` → the host refused its own repo.
    #[test]
    fn env_placement_override_replaces_the_whole_section() {
        let mut c = Config::default();
        c.placement.serve_exclude = vec!["acme/monorepo".into()]; // the baked a serverless host shape
        c.placement.maintain_exclude = vec!["acme/monorepo".into()];
        c.apply_env(
            vec![(
                "FLOE__PLACEMENT__MAINTAIN".to_string(),
                "[\"acme/monorepo\"]".to_string(),
            )]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(c.placement.maintain, vec!["acme/monorepo"]);
        assert!(
            c.placement.serve_exclude.is_empty(),
            "the file's exclude must not survive an env group override: {:?}",
            c.placement.serve_exclude
        );
        assert!(c.placement.maintain_exclude.is_empty());
        assert_eq!(c.placement.serve, vec!["*"]);
        assert!(c.placement.serves("acme", "monorepo"));
        // No PLACEMENT env at all: the file's section stands.
        let mut c = Config::default();
        c.placement.serve_exclude = vec!["acme/monorepo".into()];
        c.apply_env(vec![("FLOE__WAL__MAX_BATCH".to_string(), "7".to_string())].into_iter())
            .unwrap();
        assert_eq!(c.placement.serve_exclude, vec!["acme/monorepo"]);
    }

    /// You cannot maintain what you refuse to serve.
    #[test]
    fn validate_refuses_maintaining_a_repo_the_host_does_not_serve() {
        let mut c = Config::default();
        c.store.bucket = "b".into();
        c.server.roles = vec![Role::Serve, Role::Maintain];
        c.placement.maintain = vec!["acme/monorepo".into()];
        c.placement.serve_exclude = vec!["acme/monorepo".into()];
        let err = c.validate().unwrap_err().to_string();
        assert!(
            err.contains("cannot maintain what you refuse to serve"),
            "{err}"
        );
        // A maintain-only host may well refuse to serve (it is not a front).
        c.server.roles = vec![Role::Maintain];
        c.validate().unwrap();
        // And the prod shapes are fine: broker excludes a large repository from both; the SSD host serves + maintains it.
        c.server.roles = vec![Role::Serve, Role::Maintain];
        c.placement.maintain = vec!["*".into()];
        c.placement.maintain_exclude = vec!["acme/monorepo".into()];
        c.validate().unwrap();
        c.placement = PlacementConfig {
            serve: vec!["acme/monorepo".into()],
            serve_exclude: vec![],
            maintain: vec!["acme/monorepo".into()],
            maintain_exclude: vec![],
        };
        c.validate().unwrap();
    }

    /// Config and image release independently: an override for a key this
    /// build does not know (or a value it cannot parse) is ignored and
    /// reported, the known ones still apply, startup continues.
    #[test]
    fn env_override_unknown_key_is_ignored_not_fatal() {
        let mut c = Config::default();
        let ignored = c
            .apply_env_report(
                vec![
                    ("FLOE__CACHE__NOT_A_KEY_YET".to_string(), "0.9".to_string()),
                    (
                        "FLOE__WAL__MAX_BATCH".to_string(),
                        "not-a-number".to_string(),
                    ),
                    ("FLOE__NOSUCHSECTION__X".to_string(), "1".to_string()),
                    ("FLOE__WAL__BATCH_WINDOW".to_string(), "30ms".to_string()),
                ]
                .into_iter(),
            )
            .unwrap();
        assert_eq!(
            c.wal.batch_window,
            Duration::from_millis(30),
            "known override still applied"
        );
        let keys: Vec<&str> = ignored.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "FLOE__CACHE__NOT_A_KEY_YET",
                "FLOE__WAL__MAX_BATCH",
                "FLOE__NOSUCHSECTION__X"
            ]
        );
        assert!(ignored[0].1.contains("unknown field"), "{:?}", ignored[0]);
        // Plain apply_env is the same, just warns.
        let mut c2 = Config::default();
        c2.apply_env(vec![("FLOE__CACHE__NOT_A_KEY_YET".to_string(), "1".to_string())].into_iter())
            .unwrap();
    }

    #[test]
    fn settings_merge_over_config_and_are_restricted() {
        let mut base = Config::default();
        base.store.bucket = "b".into();
        let eff = base
            .with_settings(
                r"
[bundles]
min_commits = 3
main_only = false
[maintenance]
checkpoints = false
",
            )
            .unwrap();
        assert_eq!(eff.bundles.min_commits, 3);
        assert!(!eff.bundles.main_only);
        assert!(!eff.maintenance.checkpoints);
        assert_eq!(
            eff.bundles.strategy.len(),
            base.bundles.strategy.len(),
            "untouched keys keep the host's values"
        );
        // Forbidden section.
        let e = base
            .with_settings(
                "[server]
listen = \"0.0.0.0:1\"\n",
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("[server]"), "{e}");
        // Unknown key inside an allowed section.
        assert!(base.with_settings("[bundles]\nnope = 1\n").is_err());
        // Invalid effective config (incremental without a base).
        let e = base
            .with_settings("[[bundles.strategy]]\nname = \"x\"\nkind = \"incremental\"\nschedule = \"@hourly\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("settings"), "{e}");
        assert_eq!(
            base.with_settings("  ").unwrap().bundles.min_commits,
            base.bundles.min_commits
        );
        let e = base
            .with_settings("[upstream]\ntoken_env = \"AWS_SECRET_ACCESS_KEY\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("token_env"), "{e}");
        let pub_toml = base.public_settings_toml().unwrap();
        assert!(pub_toml.contains("[bundles]"), "{pub_toml}");
        assert!(!pub_toml.contains("session_secret"), "{pub_toml}");
        assert!(!pub_toml.contains("[server]"), "{pub_toml}");
        assert!(!pub_toml.contains("token_env"), "{pub_toml}");
    }

    #[test]
    fn repo_description_is_metadata_not_config() {
        let c = Config::default();
        let doc = "[repo]\ndescription = \"Mirror of https://github.com/acme/widgets\"\n\n[bundles]\nmain_only = true\n";
        c.with_settings(doc).unwrap();
        assert_eq!(
            RepoMeta::from_settings(doc).unwrap().description.as_deref(),
            Some("Mirror of https://github.com/acme/widgets")
        );
        assert_eq!(RepoMeta::from_settings("").unwrap(), RepoMeta::default());
        assert_eq!(RepoMeta::from_settings("[bundles]\nmain_only = true\n").unwrap(), RepoMeta::default());
        for bad in [
            "[repo]\ntopic = \"x\"\n".to_string(),
            "[repo]\ndescription = \"a\\nb\"\n".to_string(),
            format!("[repo]\ndescription = \"{}\"\n", "x".repeat(DESCRIPTION_MAX_CHARS + 1)),
        ] {
            assert!(c.with_settings(&bad).is_err(), "{bad}");
            assert!(RepoMeta::from_settings(&bad).is_err(), "{bad}");
        }
        // Never part of the effective config a reader sees.
        assert!(!c.with_settings(doc).unwrap().public_settings_toml().unwrap().contains("Mirror of"));
    }

    #[test]
    fn github_mirror_validates_and_derives_the_token_host() {
        let mut c = Config::default();
        c.github_mirror.enabled = true;
        c.github_mirror.users = vec!["@me".into()];
        c.github_mirror.private_visible_to_all_readers = true;
        c.validate().unwrap();
        // D60: fleet-wide, so a serve-only host accepts it (the loop runs on maintainers).
        let mut serve_only = c.clone();
        serve_only.server.roles = vec![Role::Serve];
        serve_only.validate().unwrap();
        let edits: [fn(&mut GithubMirrorConfig); 8] = [
            |m: &mut GithubMirrorConfig| m.include = vec!["acme".into()],
            |m: &mut GithubMirrorConfig| m.exclude = vec!["a/b/c".into()],
            |m: &mut GithubMirrorConfig| m.repos = vec!["acme/*".into()],
            |m: &mut GithubMirrorConfig| m.api_url = "ftp://x".into(),
            |m: &mut GithubMirrorConfig| m.api_url = "http://ghe.corp/api/v3".into(),
            |m: &mut GithubMirrorConfig| m.git_url = "http://ghe.corp".into(),
            |m: &mut GithubMirrorConfig| m.follow = vec![],
            |m: &mut GithubMirrorConfig| m.follow = vec!["refs/archive/*".into()],
        ];
        for edit in edits {
            let mut bad = c.clone();
            edit(&mut bad.github_mirror);
            assert!(bad.validate().is_err(), "{:?}", bad.github_mirror);
            assert!(bad.github_mirror.check().is_err(), "{:?}", bad.github_mirror);
        }
        let mut local = c.clone();
        local.github_mirror.api_url = "http://127.0.0.1:8080".into();
        local.validate().unwrap();
        // Private repositories need the acknowledgement in every auth mode
        // (the bucket is the fleet's).
        let mut none = c.clone();
        none.github_mirror.private_visible_to_all_readers = false;
        assert!(none.validate().is_err());
        none.github_mirror.include_private = false;
        none.validate().unwrap();
        let mut tok = c.clone();
        tok.server.auth.mode = AuthMode::Token;
        tok.server.auth.tokens = vec![StaticToken {
            principal: "ci".into(),
            token: "t".into(),
            token_env: None,
            write: true,
            admin: false,
        }];
        tok.validate().unwrap();
        tok.github_mirror.private_visible_to_all_readers = false;
        assert!(tok.validate().is_err());
        tok.github_mirror.include_private = false;
        tok.validate().unwrap();

        // The mirror's token: only for mirror-managed repositories on its host,
        // when the mirror is enabled here or the variable is set here.
        let managed = "[upstream]\ngit = \"https://github.com/a/b.git\"\nsource = \"github:1\"\n";
        let own = "[upstream]\ngit = \"https://github.com/a/b.git\"\n";
        let mut d = c.clone();
        d.upstream.token_env = Some("OTHER_TOKEN".into());
        d.derive_mirror_token_env(|_| false);
        assert!(d.upstream.token_env_by_host.is_empty(), "nothing host-wide");
        let m = d.with_settings(managed).unwrap();
        assert_eq!(
            m.upstream_token_env("https://github.com/a/b.git"),
            Some("FLOE_GITHUB_TOKEN")
        );
        assert_eq!(
            m.upstream_token_env("https://gitlab.com/a/b.git"),
            Some("OTHER_TOKEN"),
            "another host"
        );
        let o = d.with_settings(own).unwrap();
        assert_eq!(
            o.upstream_token_env("https://github.com/a/b.git"),
            Some("OTHER_TOKEN"),
            "an own repository is unchanged"
        );
        let mut off = Config::default();
        off.derive_mirror_token_env(|_| false);
        assert_eq!(
            off.with_settings(managed)
                .unwrap()
                .upstream_token_env("https://github.com/a/b.git"),
            None
        );
        off.derive_mirror_token_env(|k| k == "FLOE_GITHUB_TOKEN");
        assert_eq!(
            off.with_settings(managed)
                .unwrap()
                .upstream_token_env("https://github.com/a/b.git"),
            Some("FLOE_GITHUB_TOKEN")
        );
        assert_eq!(
            off.with_settings(own)
                .unwrap()
                .upstream_token_env("https://github.com/a/b.git"),
            None
        );
        // An explicit host entry wins.
        let mut ex = c;
        ex.upstream
            .token_env_by_host
            .insert("github.com".into(), "MINE".into());
        ex.derive_mirror_token_env(|_| true);
        assert_eq!(
            ex.with_settings(managed)
                .unwrap()
                .upstream_token_env("https://github.com/a/b.git"),
            Some("MINE")
        );
    }

    #[test]
    fn upstream_follow_keys_validate_and_redact() {
        let mut base = Config::default();
        base.upstream.token_env = Some("FLOE_UPSTREAM_TOKEN".into());
        base.upstream
            .token_env_by_host
            .insert("github.com".into(), "FLOE_GITHUB_TOKEN".into());
        base.validate().unwrap();
        // `follow = []` is follow off and always publishes (the mirror's freeze).
        let c = base.with_settings("[upstream]\nfollow = []\n").unwrap();
        assert!(c.upstream.follow.is_empty());
        let c = base
            .with_settings(
                "[upstream]\ngit = \"https://github.com/a/b.git\"\nfollow = [\"refs/heads/*\", \"^refs/heads/wip/*\"]\non_rewrite = \"refuse\"\nhead = \"refs/heads/main\"\nfollow_interval = \"10m\"\nsource = \"github:1\"\n",
            )
            .unwrap();
        assert_eq!(c.upstream.on_rewrite, OnRewrite::Refuse);
        assert_eq!(c.upstream.follow_interval, Some(Duration::from_mins(10)));
        assert_eq!(Config::default().upstream.on_rewrite, OnRewrite::Archive);
        for bad in [
            "[upstream]\ngit = \"https://h/a\"\nfollow = [\"refs/archive/*\"]\n",
            "[upstream]\ngit = \"https://h/a\"\nfollow = [\"^refs/heads/x\"]\n",
            "[upstream]\nfollow = [\"refs/heads/*\"]\n",
            "[upstream]\nhead = \"main\"\n",
            "[upstream]\non_rewrite = \"overwrite\"\n",
            "[upstream.token_env_by_host]\n\"github.com\" = \"AWS_SECRET_ACCESS_KEY\"\n",
        ] {
            assert!(base.with_settings(bad).is_err(), "{bad}");
        }
        let pub_toml = base.public_settings_toml().unwrap();
        assert!(!pub_toml.contains("token_env"), "{pub_toml}");
        assert!(!pub_toml.contains("FLOE_GITHUB_TOKEN"), "{pub_toml}");

        // Token resolution: host entry beats token_env; other hosts fall back.
        assert_eq!(
            base.upstream_token_env("https://github.com/a/b.git"),
            Some("FLOE_GITHUB_TOKEN")
        );
        assert_eq!(
            base.upstream_token_env("https://github.com:443/a/b.git"),
            Some("FLOE_GITHUB_TOKEN")
        );
        // The authority ends where git's does: `?`/`#` cannot smuggle a scoped
        // token to another host, and userinfo gets no token at all.
        assert_eq!(
            base.upstream_token_env("https://evil.example?@github.com/a/b"),
            Some("FLOE_UPSTREAM_TOKEN")
        );
        assert_eq!(
            base.upstream_token_env("https://evil.example#@github.com/a/b"),
            Some("FLOE_UPSTREAM_TOKEN")
        );
        assert_eq!(
            base.upstream_token_env("https://x@github.com/a/b.git"),
            None
        );
        for bad in [
            "[upstream]\ngit = \"https://user:ghp_secret@github.com/a/b\"\n",
            "[upstream]\ngit = \"https://evil.example?@github.com/a/b\"\n",
            "[upstream]\ngit = \"https://github.com/a/b#x\"\n",
            "[upstream]\nlfs = \"https://u@github.com/a/b.git/info/lfs\"\n",
        ] {
            let err = base.with_settings(bad).err().map(|e| format!("{e:#}"));
            assert!(err.is_some(), "{bad}");
            assert!(!err.unwrap_or_default().contains("ghp_secret"), "{bad}");
        }
        assert_eq!(
            base.upstream_token_env("https://gitlab.com/a/b.git"),
            Some("FLOE_UPSTREAM_TOKEN")
        );
        let mut bad = base.clone();
        bad.upstream
            .token_env_by_host
            .insert("https://github.com".into(), "X".into());
        assert!(bad.validate().is_err());
    }

    #[test]
    fn auth_modes_validate_fail_closed() {
        let multiple = Config::parse(
            r#"
[server.auth]
audiences = ["floe-cli", "https://git.example.com"]
"#,
        )
        .unwrap();
        assert_eq!(
            multiple.server.auth.audiences,
            vec![
                "floe-cli".to_string(),
                "https://git.example.com".to_string()
            ]
        );
        // token mode needs tokens.
        let err = Config::parse("[store]\nbucket = \"b\"\n[server.auth]\nmode = \"token\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("tokens"), "{err}");
        // oidc: anonymous_read off, an allowlist, and a way in.
        let err = Config::parse("[store]\nbucket = \"b\"\n[server.auth]\nmode = \"oidc\"\nanonymous_read = false\nallowed_domains = [\"example.com\"]\n").unwrap_err();
        assert!(err.to_string().contains("way in"), "{err}");
        let err = Config::parse("[store]\nbucket = \"b\"\n[server.auth]\nmode = \"oidc\"\nanonymous_read = false\nallowed_domains = [\"example.com\"]\noauth_client_id = \"x\"\noauth_client_secret = \"y\"\n").unwrap_err();
        assert!(err.to_string().contains("session_secret"), "{err}");
        let ok = Config::parse("[store]\nbucket = \"b\"\n[server.auth]\nmode = \"oidc\"\nissuer = \"https://login.example.com\"\nanonymous_read = false\nallowed_domains = [\"example.com\"]\noauth_client_id = \"x\"\noauth_client_secret = \"y\"\nsession_secret = \"0123456789abcdef0123456789abcdef\"\n").unwrap();
        assert_eq!(ok.server.auth.issuer, "https://login.example.com");
        assert_eq!(
            ok.server.auth.access_token_ttl,
            Duration::from_hours(90 * 24)
        );
        let err = Config::parse(
            "[store]\nbucket = \"b\"\n[server]\nlisten = \"0.0.0.0:8080\"\n[server.auth]\nmode = \"none\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("loopback-only"), "{err}");
    }

    #[test]
    fn events_section_parses_and_validates() {
        let c = RuntimeConfig::from_toml(
            r#"
[events]
sweep_interval = "1m"
webhook_url = "https://hooks.example.com/floe"
webhook_secret = { value = "s" }
"#,
        )
        .unwrap();
        assert_eq!(c.events.sweep_interval, Duration::from_mins(1));
        assert_eq!(c.events.webhook_secret, Some(Secret::Value("s".into())));
        let bad = RuntimeConfig::from_toml("[events]\nwebhook_url = \"ftp://x\"\n").unwrap();
        let err = bad.validate().unwrap_err();
        assert!(err.to_string().contains("webhook_url"), "{err}");
    }

    /// D60: one home per key — the file refuses runtime sections, env
    /// overrides of them are ignored (reported), never applied.
    #[test]
    fn runtime_sections_are_refused_in_the_file_and_env() {
        for section in runtime::RUNTIME_SECTIONS {
            let err = Config::parse(&format!("[{section}]\n")).unwrap_err();
            assert!(err.to_string().contains("config store"), "{err}");
        }
        let mut c = Config::default();
        let ignored = c
            .apply_env_report(
                [
                    ("FLOE__GITHUB_MIRROR__ENABLED".to_string(), "true".to_string()),
                    ("FLOE__EVENTS__WEBHOOK_URL".to_string(), "https://x".to_string()),
                    ("FLOE__CONFIG_STORE__TTL".to_string(), "1m".to_string()),
                ]
                .into_iter(),
            )
            .unwrap();
        assert_eq!(ignored.len(), 2, "{ignored:?}");
        assert!(!c.github_mirror.enabled);
        assert!(c.events.webhook_url.is_none());
        assert_eq!(c.config_store.ttl, Duration::from_mins(1));
        let parsed = Config::parse("[config_store]\nbucket = \"cfg\"\nhistory = \"records\"\n").unwrap();
        assert_eq!(parsed.config_store.bucket.as_deref(), Some("cfg"));
        assert_eq!(parsed.config_store.history, HistoryMode::Records);
    }

    #[test]
    fn catalog_section_parses_and_validates() {
        let d = CatalogConfig::default();
        assert!(!d.enabled);
        assert_eq!(d.namespace, "floe");
        assert_eq!(d.flush_rows, 5000);
        assert_eq!(d.flush_interval, Duration::from_secs(30));
        let rt = RuntimeConfig::from_toml(
            r#"
[catalog]
enabled = true
uri = "http://localhost:9000/iceberg"
warehouse = "floe-catalog"
flush_interval = "5s"
flush_rows = 10
max_buffer_rows = 100
"#,
        )
        .unwrap();
        let mut c = Config::default().with_runtime(&rt).unwrap();
        c.validate().unwrap();
        assert_eq!(c.catalog.flush_interval, Duration::from_secs(5));
        assert_eq!(c.catalog.s3_access_key_env, "AWS_ACCESS_KEY_ID");
        c.catalog.max_buffer_rows = 5;
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("max_buffer_rows"), "{err}");
        c.catalog.max_buffer_rows = 100;
        assert_eq!(c.catalog.auth, CatalogAuth::None);
        c.catalog.uri = Some("ftp://x/iceberg".into());
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("catalog.uri"), "{err}");
        c.catalog.uri = Some("http://key:secret@x/iceberg".into());
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("catalog.uri"), "{err}");
        c.catalog.uri = Some("http://localhost:9000/iceberg".into());
        c.catalog.token_env = Some("T".into());
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("bearer"), "{err}");
        c.catalog.auth = CatalogAuth::Bearer;
        c.validate().unwrap();
        c.catalog.credential_env = Some("C".into());
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("exactly one"), "{err}");
        c.catalog.token_env = None;
        c.catalog.credential_env = None;
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("exactly one"), "{err}");
        c.catalog.uri = None;
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("catalog.uri"), "{err}");
        // Disabled: nothing is required.
        c.catalog.enabled = false;
        c.validate().unwrap();
        let err = RuntimeConfig::from_toml("[catalog]\nbogus = 1\n").unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
    }

    #[test]
    fn catalog_sigv4_parses_and_fails_closed() {
        let rt = RuntimeConfig::from_toml(
            r#"
[catalog]
enabled = true
uri = "http://rustfs:9000/iceberg"
warehouse = "floe-catalog"
auth = "sigv4"
s3_region = "eu-west-1"
"#,
        )
        .unwrap();
        let mut c = Config::default().with_runtime(&rt).unwrap();
        c.validate().unwrap();
        assert_eq!(c.catalog.auth, CatalogAuth::Sigv4);
        assert_eq!(c.catalog.sigv4_service, "s3");
        // The region defaults to the data-file store's.
        assert_eq!(c.catalog.sigv4_region(), "eu-west-1");
        c.catalog.sigv4_region = Some("us-east-2".into());
        assert_eq!(c.catalog.sigv4_region(), "us-east-2");
        c.catalog.sigv4_service = "S3 Tables".into();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("sigv4_service"), "{err}");
        c.catalog.sigv4_service = "s3tables".into();
        c.validate().unwrap();
        c.catalog.sigv4_region = Some("us east".into());
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("sigv4_region"), "{err}");
        c.catalog.sigv4_region = None;
        // A bearer token next to SigV4 would be ignored: refuse it.
        c.catalog.token_env = Some("T".into());
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("sigv4"), "{err}");
        c.catalog.token_env = None;
        c.catalog.s3_secret_key_env = String::new();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("s3_secret_key_env"), "{err}");
        let err = RuntimeConfig::from_toml("[catalog]\nauth = \"basic\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("unknown variant"), "{err:#}");
    }

    #[test]
    fn github_webhook_section_parses_and_validates() {
        let d = GithubConfig::default();
        assert_eq!(d.webhook_poll_interval, Duration::from_secs(1));
        let c = Config::parse(
            r#"
[server.auth]
mode = "none"
[github]
enabled = true
webhook_poll_interval = "250ms"
"#,
        )
        .unwrap();
        assert_eq!(c.github.webhook_poll_interval, Duration::from_millis(250));
        let err = Config::parse("[github]\nwebhook_url = \"ftp://x\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
    }

    #[test]
    fn rejects_bad_bundle_graph() {
        let text = r#"
[[bundles.strategy]]
name = "daily"
kind = "incremental"
schedule = "@daily"
keep = 3
"#;
        assert!(Config::parse(text).is_err());
    }
}

#[cfg(test)]
mod bundle_strategy_validation {
    use super::*;

    fn base() -> Config {
        let mut c = Config::default();
        c.store.bucket = "b".into();
        c
    }

    #[test]
    fn defaults_validate() {
        base().validate().unwrap();
    }

    #[test]
    fn bad_schedule_incremental_without_full_root_and_keep_zero_are_rejected() {
        let mut c = base();
        c.bundles.strategy[2].schedule = "every hour".into();
        assert!(c.validate().unwrap_err().to_string().contains("schedule"));
        let mut c = base();
        c.bundles.strategy[1].base = Some("nope".into());
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("not a strategy")
        );
        let mut c = base();
        c.bundles.strategy[0].keep = 0;
        assert!(c.validate().unwrap_err().to_string().contains("keep"));
        let mut c = base();
        c.bundles.strategy[2].keep = 28;
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("not a knob on an incremental")
        );
        let mut c = base();
        c.bundles.strategy[1].base = Some("hourly".into());
        c.bundles.strategy[2].base = Some("daily".into());
        assert!(c.validate().unwrap_err().to_string().contains("cycle"));
    }
}
