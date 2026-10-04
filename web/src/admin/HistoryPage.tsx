import { Suspense, useState } from "react";
import { api, ApiError, type AdminConfig, type ConfigHistoryEntry, type ConfigRevision } from "../api";
import { invalidate, useData } from "../data";
import { Box } from "../components/Layout";
import { relTime } from "../format";
import { ConfigDiff } from "./ConfigDiff";
import { errorMessage } from "./SectionEditor";

/** Page 5: every revision (author, message, time, diff) and rollback = publishing an old revision as a new one. */
export function HistoryPage() {
  const config: AdminConfig = useData("admin:config", () => api.admin.config.get(), 5_000);
  const [before, setBefore] = useState<number | undefined>(undefined);
  const page = useData(`admin:history:${before ?? "top"}`, () => api.admin.config.history({ before, n: 25 }), 10_000);
  const [selected, setSelected] = useState<number | null>(null);
  const entries = page.entries;
  const last = entries[entries.length - 1];
  if (config.revision === 0) {
    return (
      <Box title="Config history">
        <p className="muted pad">No revisions yet: the built-in runtime defaults apply. The first publish (or `floe config import`) creates revision 1.</p>
      </Box>
    );
  }
  return (
    <div className="admin-cols">
      <Box title={`Revisions (current: ${config.revision}, history in ${config.history_mode === "versions" ? "bucket object versions" : "floe records"})`}>
        <table className="grid">
          <thead>
            <tr>
              <th>rev</th>
              <th>when</th>
              <th>who</th>
              <th>message</th>
              <th>changes</th>
            </tr>
          </thead>
          <tbody>
            {entries.map((e: ConfigHistoryEntry) => (
              <tr key={e.revision} className={`history-row${selected === e.revision ? " selected" : ""}`} aria-selected={selected === e.revision}>
                <td>
                  <button type="button" className="btn small" onClick={() => setSelected(e.revision)} aria-label={`Show revision ${e.revision}`}>
                    {e.revision}
                  </button>
                </td>
                <td title={new Date(e.updated_at).toLocaleString()}>{relTime(e.updated_at)}</td>
                <td>{e.author}</td>
                <td>
                  {e.message || <span className="muted">—</span>}
                  {e.rolled_back_from ? <span className="pill">rollback to {e.rolled_back_from}</span> : null}
                </td>
                <td>{e.diff.length}</td>
              </tr>
            ))}
          </tbody>
        </table>
        <div className="pad secret-row">
          {before !== undefined && (
            <button type="button" className="btn small" onClick={() => setBefore(undefined)}>
              Newest
            </button>
          )}
          {last && last.revision > 1 && (
            <button type="button" className="btn small" onClick={() => setBefore(last.revision)}>
              Older
            </button>
          )}
        </div>
      </Box>
      {selected !== null && (
        <Suspense fallback={<Box title={`Revision ${selected}`}>loading…</Box>}>
          <RevisionDetail n={selected} current={config.revision} />
        </Suspense>
      )}
    </div>
  );
}

function RevisionDetail({ n, current }: { n: number; current: number }) {
  const rev = useData<ConfigRevision | { gone: string }>(
    `admin:revision:${n}`,
    () =>
      api.admin.config.revision(n).catch((e: unknown) => {
        if (e instanceof ApiError && e.status === 410) return { gone: e.message };
        throw e;
      }),
    Number.POSITIVE_INFINITY,
  );
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const [showDoc, setShowDoc] = useState(false);
  if ("gone" in rev) {
    return (
      <Box title={`Revision ${n}`}>
        <div className="notice warn">{rev.gone}</div>
      </Box>
    );
  }
  const rollback = async () => {
    if (!window.confirm(`Publish revision ${n}'s document as a new revision?`)) return;
    setBusy(true);
    setNote(null);
    try {
      const r = await api.admin.config.rollback(n, { base_revision: current, message });
      setNote({ ok: true, text: `Published revision ${r.revision}.${r.restart_required.length ? ` Restart required for: ${r.restart_required.join(", ")}.` : ""}` });
      invalidate("admin:");
    } catch (e) {
      setNote({ ok: false, text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Box title={`Revision ${n} — ${rev.author}, ${rev.updated_at ? new Date(rev.updated_at).toLocaleString() : ""}`}>
      <p className="pad">{rev.message || <span className="muted">no message</span>}</p>
      <div className="box-header">Changes against revision {n - 1 || "defaults"}</div>
      <ConfigDiff diff={rev.diff} empty="No changes in this revision." />
      <div className="pad">
        <button type="button" className="btn small" onClick={() => setShowDoc(!showDoc)} aria-expanded={showDoc}>
          {showDoc ? "Hide document" : "Show document"}
        </button>
      </div>
      {showDoc && <pre className="code-block pad">{JSON.stringify(rev.document, null, 2)}</pre>}
      {n !== current && (
        <div className="save-bar">
          <input type="text" className="text" placeholder="Why roll back" aria-label="Rollback message" value={message} onChange={(e) => setMessage(e.target.value)} />
          <button type="button" className="btn danger" disabled={busy} onClick={rollback}>
            {busy ? "Publishing…" : `Roll back to revision ${n}`}
          </button>
        </div>
      )}
      {note && (
        <div className={`notice ${note.ok ? "ok" : "error"}`} role={note.ok ? "status" : "alert"}>
          {note.text}
        </div>
      )}
    </Box>
  );
}
