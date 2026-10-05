import { StrictMode, lazy } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import { Layout } from "./components/Layout";
import { Owners } from "./pages/Owners";
import { Repos } from "./pages/Repos";
import { RepoLayout } from "./pages/RepoLayout";
import { TreePage } from "./pages/TreePage";
import { CommitsPage } from "./pages/CommitsPage";
import { track } from "./data";
import "./styles.css";

// Heavy pages (syntax highlighting / diff rendering / WAL dashboard) are split
// into their own chunks and only downloaded when a user navigates to them.
// `track` shows the chunk download in the top progress bar; the route
// boundaries in Layout/RepoLayout provide the Suspense fallbacks.
const BlobPage = lazy(() => track(import("./pages/BlobPage")).then((m) => ({ default: m.BlobPage })));
const CommitPage = lazy(() => track(import("./pages/CommitPage")).then((m) => ({ default: m.CommitPage })));
const OverviewPage = lazy(() => track(import("./pages/OverviewPage")).then((m) => ({ default: m.OverviewPage })));
const SettingsPage = lazy(() => track(import("./pages/SettingsPage")).then((m) => ({ default: m.SettingsPage })));
const ApiPage = lazy(() => track(import("./pages/ApiPage")).then((m) => ({ default: m.ApiPage })));
// The admin area (D62): its own chunks, downloaded only by admins who open it.
const AdminLayout = lazy(() => track(import("./admin/AdminLayout")).then((m) => ({ default: m.AdminLayout })));
const AdminOverview = lazy(() => track(import("./admin/OverviewPage")).then((m) => ({ default: m.OverviewPage })));
const AdminMirror = lazy(() => track(import("./admin/MirrorPage")).then((m) => ({ default: m.MirrorPage })));
const AdminCatalog = lazy(() => track(import("./admin/CatalogPage")).then((m) => ({ default: m.CatalogPage })));
const AdminEvents = lazy(() => track(import("./admin/EventsPage")).then((m) => ({ default: m.EventsPage })));
const AdminRepos = lazy(() => track(import("./admin/ReposPage")).then((m) => ({ default: m.ReposPage })));
const AdminHistory = lazy(() => track(import("./admin/HistoryPage")).then((m) => ({ default: m.HistoryPage })));

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <BrowserRouter>
      <Routes>
        <Route element={<Layout />}>
          <Route index element={<Owners />} />
          <Route path="api" element={<ApiPage />} />
          <Route path="_admin" element={<AdminLayout />}>
            <Route index element={<AdminOverview />} />
            <Route path="mirror" element={<AdminMirror />} />
            <Route path="catalog" element={<AdminCatalog />} />
            <Route path="events" element={<AdminEvents />} />
            <Route path="repos" element={<AdminRepos />} />
            <Route path="history" element={<AdminHistory />} />
          </Route>
          <Route path=":owner" element={<Repos />} />
          <Route path=":owner/:repo" element={<RepoLayout />}>
            <Route index element={<TreePage />} />
            <Route path="tree/*" element={<TreePage />} />
            <Route path="blob/*" element={<BlobPage />} />
            <Route path="wal" element={<OverviewPage />} />
            <Route path="settings" element={<SettingsPage />} />
            <Route path="commits" element={<CommitsPage />} />
            <Route path="commits/*" element={<CommitsPage />} />
            <Route path="commit/:sha" element={<CommitPage />} />
          </Route>
        </Route>
      </Routes>
    </BrowserRouter>
  </StrictMode>,
);
