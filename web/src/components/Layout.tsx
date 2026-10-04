import { Link, NavLink, Outlet, useLocation } from "react-router-dom";
import { useEffect, useState, type ReactNode } from "react";
import { client } from "../api";
import { RouteBoundary, TopProgress, useBusy } from "./Loading";
import { ErrorTray } from "./ErrorTray";
import { InstanceFooter } from "./InstanceFooter";

export function Layout() {
  const busy = useBusy();
  // On a repo page the API tab pre-fills that repo in the examples.
  const m = /^\/([^/_][^/]*)\/([^/]+)/.exec(useLocation().pathname);
  const apiHref = m && m[1] !== "services" ? `/api?repo=${m[1]}/${m[2]}` : "/api";
  // The Admin link only for admins (D62); the API enforces it either way.
  const [admin, setAdmin] = useState(false);
  useEffect(() => {
    let live = true;
    client.me().then(
      (me) => {
        if (live) setAdmin(me.admin);
      },
      () => {},
    );
    return () => {
      live = false;
    };
  }, []);
  return (
    <>
      <header className="topbar">
        <Link to="/" className="brand">
          <svg viewBox="0 0 16 16" width="24" height="24" aria-hidden>
            <path
              fill="currentColor"
              d="M2 2.5A2.5 2.5 0 0 1 4.5 0h8.75a.75.75 0 0 1 .75.75v12.5a.75.75 0 0 1-.75.75h-2.5a.75.75 0 0 1 0-1.5h1.75v-2h-8a1 1 0 0 0-.714 1.7.75.75 0 1 1-1.072 1.05A2.495 2.495 0 0 1 2 11.5Zm10.5-1h-8a1 1 0 0 0-1 1v6.708A2.486 2.486 0 0 1 4.5 9h8ZM5 12.25a.25.25 0 0 1 .25-.25h3.5a.25.25 0 0 1 .25.25v3.25a.25.25 0 0 1-.4.2l-1.45-1.087a.25.25 0 0 0-.3 0L5.4 15.7a.25.25 0 0 1-.4-.2Z"
            />
          </svg>
          floe
        </Link>
        <nav className="topnav">
          <NavLink to={apiHref} className={({ isActive }) => (isActive ? "topnav-link active" : "topnav-link")}>
            API
          </NavLink>
          {admin && (
            <NavLink to="/_admin" className={({ isActive }) => (isActive ? "topnav-link active" : "topnav-link")}>
              Admin
            </NavLink>
          )}
        </nav>
      </header>
      <TopProgress />
      <main className="container" aria-busy={busy}>
        <RouteBoundary>
          <Outlet />
        </RouteBoundary>
      </main>
      <ErrorTray />
      <InstanceFooter />
    </>
  );
}

export function Box({
  title,
  children,
  className = "",
  id,
}: {
  title?: ReactNode;
  children: ReactNode;
  className?: string;
  id?: string;
}) {
  return (
    <div className={`box ${className}`} id={id}>
      {title && <div className="box-header">{title}</div>}
      {children}
    </div>
  );
}
