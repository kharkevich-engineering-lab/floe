import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { api, type CredentialTest, type MirrorPreview, type MirrorStatus } from "../api";
import { testToken } from "./schema-form";
import { invalidate, useData } from "../data";
import { Box } from "../components/Layout";
import { EmptyState, Notice, StatusBadge } from "../components/ui";
import { mirrorState } from "./status";
import { relTime } from "../format";
import { Facts, SectionEditor, errorMessage, focusForm } from "./SectionEditor";

/** The `github_mirror` section: status and a credential test first, then the settings with a live selection preview, then the mirrored repositories. */
export function MirrorPage() {
  return (
    <>
      <SectionEditor
        section="github_mirror"
        icon="mirror"
        summary={mirrorSummary}
        renderAfter={mirrorAfter}
      />
      <MirroredRepos />
    </>
  );
}

const mirrorSummary = (current: Record<string, unknown>) => <MirrorSummary current={current} />;
const mirrorAfter = (path: string, current: Record<string, unknown>) => (path === "github_mirror.exclude" ? <SelectionPreview current={current} /> : null);

function MirrorSummary({ current }: { current: Record<string, unknown> }) {
  const m: MirrorStatus = useData("admin:mirror", () => api.admin.mirror.status(), 10_000);
  const s = m.summary;
  const state = mirrorState(s);
  const counts = Object.entries(s.counts ?? {}).filter(([, n]) => n > 0);
  return (
    <div className="section-status">
      <div className="section-status-head">
        <StatusBadge tone={state.tone}>{state.label}</StatusBadge>
        <span className="muted">{state.detail}</span>
        <span className="spacer" />
        {!s.enabled && !s.token_login ? (
          <button type="button" className="btn primary" onClick={() => focusForm("github_mirror")}>
            Set up mirroring
          </button>
        ) : null}
      </div>
      <Facts
        rows={[
          ["Signed in to GitHub as", s.token_login ?? <span key="l" className="muted">Unknown — test the token</span>],
          ["Repositories", m.repos.length === 0 ? "None yet" : counts.length > 0 ? counts.map(([k, n]) => `${n} ${k}`).join(" · ") : String(m.repos.length)],
          ["Running on", s.lease ? s.lease.holder : <span key="h" className="muted">No host holds the mirror lease</span>],
        ]}
      />
      <CredentialTestButton current={current} />
    </div>
  );
}

function CredentialTestButton({ current }: { current: Record<string, unknown> }) {
  const [busy, setBusy] = useState(false);
  const [res, setRes] = useState<CredentialTest | null>(null);
  const [err, setErr] = useState("");
  const test = async () => {
    setBusy(true);
    setErr("");
    setRes(null);
    try {
      const token = testToken(current.token);
      setRes(await api.admin.mirror.test({ api_url: typeof current.api_url === "string" ? current.api_url : undefined, token }));
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="section-status-actions">
      <button type="button" className="btn" onClick={test} disabled={busy}>
        {busy ? "Testing…" : "Test the GitHub token"}
      </button>
      <div aria-live="polite" className="grow">
        {res?.ok && (
          <Notice tone="ok" title={`Signed in as ${res.login ?? "?"}`}>
            {res.scopes ? `Scopes: ${res.scopes}. ` : ""}
            {res.rate_remaining !== null && res.rate_remaining !== undefined ? `${res.rate_remaining} API requests left this hour.` : ""}
          </Notice>
        )}
        {res && !res.ok && (
          <Notice tone="danger" title="GitHub refused the token">
            {res.status ? `HTTP ${res.status}: ` : ""}
            {res.error_class ?? res.message ?? "no details"}
          </Notice>
        )}
        {err && <Notice tone="danger" title="Could not run the test">{err}</Notice>}
      </div>
    </div>
  );
}

function useDebouncedJson(value: unknown, ms: number): string {
  const text = JSON.stringify(value);
  const [v, setV] = useState(text);
  useEffect(() => {
    const t = setTimeout(() => setV(text), ms);
    return () => clearTimeout(t);
  }, [text, ms]);
  return v;
}

/** Which known repositories the edited include/exclude (and skips) select — no forge call; "Discover now" runs a dry-run pass. */
function SelectionPreview({ current }: { current: Record<string, unknown> }) {
  const section = {
    include: current.include,
    exclude: current.exclude,
    repos: current.repos,
    skip_archived: current.skip_archived,
    skip_forks: current.skip_forks,
    include_private: current.include_private,
    max_repo_size: current.max_repo_size,
  };
  const debounced = useDebouncedJson(section, 400);
  const [preview, setPreview] = useState<MirrorPreview | null>(null);
  const [err, setErr] = useState("");
  const [discovering, setDiscovering] = useState(false);
  useEffect(() => {
    let cancelled = false;
    api.admin.mirror
      .preview(JSON.parse(debounced) as Record<string, unknown>)
      .then((p) => {
        if (!cancelled) {
          setPreview(p);
          setErr("");
        }
      })
      .catch((e: unknown) => {
        if (!cancelled) setErr(errorMessage(e));
      });
    return () => {
      cancelled = true;
    };
  }, [debounced]);
  const discover = async () => {
    setDiscovering(true);
    try {
      // Never the form's env reference: a typed value, else the stored token
      // (which the server sends only to the configured api_url).
      const { token: _token, ...rest } = current;
      const typed = testToken(current.token);
      setPreview(await api.admin.mirror.preview(typed ? { ...rest, token: typed } : rest, { discover: true }));
      setErr("");
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setDiscovering(false);
    }
  };
  const plan = preview?.plan;
  return (
    <div className="preview-panel" aria-live="polite">
      <strong>Live preview</strong>{" "}
      {preview ? (
        <span className="muted">
          of {preview.known} known repositories: {preview.selected} selected, {preview.too_large} too large, {preview.out} out
        </span>
      ) : (
        <span className="muted">loading…</span>
      )}{" "}
      <button type="button" className="btn small" onClick={discover} disabled={discovering}>
        {discovering ? "Discovering…" : "Discover now (dry run)"}
      </button>
      {err && <p className="field-error">{err}</p>}
      {preview && preview.repos.length > 0 && (
        <ul className="preview-list">
          {preview.repos.map((r) => (
            <li key={r.full_name}>
              <span className={`verdict ${r.verdict}`}>{r.verdict}</span>
              <span>{r.full_name}</span>
              <span className="muted">{r.reason}</span>
            </li>
          ))}
        </ul>
      )}
      {preview?.known === 0 && <p className="muted small">The mirror has not discovered anything yet: use “Discover now” to list what GitHub would return.</p>}
      {plan && "error" in plan && <p className="field-error">Dry run failed: {plan.error}</p>}
      {plan && "lines" in plan && (
        <>
          <p className="small">
            Dry run: {plan.summary} ({plan.discovered} discovered{plan.complete ? "" : ", incomplete"})
          </p>
          <ul className="preview-list">
            {plan.lines.length === 0 && <li className="muted">nothing to do</li>}
            {plan.lines.map((l) => (
              <li key={l}>{l}</li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

function MirroredRepos() {
  const m: MirrorStatus = useData("admin:mirror", () => api.admin.mirror.status(), 10_000);
  const [busy, setBusy] = useState("");
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const [filter, setFilter] = useState("");
  const act = async (key: string, f: () => Promise<{ message?: string; revision?: number }>) => {
    setBusy(key);
    setNote(null);
    try {
      const r = await f();
      setNote({ ok: true, text: r.message ?? (r.revision ? `Published config revision ${r.revision}.` : "Done.") });
      invalidate("admin:");
    } catch (e) {
      setNote({ ok: false, text: errorMessage(e) });
    } finally {
      setBusy("");
    }
  };
  const s = m.summary;
  const rows = m.repos.filter((r) => !filter || r.full_name.toLowerCase().includes(filter.toLowerCase()) || r.status.includes(filter.toLowerCase()));
  return (
    <Box
      className="mirrored"
      title={
        <>
          <h2 className="box-title">Mirrored repositories</h2>
          <span className="pill">{m.repos.length}</span>
          <span className="spacer" />
          <button type="button" className="btn small" disabled={busy !== "" || !s.enabled} onClick={() => act("sync", () => api.admin.mirror.sync())}>
            {busy === "sync" ? "Requesting…" : "Sync now"}
          </button>
        </>
      }
    >
      {s.error && <Notice tone="danger">{s.error}</Notice>}
      {note && <Notice tone={note.ok ? "ok" : "danger"}>{note.text}</Notice>}
      {m.repos.length === 0 ? (
        <EmptyState icon="mirror" title="No mirrored repositories yet">
          <p>Turn mirroring on, choose sources and save; the first run starts within a minute on a maintenance host.</p>
        </EmptyState>
      ) : (
        <>
          <div className="pad filter-row">
            <input type="search" className="text" placeholder="Filter by name or status" aria-label="Filter repositories" value={filter} onChange={(e) => setFilter(e.target.value)} />
          </div>
          <div className="scroll-x">
            <table className="grid">
              <thead>
                <tr>
                  <th>GitHub</th>
                  <th>floe</th>
                  <th>status</th>
                  <th>last seen</th>
                  <th>pushed</th>
                  <th>
                    <span className="sr-only">actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <tr key={r.id}>
                    <td>
                      {r.full_name}
                      {r.private && <span className="pill">private</span>}
                      {r.archived && <span className="pill">archived</span>}
                      {r.fork && <span className="pill">fork</span>}
                    </td>
                    <td>{r.floe ? <Link to={`/${r.floe}`}>{r.floe}</Link> : <span className="muted">—</span>}</td>
                    <td>
                      <span className={`pill status-${r.status}`}>{r.paused ? "paused" : r.status}</span>
                      {r.last_error && <div className="field-error">{r.last_error}</div>}
                    </td>
                    <td>{r.last_seen ? relTime(r.last_seen) : "—"}</td>
                    <td>{r.pushed_at ? relTime(r.pushed_at) : "—"}</td>
                    <td>
                      {r.paused ? (
                        <button type="button" className="btn small" disabled={busy !== ""} onClick={() => act(r.id, () => api.admin.mirror.resume(r.full_name))}>
                          Resume
                        </button>
                      ) : (
                        <button type="button" className="btn small" disabled={busy !== ""} onClick={() => act(r.id, () => api.admin.mirror.pause(r.full_name))}>
                          Pause
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </Box>
  );
}
