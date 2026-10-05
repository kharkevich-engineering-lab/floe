# Spike: the SigV4 route to RustFS S3 Tables (R1)

Context: M0 spike (a) of `docs/design/code-intelligence.md` (§2.2, §14 R1, open question 1). For whoever
builds `floe-ice` (M6). Recorded 2026-10-04.

## Decision: reuse the catalog's in-process signing proxy (D63); no separate spike

The question — iceberg-rust 0.11's `AuthManager`/`AuthSession`, or a signing proxy — was answered by the
github-mirror effort while this milestone was in flight, for the same RustFS endpoint, so it is recorded here
instead of being decided twice:

- **D63** (branch `feat/catalog-sigv4`, stacked on the github-mirror PR #4; `docs/design/github-mirror.md` §C.9):
  `[catalog] auth = "sigv4"` puts an in-process signing proxy (`floe_catalog::sigv4::SigningProxy`) between
  `RestCatalog` and the endpoint — an axum server on a Unix socket in a `0700` temp directory, reached only
  through the reqwest client handed to `RestCatalogBuilder::with_client`, signing with `aws-sigv4` (signing name
  `s3` for RustFS `/iceberg`, `s3tables` for AWS). `iceberg-catalog-rest` 0.10.1 has no request hook, and 0.11's
  `AuthManager` was not released.
- The module is self-contained by design so it can move into the shared Iceberg crate (`floe-ice`, §2.2) that
  code intelligence needs. When iceberg-rust 0.11 lands, `AuthManager` replaces the proxy behind the same config
  for both users.

## Consequences for this design

- R1 is closed for M6 once D63 merges: `floe-ice`'s `conn.rs` takes the proxy from `floe-catalog` (extract, do
  not duplicate; §15 question 2) and `[codeintel.catalog]` keeps sharing the `[catalog]` connection (uri, SigV4
  credentials), as §11 already says.
- Until `[catalog]` exists on `main` (it arrives with PR #4), `Config::validate` refuses `codeintel.enabled` with
  `require_catalog = true`; the standalone shape (`require_catalog = false`) is the only one that validates.
  When PR #4 lands, that rule becomes "`require_catalog = true` needs `catalog.enabled`" as §11 specifies.
- The design's iceberg-rust 0.11 `AuthSession` route (≈ 50 LOC) remains the long-term shape; nothing in
  M1–M5 depends on either.
