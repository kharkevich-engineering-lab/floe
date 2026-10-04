import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import { api, type AdminOverview, type TlsStatus } from "../api";
import { useData } from "../data";
import { Box } from "../components/Layout";
import { RouteBoundary } from "../components/Loading";
import { relTime } from "../format";

function KV({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <table className="kv">
      <tbody>
        {rows.map(([k, v]) => (
          <tr key={k}>
            <th scope="row">{k}</th>
            <td>{v}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

const when = (s?: string | null) => (s ? `${relTime(s)} (${new Date(s).toLocaleString()})` : "—");
const STALE_MS = 10 * 60 * 1000;

/** Page 1: what runs where — the config revision, this and every other instance, the mirror, the catalog, TLS. */
export function OverviewPage() {
  const o: AdminOverview = useData("admin:overview", () => api.admin.overview(), 10_000);
  const c = o.config;
  const me = o.instance;
  const counts = Object.entries(o.mirror.counts ?? {}).filter(([, n]) => n > 0);
  return (
    <div className="admin-cols">
      <Box title="Configuration">
        {c.error && <div className="notice error">Config store unreachable: {c.error}</div>}
        <KV
          rows={[
            ["Revision", c.revision === 0 ? "none yet (built-in defaults)" : <Link key="rev" to="/_admin/history">{c.revision}</Link>],
            ["Published", c.revision ? `${when(c.updated_at)} by ${c.author ?? "?"}` : "—"],
            ["Message", c.message || "—"],
            ["Store", <code key="loc">{o.store.location}</code>],
            ["History", o.store.history_mode === "versions" ? "bucket object versions" : "floe history records"],
            ["Sealing key", o.store.sealing_key ? `${o.store.key_env} is set here` : `${o.store.key_env} is not set here: secrets can only be env references`],
          ]}
        />
      </Box>
      <Box title="This instance">
        {me.apply_error && (
          <div className="notice error" role="alert">
            Not applied: {me.apply_error}
          </div>
        )}
        {me.restart_required.length > 0 && <div className="notice warn">Restart required for: {me.restart_required.join(", ")}</div>}
        <KV
          rows={[
            ["Instance", <code key="id">{me.id}</code>],
            ["Version", me.version],
            ["Roles", me.roles.join(", ")],
            ["Applied revision", String(me.applied_revision)],
            ["Last check", me.check_error ? <span key="err" className="field-error">{me.check_error}</span> : when(me.last_check)],
            ["Started", when(me.started_at)],
          ]}
        />
      </Box>
      <Box title={`Instances seen (${o.instances.length})`}>
        {o.instances.length === 0 ? (
          <p className="muted pad">No heartbeats yet (instances write one within a minute of starting).</p>
        ) : (
          <table className="grid">
            <thead>
              <tr>
                <th>instance</th>
                <th>revision</th>
                <th>seen</th>
                <th>notes</th>
              </tr>
            </thead>
            <tbody>
              {o.instances.map((i) => {
                const stale = Date.now() - new Date(i.seen_at).getTime() > STALE_MS;
                return (
                  <tr key={i.instance} className={stale ? "stale" : undefined}>
                    <td>
                      <code>{i.instance}</code>
                      <div className="muted small">
                        {i.version} · {i.roles.join(", ")}
                      </div>
                    </td>
                    <td>{i.applied_revision}</td>
                    <td>{relTime(i.seen_at)}</td>
                    <td>
                      {i.apply_error && <div className="field-error">{i.apply_error}</div>}
                      {i.restart_required.length > 0 && <span className="pill restart">restart: {i.restart_required.join(", ")}</span>}
                      {stale && <span className="muted">stale</span>}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </Box>
      <Box title={<Link to="/_admin/mirror">GitHub mirror</Link>}>
        {o.mirror.error && <div className="notice error">{o.mirror.error}</div>}
        <KV
          rows={[
            ["State", o.mirror.enabled ? <span key="state" className="pill live">enabled</span> : "disabled"],
            ["Lease", o.mirror.lease ? `${o.mirror.lease.holder} (until ${new Date(o.mirror.lease.expires_at).toLocaleTimeString()})` : "not held"],
            ["Token user", o.mirror.token_login ?? "—"],
            [
              "Last pass",
              o.mirror.last_pass?.finished_at
                ? `${relTime(o.mirror.last_pass.finished_at)} · ${o.mirror.last_pass.complete ? "complete" : "incomplete"} · created ${o.mirror.last_pass.created}, errors ${o.mirror.last_pass.errors}`
                : "—",
            ],
            ["Repositories", counts.length === 0 ? "none" : counts.map(([k, n]) => `${n} ${k}`).join(" · ")],
          ]}
        />
      </Box>
      <Box title={<Link to="/_admin/catalog">Catalog</Link>}>
        <KV
          rows={[
            ["Binary", o.catalog.compiled ? "built with the catalog feature" : "built without the catalog feature"],
            ["Configured", o.catalog.enabled ? "enabled" : "disabled"],
            ["Writer here", o.catalog.running ? (o.catalog.up === false ? "running, catalog unreachable" : "running") : "not running"],
            ["Catalog", o.catalog.uri ? `${o.catalog.uri} · ${o.catalog.namespace}` : "—"],
          ]}
        />
      </Box>
      <Box title="TLS certificate">
        <RouteBoundary fallback={<p className="muted pad">loading…</p>}>
          <TlsBox />
        </RouteBoundary>
      </Box>
    </div>
  );
}

/** Read-only TLS status from `GET /api/v1/tls` (D59). TLS is bootstrap config (`[server.tls]`): never editable here. */
function TlsBox() {
  const tls: TlsStatus = useData("admin:tls", () => api.tls(), 60_000);
  if (tls.mode === "off") {
    return (
      <p className="muted pad">
        TLS is off on this instance (plain HTTP: loopback, or an edge terminates TLS). Configured in the bootstrap file (<code>[server.tls]</code>).
      </p>
    );
  }
  const expires = tls.not_after ? new Date(tls.not_after).getTime() - Date.now() : null;
  const soon = expires !== null && expires < 14 * 24 * 3600 * 1000;
  return (
    <>
      {!tls.loaded && <div className="notice warn">No certificate loaded yet.</div>}
      {tls.last_error && (
        <div className="notice error" role="alert">
          Last renewal failed{tls.failures > 1 ? ` (${tls.failures} times)` : ""}: {tls.last_error}
        </div>
      )}
      {soon && <div className="notice warn">The certificate expires {relTime(tls.not_after ?? "")}.</div>}
      <KV
        rows={[
          ["Mode", tls.mode],
          ["Domains", tls.domains.length > 0 ? tls.domains.join(", ") : "—"],
          ["Issuer", tls.issuer ?? "—"],
          ["Valid", `${tls.not_before ? new Date(tls.not_before).toLocaleDateString() : "?"} → ${tls.not_after ? new Date(tls.not_after).toLocaleDateString() : "?"}`],
          ["Source", tls.source ?? "—"],
          ["Fingerprint", tls.fingerprint ? <code key="fp">{tls.fingerprint}</code> : "—"],
          ["Last renewal", when(tls.last_renewal_at)],
          ["Next attempt", tls.renewal_due ? `due · ${when(tls.next_attempt_at)}` : when(tls.next_attempt_at)],
        ]}
      />
    </>
  );
}
