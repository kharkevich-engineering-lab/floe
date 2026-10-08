import { useState } from "react";
import { api, type CatalogStatus, type CatalogTest } from "../api";
import { useData } from "../data";
import { Notice, StatusBadge } from "../components/ui";
import { catalogState } from "./status";
import { Facts, SectionEditor, errorMessage, focusForm } from "./SectionEditor";

/** The `catalog` section: status first (from `GET …/admin/catalog`), then connection, authentication, storage, advanced. */
export function CatalogPage() {
  return <SectionEditor section="catalog" icon="database" title="Audit catalog" summary={catalogSummary} />;
}

const catalogSummary = (current: Record<string, unknown>, saved: Record<string, unknown>) => <CatalogSummary current={current} saved={saved} />;

const AUTH_LABEL: Record<string, string> = { none: "None", bearer: "Bearer token or OAuth2", sigv4: "AWS Signature V4" };

function CatalogSummary({ current, saved }: { current: Record<string, unknown>; saved: Record<string, unknown> }) {
  const s: CatalogStatus = useData("admin:catalog", () => api.admin.catalog.status(), 10_000);
  const [busy, setBusy] = useState(false);
  const [res, setRes] = useState<CatalogTest | null>(null);
  const [err, setErr] = useState("");
  const state = catalogState(s);
  const configured = typeof saved.uri === "string" && saved.uri !== "";
  const canTest = typeof current.uri === "string" && current.uri !== "";
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
    <div className="section-status">
      <div className="section-status-head">
        <StatusBadge tone={state.tone}>{state.label}</StatusBadge>
        <span className="muted">{state.detail}</span>
        <span className="spacer" />
        {!configured && s.compiled ? (
          <button type="button" className="btn primary" onClick={() => focusForm("catalog")}>
            Configure the catalog
          </button>
        ) : (
          <button type="button" className="btn primary" onClick={test} disabled={busy || !canTest} title={canTest ? undefined : "Set a catalog URI first"}>
            {busy ? "Testing…" : "Test connection"}
          </button>
        )}
      </div>
      <Facts
        rows={[
          ["Catalog", s.uri ? <code key="u">{s.uri}</code> : <span key="u" className="muted">Not set</span>],
          ["Warehouse · namespace", s.warehouse || s.namespace ? `${s.warehouse ?? "—"} · ${s.namespace}` : "—"],
          ["Sign-in", AUTH_LABEL[s.auth] ?? s.auth],
          ["Writer on this instance", s.running ? (s.up === false ? "Running, catalog unreachable" : "Running") : "Not running"],
          ["WAL tail", s.tail ? "Running" : "Not on this instance (events role)"],
        ]}
      />
      <div aria-live="polite">
        {res?.ok && (
          <Notice tone="ok" title="The catalog answered">
            HTTP {res.status}
            {res.latency_ms !== undefined ? ` in ${res.latency_ms} ms` : ""}.
            {res.unauthenticated && " The writer signs or exchanges credentials itself; this probe did not authenticate."}
          </Notice>
        )}
        {res && !res.ok && (
          <Notice tone="danger" title="Connection failed">
            {res.status ? `HTTP ${res.status}: ` : ""}
            {res.error_class ?? "no details"}
          </Notice>
        )}
        {err && <Notice tone="danger" title="Could not run the test">{err}</Notice>}
      </div>
    </div>
  );
}
