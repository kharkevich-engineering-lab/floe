import { useEffect, useState } from "react";
import logo from "../assets/logo.svg";

/** Which instance answered — kind is the loud part (a serverless host vs the SSD host must
 * be distinguishable at a glance), then the name, revision, build. */
export interface InstanceInfo {
  kind: "serverless" | "ssd" | "dev" | string;
  name: string;
  revision: string;
  instance: string;
  version: string;
  roles: string[];
  disk: "tmpfs" | "ssd" | string;
}

const LABEL: Record<string, string> = { serverless: "Serverless host", ssd: "SSD host", dev: "Development" };

/** The page footer: the Lab lockup and which machine answered this page. */
export function InstanceFooter() {
  const [info, setInfo] = useState<InstanceInfo | null>(null);
  useEffect(() => {
    let live = true;
    // Non-repo instance facts live at /services/api/instance (D27: /api/v1 is discovery/me/owners only).
    fetch("/services/api/instance", { headers: { Accept: "application/json" }, credentials: "same-origin" })
      .then((r) => (r.ok ? r.json() : null))
      .then((j) => live && j && setInfo(j as InstanceInfo))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);
  const where = info ? (info.kind === "serverless" ? `${info.revision || info.name}${info.instance ? ` · ${info.instance}` : ""}` : info.name) : "";
  return (
    <footer className="site-footer">
      <div className="footer-brand">
        <img src={logo} alt="" width={24} height={24} />
        <span>
          <strong>floe</strong> — a Kharkevich Engineering Lab product
        </span>
      </div>
      {info && (
        <p className={`instance kind-${info.kind}`} title={`Roles: ${info.roles.join(", ")} · disk: ${info.disk}`}>
          <span className="instance-kind">{LABEL[info.kind] ?? info.kind}</span>
          <span className="instance-where">{where}</span>
          <span className="instance-version">{info.version}</span>
        </p>
      )}
    </footer>
  );
}
