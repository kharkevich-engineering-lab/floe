import type { ReactNode } from "react";
import { Icon, type IconName } from "../components/Icon";
import { EmptyState, Notice, StatusBadge, type Tone } from "../components/ui";
import { catalogState, mirrorState } from "./status";
import { Link } from "react-router-dom";
import { api, type AdminOverview, type TlsStatus } from "../api";
import { useData, useNow } from "../data";
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

/** Page 1: what runs where — status tiles first (mirror, catalog, configuration, TLS), then this instance and every other one. */
export function OverviewPage() {
  const o: AdminOverview = useData("admin:overview", () => api.admin.overview(), 10_000);
  const now = useNow(10_000);
  const c = o.config;
  const me = o.instance;
  const mirror = mirrorState(o.mirror);
  const catalog = catalogState(o.catalog);
  const restarts = o.instances.filter((i) => i.restart_required.length > 0).length + (me.restart_required.length > 0 && !o.instances.some((i) => i.instance === me.id) ? 1 : 0);
  return (
    <div className="admin-overview">
      {c.error && (
        <Notice tone="danger" title="The configuration store is unreachable">
          {c.error} Every instance keeps serving git with the settings it last applied.
        </Notice>
      )}
      {me.apply_error && (
        <Notice tone="danger" title="This instance could not apply the latest settings">
          {me.apply_error}
        </Notice>
      )}
      {restarts > 0 && (
        <Notice tone="warn" title={`${restarts} ${restarts === 1 ? "instance needs" : "instances need"} a restart`}>
          Saved settings take effect on {restarts === 1 ? "it" : "them"} after a restart: {[...new Set([...me.restart_required, ...o.instances.flatMap((i) => i.restart_required)])].join(", ")}.
        </Notice>
      )}
      <ul className="tiles" aria-label="Status">
        <Tile to="/_admin/mirror" icon="mirror" title="GitHub mirroring" tone={mirror.tone} state={mirror.label} detail={mirror.detail} />
        <Tile to="/_admin/catalog" icon="database" title="Audit catalog" tone={catalog.tone} state={catalog.label} detail={catalog.detail} />
        <Tile
          to="/_admin/history"
          icon="history"
          title="Configuration"
          tone={c.error ? "danger" : "info"}
          state={c.revision ? `Revision ${c.revision}` : "Defaults"}
          detail={c.revision ? `Saved ${when(c.updated_at)} by ${c.author ?? "?"}` : "Nothing saved yet: the built-in defaults apply."}
        />
        <RouteBoundary fallback={<li className="tile" aria-busy="true" />}>
          <TlsTile />
        </RouteBoundary>
      </ul>

      <div className="admin-cols">
        <Box title={<h2 className="box-title">This instance</h2>}>
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
        <Box title={<h2 className="box-title">Configuration store</h2>}>
          <KV
            rows={[
              ["Location", <code key="loc">{o.store.location}</code>],
              ["Last message", c.message || "—"],
              ["History kept as", o.store.history_mode === "versions" ? "Bucket object versions" : "floe history records"],
              [
                "Encryption key",
                o.store.sealing_key ? (
                  <StatusBadge key="k" tone="ok">
                    Set on this host
                  </StatusBadge>
                ) : (
                  <span key="k">
                    <StatusBadge tone="warn">Not set</StatusBadge> <span className="muted small">Secrets can only be environment variables until <code>{o.store.key_env}</code> is set.</span>
                  </span>
                ),
              ],
            ]}
          />
        </Box>
      </div>

      <Box
        title={
          <>
            <h2 className="box-title">Instances</h2>
            <span className="pill">{o.instances.length}</span>
          </>
        }
      >
        {o.instances.length === 0 ? (
          <EmptyState icon="pulse" title="No heartbeats yet">
            <p>Instances report in within a minute of starting.</p>
          </EmptyState>
        ) : (
          <div className="scroll-x">
            <table className="grid">
              <thead>
                <tr>
                  <th scope="col">Instance</th>
                  <th scope="col">Revision</th>
                  <th scope="col">Last seen</th>
                  <th scope="col">Status</th>
                </tr>
              </thead>
              <tbody>
                {o.instances.map((i) => {
                  const stale = now - new Date(i.seen_at).getTime() > STALE_MS;
                  return (
                    <tr key={i.instance} className={stale ? "stale" : undefined}>
                      <td>
                        <code>{i.instance}</code>
                        <div className="muted small">
                          {i.version} · {i.roles.join(", ")}
                        </div>
                      </td>
                      <td className="tabular">{i.applied_revision}</td>
                      <td>{relTime(i.seen_at)}</td>
                      <td>
                        {i.apply_error ? (
                          <StatusBadge tone="danger" title={i.apply_error}>
                            Not applied
                          </StatusBadge>
                        ) : i.restart_required.length > 0 ? (
                          <StatusBadge tone="warn" title={i.restart_required.join(", ")}>
                            Restart required
                          </StatusBadge>
                        ) : stale ? (
                          <StatusBadge tone="neutral">Not seen for 10 min</StatusBadge>
                        ) : (
                          <StatusBadge tone="ok">Up to date</StatusBadge>
                        )}
                        {i.apply_error && <div className="field-error">{i.apply_error}</div>}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Box>

      <Box id="tls" title={<h2 className="box-title">TLS certificate</h2>}>
        <RouteBoundary fallback={<p className="muted pad">Loading…</p>}>
          <TlsBox />
        </RouteBoundary>
      </Box>
    </div>
  );
}

function Tile({ to, icon, title, tone, state, detail }: { to: string; icon: IconName; title: string; tone: Tone; state: string; detail: ReactNode }) {
  return (
    <li className="tile">
      <Link to={to} className="tile-link">
        <span className="tile-head">
          <Icon name={icon} />
          <span className="tile-title">{title}</span>
          <Icon name="arrow-right" className="icon tile-arrow" />
        </span>
        <StatusBadge tone={tone}>{state}</StatusBadge>
        <span className="tile-detail">{detail}</span>
      </Link>
    </li>
  );
}

function TlsTile() {
  const tls: TlsStatus = useData("admin:tls", () => api.tls(), 60_000);
  const now = useNow(60_000);
  let tone: Tone = "ok";
  let state = "Valid";
  let detail: ReactNode = tls.not_after ? `Expires ${relTime(tls.not_after)}` : "";
  if (tls.mode === "off") {
    tone = "neutral";
    state = "Off here";
    detail = "Plain HTTP on this instance; an edge terminates TLS.";
  } else if (tls.last_error) {
    tone = "danger";
    state = "Renewal failing";
    detail = tls.last_error;
  } else if (!tls.loaded) {
    tone = "warn";
    state = "No certificate yet";
  } else if (tls.not_after && new Date(tls.not_after).getTime() - now < 14 * 24 * 3600 * 1000) {
    tone = "warn";
    state = "Expires soon";
  }
  return (
    <li className="tile">
      <a href="#tls" className="tile-link">
        <span className="tile-head">
          <Icon name="shield" />
          <span className="tile-title">TLS certificate</span>
        </span>
        <StatusBadge tone={tone}>{state}</StatusBadge>
        <span className="tile-detail">{detail}</span>
      </a>
    </li>
  );
}

/** Read-only TLS status from `GET /api/v1/tls` (D59). TLS is bootstrap config (`[server.tls]`): never editable here. */
function TlsBox() {
  const tls: TlsStatus = useData("admin:tls", () => api.tls(), 60_000);
  const now = useNow(60_000);
  if (tls.mode === "off") {
    return (
      <p className="muted pad">
        TLS is off on this instance (plain HTTP: loopback, or an edge terminates TLS). It is set in the startup configuration file (<code>[server.tls]</code>), not here.
      </p>
    );
  }
  const expires = tls.not_after ? new Date(tls.not_after).getTime() - now : null;
  const soon = expires !== null && expires < 14 * 24 * 3600 * 1000;
  return (
    <>
      {!tls.loaded && <Notice tone="warn">No certificate loaded yet.</Notice>}
      {tls.last_error && (
        <Notice tone="danger" title={`Last renewal failed${tls.failures > 1 ? ` (${tls.failures} times)` : ""}`}>
          {tls.last_error}
        </Notice>
      )}
      {soon && <Notice tone="warn">The certificate expires {relTime(tls.not_after ?? "")}.</Notice>}
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
