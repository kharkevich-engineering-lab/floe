import { Link, NavLink, Outlet, useLocation } from "react-router-dom";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { client, type Me } from "../api";
import { RouteBoundary, TopProgress, useBusy } from "./Loading";
import { ErrorTray } from "./ErrorTray";
import { InstanceFooter } from "./InstanceFooter";
import { Icon } from "./Icon";
import { THEME_LABEL, nextTheme, useTheme } from "../theme";
import logo from "../assets/logo.svg";

const navClass = ({ isActive }: { isActive: boolean }) => (isActive ? "nav-link active" : "nav-link");

/** The app shell: product lockup, primary navigation, theme and user menu; the page; the footer. */
export function Layout() {
  const busy = useBusy();
  const { pathname } = useLocation();
  // On a repo page the API tab pre-fills that repo in the examples.
  const m = /^\/([^/_][^/]*)\/([^/]+)/.exec(pathname);
  const apiHref = m && m[1] !== "services" ? `/api?repo=${m[1]}/${m[2]}` : "/api";
  // The Admin link only for admins (D62); the API enforces it either way.
  const [me, setMe] = useState<Me | null>(null);
  useEffect(() => {
    let live = true;
    client.me().then(
      (v) => {
        if (live) setMe(v);
      },
      () => {},
    );
    return () => {
      live = false;
    };
  }, []);
  const [menuOpen, setMenuOpen] = useState(false);
  // Close the mobile menu on navigation.
  const [lastPath, setLastPath] = useState(pathname);
  if (lastPath !== pathname) {
    setLastPath(pathname);
    setMenuOpen(false);
  }
  const reposActive = pathname === "/" || (!pathname.startsWith("/_admin") && !pathname.startsWith("/api"));
  const links = (
    <>
      <NavLink to="/" className={() => (reposActive ? "nav-link active" : "nav-link")} aria-current={reposActive ? "page" : undefined}>
        Repositories
      </NavLink>
      <NavLink to={apiHref} className={navClass}>
        API
      </NavLink>
      {me?.admin && (
        <NavLink to="/_admin" className={navClass}>
          Administration
        </NavLink>
      )}
    </>
  );
  return (
    <>
      <a className="skip-link" href="#main">
        Skip to content
      </a>
      <header className="site-header">
        <Link to="/" className="brand" aria-label="floe — home">
          <img src={logo} alt="" width={32} height={32} />
          <span>
            <strong>floe</strong>
            <small>Kharkevich Engineering Lab</small>
          </span>
        </Link>
        <nav className="primary-nav" aria-label="Primary">
          {links}
        </nav>
        <div className="header-actions">
          <ThemeToggle />
          {me && <UserMenu me={me} />}
          <button
            type="button"
            className="icon-button menu-button"
            aria-label={menuOpen ? "Close navigation menu" : "Open navigation menu"}
            aria-expanded={menuOpen}
            aria-controls="mobile-nav"
            onClick={() => setMenuOpen((o) => !o)}
          >
            <Icon name={menuOpen ? "close" : "menu"} size={20} />
          </button>
        </div>
      </header>
      {menuOpen && (
        <nav className="mobile-nav" id="mobile-nav" aria-label="Primary">
          {links}
        </nav>
      )}
      <TopProgress />
      <main id="main" className="container" aria-busy={busy} tabIndex={-1}>
        <RouteBoundary>
          <Outlet />
        </RouteBoundary>
      </main>
      <InstanceFooter />
      <ErrorTray />
    </>
  );
}

function ThemeToggle() {
  const { theme } = useTheme();
  const label = THEME_LABEL[theme];
  return (
    <button type="button" className="icon-button" onClick={nextTheme} aria-label={label} title={label}>
      <Icon name={theme === "system" ? "sun-moon" : theme === "light" ? "sun" : "moon"} size={20} />
    </button>
  );
}

/** Who is signed in, what they may do, and (when sign-in is by browser session) tokens and sign-out. */
function UserMenu({ me }: { me: Me }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const id = useId();
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setOpen(false);
        ref.current?.querySelector("button")?.focus();
      }
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);
  const name = me.anonymous ? "Anonymous" : me.principal;
  const roles: ReactNode[] = [];
  roles.push(<li key="r">Read</li>);
  if (me.write) roles.push(<li key="w">Push</li>);
  if (me.admin) roles.push(<li key="a">Administer</li>);
  return (
    <div className="user-menu" ref={ref}>
      <button type="button" className="user-button" aria-expanded={open} aria-controls={id} onClick={() => setOpen((o) => !o)}>
        <span className="user-avatar" aria-hidden>
          {name.slice(0, 1).toUpperCase()}
        </span>
        <span className="user-name">{name}</span>
        <Icon name="chevron" size={14} />
      </button>
      {open && (
        <div className="popover user-pop" id={id}>
          <p className="eyebrow">Signed in as</p>
          <p className="user-pop-name">{name}</p>
          <ul className="role-list" aria-label="Permissions">
            {roles}
          </ul>
          <div className="user-pop-links">
            <a href="/_auth/tokens">Access tokens</a>
            {!me.anonymous && <a href="/_auth/logout">Sign out</a>}
          </div>
        </div>
      )}
    </div>
  );
}

/** A titled panel. Kept for the pages that compose with it; `title` renders in the panel's header row. */
export function Box({ title, children, className = "", id }: { title?: ReactNode; children: ReactNode; className?: string; id?: string }) {
  return (
    <section className={`box ${className}`} id={id}>
      {title && <div className="box-header">{title}</div>}
      {children}
    </section>
  );
}
