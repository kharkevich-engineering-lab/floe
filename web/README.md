# floe web UI

Context: **frontend implementation and build guide** for engineers changing the bundled React SPA, its Vite
pipeline, static assets, or loading states. The HTTP contract belongs in `web/API.md`; public SDK usage
belongs in `web/sdk/README.md`.

React 19 + Vite 8 (rolldown) SPA, embedded in the floe binary (`rust-embed`)
and served under `/_ui/`. Build from the repository root with `just web-build`
(`pnpm run build` = `oxlint --deny-warnings && tsc --noEmit && vite build`).

For local development, run the server and Vite together:

```sh
FLOE_URL=http://127.0.0.1:8080 pnpm run dev
```

Checks: `pnpm run lint` (oxlint, config in `.oxlintrc.json`), `pnpm run typecheck`,
`pnpm run test` (`node --experimental-strip-types --test` over the pure admin modules — no test framework,
no extra dependency; part of `pnpm run build`), `pnpx react-doctor@latest .` (kept at 100/100).

## Design system (D65)

The SPA is a Kharkevich Engineering Lab product and wears the corporate design system of `hub.kharkevich.com`
(`styles/main.css`, identical to kharkevich.com; the rules are on the hub's brand page):

- **Tokens** — `src/styles.css` `:root` (dark, the default) and `html.light`: `--bg`, `--bg2`, `--surface`,
  `--line`, `--line-strong`, `--text`, `--muted`, `--blue`, `--blue2`, `--blue-cta`, `--brand-accent`, `--shadow`,
  `--radius-sm` (3 px) / `--radius-md` (8 px) verbatim from the hub; `--max` (1400 px) from kharkevich.com. Status
  colours are the hub's status hues (`#10b981`, `#ef4444`, `#f59e0b`), darkened in the light theme until text passes
  WCAG AA. Everything else is a `color-mix` of those; no raw colours outside the token block.
- **Type** — Inter variable, self-hosted (`src/assets/fonts`, bundled by Vite; no CDN — the binary embeds the SPA),
  on the brand-page scale (display 800/−0.03em, headings 700/−0.02em, body 15 px/1.6, eyebrow 11 px/600/0.18em
  uppercase). Code uses the hub's system monospace stack.
- **Theme** — `src/theme.ts`: the sister sites' `kharkevich-theme` localStorage key and system → light → dark cycle;
  `index.html` applies it before first paint. Diffs and code follow it (`useTheme().light`).
- **Mark** — the Lab logo (`src/assets/logo.svg`, from the hub's brand pack, never recoloured) with the product
  name **floe** in the lockup.
- **Primitives** — `components/ui.tsx` (`PageHeader`, `StatusBadge`, `Notice`, `EmptyState`, `KeyHint`) and
  `components/Icon.tsx` (the hub's 24 px / 2 px-stroke line icons, inline, `currentColor`). Pages compose these;
  a new page should not need new colours or one-off spacing.
- **Copy** — Canadian English per the hub (`-ize`, `-our`, spaced em dashes, typographic apostrophes). English
  only: the hub's bilingual rule covers its own sites; strings stay in components (not yet a dictionary).
- **Accessibility** — skip link, landmarks, visible focus (`--blue` 2 px ring), 24 px minimum targets, links in
  running text underlined, status never by colour alone, `prefers-reduced-motion` and `forced-colors` honoured.
  Check with axe (WCAG 2.1 AA) in both themes after visual changes.

## Admin area (`src/admin/`, `/_admin/*`, D62)

Lazy chunks for admins only (the header shows "Administration" when `/api/v1/me` says `admin`). The runtime config
document's sections are rendered from `GET /api/v1/admin/config/schema` by `schema-form.ts` (pure, tested), so a new
key needs no UI work. `SectionEditor` lays a section out as: a header card that **leads with status** (each page's
`summary`, from its status endpoint) and the primary action (configure / test connection); one restart notice
(section level, and in the save bar for the changed fields that need it — never per field); the settings as
numbered steps in the schema's group order, fields shown only while their `x-floe.when` holds, `x-floe.advanced`
groups folded; plain-language help with the config key behind a small key toggle (`KeyHint`); write-only secrets
with a set / not set state; inline validation as you type (`…/config/validate`); and a sticky save bar (unsaved
count, review diff, reason, Discard, Save — CAS'd on `base_revision`, 409 → "reload latest"). History and raw JSON
are secondary actions. `useUnsavedGuard` adds a `beforeunload` prompt and a capture-phase link guard (the SPA has
no data router). Per-repository settings are edited line by line (`toml-lines.ts`, tested) so keys the form does
not manage stay verbatim. TLS is never edited here; the overview shows `GET /api/v1/tls` read-only.

## SDK (`sdk/repos.ts` → `/repos.js`, `/repos.mjs`)

The public SDK (D20, `sdk/README.md`) lives next to the SPA and is built into
`dist/` by the second step of `pnpm run build` (`vite.sdk.config.ts`: IIFE that
registers `window.repos`, plus an ESM build). `src/api.ts` is an
adapter over it — repository requests use `/{owner}/{repo}/api/*`, cross-origin pages
use `/{owner}/{repo}/api-browser/*`, and only non-repository discovery/authentication
uses `/api/v1/*` (D26/D27). Changing the API means changing `sdk/repos.ts` and
`API.md` in the same commit.

## Production build

- **Code splitting**: vendor groups (`vendor-react`, `vendor-diffs`,
  `vendor-markdown`) + per-route lazy chunks (`BlobPage`, `CommitPage`,
  `OverviewPage`, `MarkdownRenderer`); shiki grammars/themes stay lazy
  (`@pierre/diffs` loads them on demand). Entry ≈ 15 kB, react ≈ 73 kB gz.
- **Import map** (`plugins/importmap.ts`): chunks import each other through
  bare specifiers (`floe/<chunk>`) resolved by a `<script type="importmap">`
  in `index.html`; each chunk is re-hashed over its own bytes only, so
  changing one chunk does not cascade new hashes through its importers — users
  re-download only what changed. The import map lives in `index.html`
  (`no-cache` + ETag → usually a 304).
- **Precompression** (`plugins/precompress.ts`): `.br` (q11) and `.gz`
  siblings for every asset; the server negotiates `Accept-Encoding` and never
  compresses static assets at request time.
- **Serving** (`crates/floe-server/src/web/ui.rs`): `/_ui/assets/*` →
  `Cache-Control: public, max-age=31536000, immutable`, strong `ETag`
  (build-time sha256), `If-None-Match` → 304, `Vary: Accept-Encoding`,
  `Content-Length`, `HEAD`. `index.html` → `no-cache` + ETag. Dynamic JSON is
  brotli/gzip-compressed on the fly (`CompressionLayer`), SSE never.

## Loading states

- Data loading is Suspense-based (`src/data.ts`: `useData(key, fn, ttl)` — a
  promise cache with background revalidation wrapped in `startTransition`;
  sha-addressed data uses `ttl: Infinity`).
- `index.html` paints a skeleton before any JS; the shell (top bar, repo
  header, tabs) stays put while a page suspends (`RouteBoundary` =
  ErrorBoundary + Suspense with skeletons); navigations are transitions, so
  the previous page stays visible with the top progress bar running
  (`TopProgress`, fed by every in-flight fetch and lazy chunk import).

## SSE

`src/data.ts#readSse` reads `text/event-stream` over `fetch` (GET/POST, with
`Accept`/auth headers). Used for the branch/tag picker (`refs/{branches,tags}`
streams `event: ref` per match; painted progressively, a keystroke aborts the
previous stream) and for maintenance ops on the WAL page (`POST …/ops/{op}`
streams `started`/`log`/`done`/`error` into a live console).
