import { Suspense, createContext, useContext, useEffect, useMemo, useRef, useState } from "react";
import { Link, NavLink, Outlet, useLocation, useParams } from "react-router-dom";
import { api, type Refs } from "../api";
import { useData } from "../data";
import { RouteBoundary, Skeleton } from "../components/Loading";
import { CloneSetup } from "../components/CloneSetup";
import { TasksOverlay } from "../components/TasksOverlay";
import { Icon } from "../components/Icon";
import "../clone.css";

/** "Clone" dropdown: recipes come from `/services/setup.json` (cached after the first open). */
function CloneMenu({ full }: { full: string }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
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
  return (
    <div className="clone-menu" ref={ref}>
      <button type="button" className="btn btn-primary" onClick={() => setOpen((o) => !o)} aria-expanded={open} aria-controls="clone-pop">
        <Icon name="code" /> Clone <Icon name="chevron" size={14} />
      </button>
      {open && (
        <div className="clone-pop" id="clone-pop" role="dialog" aria-label={`Clone ${full}`}>
          <Suspense fallback={<Skeleton title={false} rows={4} />}>
            <CloneSetup repo={full} compact />
          </Suspense>
        </div>
      )}
    </div>
  );
}

export interface RepoCtx {
  owner: string;
  name: string;
  full: string; // owner/name
  refs: Refs;
}

const Ctx = createContext<RepoCtx | null>(null);

export function useRepo(): RepoCtx {
  const c = useContext(Ctx);
  if (!c) throw new Error("useRepo outside RepoLayout");
  return c;
}

/** Repo shell. The header (title, Clone, tabs) is static and paints at once;
 * only the body waits (Suspense skeleton) for the `refs` request, and page
 * navigations inside the repo keep the shell while the next page loads. */
export function RepoLayout() {
  const { owner = "", repo = "" } = useParams();
  const full = `${owner}/${repo}`;
  const { pathname } = useLocation();
  useEffect(() => {
    document.title = `${full} · floe`;
    return () => {
      document.title = "floe";
    };
  }, [full]);
  const walActive = pathname.endsWith("/wal");
  const settingsActive = pathname.endsWith("/settings");
  const codeActive = !walActive && !settingsActive && !/\/commits?(\/|$)/.test(pathname);
  const commitsActive = !codeActive && !walActive && !settingsActive;
  return (
    <>
      <div className="repo-head">
        <h1 className="repo-title">
          <Icon name="repo" size={20} />
          <Link to={`/${owner}`} className="owner">
            {owner}
          </Link>
          <span className="muted" aria-hidden>
            /
          </span>
          <Link to={`/${full}`}>{repo}</Link>
        </h1>
        <CloneMenu full={full} />
        <nav className="tabs" aria-label="Repository">
          <NavLink to={`/${full}`} className={() => (codeActive ? "tab active" : "tab")} aria-current={codeActive ? "page" : undefined} end>
            <Icon name="code" /> Code
          </NavLink>
          <NavLink to={`/${full}/commits`} className={() => (commitsActive ? "tab active" : "tab")} aria-current={commitsActive ? "page" : undefined}>
            <Icon name="commit" /> Commits
          </NavLink>
          <NavLink to={`/${full}/wal`} className={() => (walActive ? "tab active" : "tab")}>
            <Icon name="pulse" /> WAL
          </NavLink>
          <NavLink to={`/${full}/settings`} className={() => (settingsActive ? "tab active" : "tab")}>
            <Icon name="settings" /> Settings
          </NavLink>
          <TasksOverlay repo={full} />
        </nav>
      </div>
      <RouteBoundary fallback={<Skeleton title={false} rows={8} />}>
        <RepoBody owner={owner} repo={repo} full={full} />
      </RouteBoundary>
    </>
  );
}

function RepoBody({ owner, repo, full }: { owner: string; repo: string; full: string }) {
  const refs = useData(`refs:${full}`, () => api.refs(full));
  const ctx = useMemo(() => ({ owner, name: repo, full, refs }), [owner, repo, full, refs]);
  return (
    <Ctx.Provider value={ctx}>
      <Outlet />
    </Ctx.Provider>
  );
}
