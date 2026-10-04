import { NavLink, Outlet } from "react-router-dom";
import { api, type Me } from "../api";
import { useData } from "../data";
import { RouteBoundary } from "../components/Loading";
import "./admin.css";

const TABS: [string, string][] = [
  ["", "Overview"],
  ["mirror", "GitHub mirroring"],
  ["catalog", "Catalog"],
  ["events", "Events webhook"],
  ["repos", "Repositories"],
  ["history", "Config history"],
];

/** `/_admin/*` (D62): the runtime config document and fleet status, for admin principals. The API enforces it; this only hides the pages. */
export function AdminLayout() {
  const me: Me = useData("me", () => api.me(), 60_000);
  if (!me.admin) {
    return (
      <div className="blankslate" role="alert">
        <h1>Administrators only</h1>
        <p>
          Signed in as <strong>{me.principal}</strong>, who is not an admin. Admins are <code>tokens[].admin</code>, or <code>admin_emails</code> /{" "}
          <code>admin_domains</code> in oidc mode.
        </p>
      </div>
    );
  }
  return (
    <div className="admin">
      <h1 className="page-title">Administration</h1>
      <nav className="subtabs" aria-label="Administration sections">
        {TABS.map(([to, label]) => (
          <NavLink key={to} end={to === ""} to={to ? `/_admin/${to}` : "/_admin"} className={({ isActive }) => (isActive ? "subtab active" : "subtab")}>
            {label}
          </NavLink>
        ))}
      </nav>
      <RouteBoundary>
        <Outlet />
      </RouteBoundary>
    </div>
  );
}
