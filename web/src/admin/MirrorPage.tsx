import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { api, type CredentialTest, type MirrorPreview, type MirrorStatus } from "../api";
import { testToken } from "./schema-form";
import { invalidate, useData } from "../data";
import { Box } from "../components/Layout";
import { relTime } from "../format";
import { SectionEditor, errorMessage } from "./SectionEditor";

/** Page 2: the `github_mirror` section, the credential test, the live selection preview, and the mirrored repositories. */
export function MirrorPage() {
  return (
    <>
      <SectionEditor
        section="github_mirror"
        title="GitHub mirroring"
        renderAfter={(path, current) => {
          if (path === "github_mirror.token") return <CredentialTestButton current={current} />;
          if (path === "github_mirror.exclude") return <SelectionPreview current={current} />;
          return null;
        }}
      />
      <MirroredRepos />
    </>
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
    <div className="secret-row">
      <button type="button" className="btn small" onClick={test} disabled={busy}>
        {busy ? "Testing…" : "Test credential"}
      </button>
      <span role="status" aria-live="polite" className="small">
        {res?.ok && (
          <span className="pill live">
            OK — {res.login ?? "?"}
            {res.scopes ? ` · scopes: ${res.scopes}` : ""}
            {res.rate_remaining !== null && res.rate_remaining !== undefined ? ` · ${res.rate_remaining} requests left` : ""}
          </span>
        )}
        {res && !res.ok && (
          <span className="field-error">
            Failed{res.status ? ` (HTTP ${res.status})` : ""}: {res.error_class ?? res.message ?? "no details"}
          </span>
        )}
        {err && <span className="field-error">{err}</span>}
      </span>
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
    <div className="notice" aria-live="polite">
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
      title={
        <span className="admin-head">
          <strong>Mirrored repositories ({m.repos.length})</strong>
          <span className="muted">
            {s.enabled ? "enabled" : "disabled"}
            {s.lease ? ` · lease held by ${s.lease.holder}` : " · no lease holder"}
            {s.token_login ? ` · token user ${s.token_login}` : ""}
            {s.last_pass?.finished_at ? ` · last pass ${relTime(s.last_pass.finished_at)}${s.last_pass.complete ? "" : " (incomplete)"}` : ""}
          </span>
          <span className="spacer" />
          <button type="button" className="btn small" disabled={busy !== "" || !s.enabled} onClick={() => act("sync", () => api.admin.mirror.sync())}>
            {busy === "sync" ? "Requesting…" : "Sync now"}
          </button>
        </span>
      }
    >
      {s.error && <div className="notice error">{s.error}</div>}
      {note && (
        <div className={`notice ${note.ok ? "ok" : "error"}`} role={note.ok ? "status" : "alert"}>
          {note.text}
        </div>
      )}
      {m.repos.length === 0 ? (
        <p className="muted pad">No repositories yet. Enable the mirror, choose sources and publish; the first pass runs within a minute on a maintain host.</p>
      ) : (
        <>
          <div className="pad">
            <input type="text" className="text" placeholder="Filter by name or status" aria-label="Filter repositories" value={filter} onChange={(e) => setFilter(e.target.value)} />
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
