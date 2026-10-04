import { useState } from "react";
import { api, type CatalogStatus, type CatalogTest } from "../api";
import { useData } from "../data";
import { Box } from "../components/Layout";
import { SectionEditor, errorMessage } from "./SectionEditor";

/** Page 3: the `catalog` section (auth fields are plain env references, so new schemes render without UI work), a connection test, writer status. */
export function CatalogPage() {
  return <SectionEditor section="catalog" title="Catalog (Iceberg audit tables)" header={catalogHeader} />;
}

const catalogHeader = (current: Record<string, unknown>) => <CatalogStatusBox current={current} />;

function CatalogStatusBox({ current }: { current: Record<string, unknown> }) {
  const s: CatalogStatus = useData("admin:catalog", () => api.admin.catalog.status(), 10_000);
  const [busy, setBusy] = useState(false);
  const [res, setRes] = useState<CatalogTest | null>(null);
  const [err, setErr] = useState("");
  const test = async () => {
    setBusy(true);
    setErr("");
    setRes(null);
    try {
      setRes(await api.admin.catalog.test(current));
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Box title="Status">
      {!s.compiled && <div className="notice warn">This binary was built without the catalog feature: enabling the catalog is refused at publish.</div>}
      {s.restart_required && <div className="notice warn">The catalog section changed since this instance started: restart it to apply.</div>}
      <table className="kv">
        <tbody>
          <tr>
            <th scope="row">Writer on this instance</th>
            <td>{s.running ? (s.up === false ? "running, catalog unreachable" : "running") : "not running"}</td>
          </tr>
          <tr>
            <th scope="row">WAL tail (events role)</th>
            <td>{s.tail ? "running" : "not on this instance"}</td>
          </tr>
          <tr>
            <th scope="row">Authentication</th>
            <td>{s.auth}</td>
          </tr>
        </tbody>
      </table>
      <div className="pad secret-row">
        <button type="button" className="btn small" onClick={test} disabled={busy}>
          {busy ? "Testing…" : "Test connection"}
        </button>
        <span role="status" aria-live="polite" className="small">
          {res?.ok && <span className="pill live">OK (HTTP {res.status})</span>}
          {res && !res.ok && (
            <span className="field-error">
              Failed{res.status ? ` (HTTP ${res.status})` : ""}: {res.message ?? res.body ?? "no details"}
            </span>
          )}
          {res?.message && res.ok && <span className="muted"> {res.message}</span>}
          {err && <span className="field-error">{err}</span>}
        </span>
      </div>
    </Box>
  );
}
