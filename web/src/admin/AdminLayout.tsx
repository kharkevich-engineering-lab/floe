import { NavLink, Outlet } from "react-router-dom";
import { api, type Me } from "../api";
import { useData } from "../data";
import { RouteBoundary } from "../components/Loading";
import { Icon, type IconName } from "../components/Icon";
import { EmptyState, PageHeader } from "../components/ui";
import "./admin.css";

const NAV: { to: string; label: string; icon: IconName; group: string }[] = [
  { to: "", label: "Overview", icon: "grid", group: "Status" },
  { to: "mirror", label: "GitHub mirroring", icon: "mirror", group: "Runtime settings" },
  { to: "catalog", label: "Audit catalog", icon: "database", group: "Runtime settings" },
  { to: "events", label: "Events webhook", icon: "webhook", group: "Runtime settings" },
  { to: "repos", label: "Repository settings", icon: "repo", group: "Repositories" },
  { to: "history", label: "Change history", icon: "history", group: "Audit" },
];

/** `/_admin/*` (D62): the runtime config document and fleet status, for admin principals. The API enforces it; this only hides the pages. */
export function AdminLayout() {
  const me: Me = useData("me", () => api.me(), 60_000);
  if (!me.admin) {
    return (
      <EmptyState icon="shield" title="Administrators only">
        <p>
          You are signed in as <strong>{me.principal}</strong>, who cannot administer this floe. Administrators are tokens marked <code>admin</code>, or — with
          single sign-on — the email addresses and domains listed as administrators in the server configuration.
        </p>
      </EmptyState>
    );
  }
  const groups = [...new Set(NAV.map((n) => n.group))];
  return (
    <div className="admin">
      <PageHeader eyebrow="floe" title="Administration" description="Fleet status and the settings every floe instance reads at runtime." />
      <div className="admin-shell">
        <nav className="admin-nav" aria-label="Administration">
          {groups.map((g) => (
            <div key={g} className="admin-nav-group">
              <p className="admin-nav-heading">{g}</p>
              <ul>
                {NAV.filter((n) => n.group === g).map((n) => (
                  <li key={n.to}>
                    <NavLink end={n.to === ""} to={n.to ? `/_admin/${n.to}` : "/_admin"} className={({ isActive }) => (isActive ? "admin-nav-link active" : "admin-nav-link")}>
                      <Icon name={n.icon} />
                      {n.label}
                    </NavLink>
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </nav>
        <div className="admin-main">
          <RouteBoundary>
            <Outlet />
          </RouteBoundary>
        </div>
      </div>
    </div>
  );
}
