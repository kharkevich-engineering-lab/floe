import { Link } from "react-router-dom";
import { api } from "../api";
import { useData } from "../data";
import { Hero } from "../components/Hero";
import { CodeSample } from "../components/CopyButton";
import { Icon } from "../components/Icon";
import { EmptyState, PageHeader } from "../components/ui";

export function Owners() {
  const owners = useData("owners", api.owners);
  if (owners.length === 0) {
    return <BlankSlate />;
  }
  return (
    <>
      <Hero />
      <PageHeader level={2} eyebrow="Browse" title="Repositories by owner" description={`${owners.length} ${owners.length === 1 ? "owner" : "owners"} on this host.`} />
      <ul className="owner-grid">
        {owners.map((o) => (
          <li key={o}>
            <Link to={`/${o}`} className="owner-card">
              <Icon name="folder" size={20} />
              <span className="ellipsis">{o}</span>
              <Icon name="arrow-right" className="arrow" />
            </Link>
          </li>
        ))}
      </ul>
    </>
  );
}

/** First repo: install.sh (this host:port) sets helper + proactiveAuth + origin. */
function BlankSlate() {
  const origin = window.location.origin;
  const host = window.location.host;
  const install = `sh -c "$(curl -fsSLk '${origin}/services/public/install.sh')" -- area/repository`;
  return (
    <>
      <Hero />
      <section className="box">
        <EmptyState icon="repo" title="No repositories yet">
          <p>
            Push your first one. From a local git tree, run the installer with <code>area/repository</code> — it turns on{" "}
            <code>http.https://{host}/.proactiveAuth=auto</code> (git must send a token up front) and points <code>origin</code> at{" "}
            <code>{origin}/area/repository.git</code>. Then push. Anything but <code>area/repository.git</code> is refused.
          </p>
        </EmptyState>
        <div className="pad" style={{ paddingTop: 0, maxWidth: 760, margin: "0 auto" }}>
          <p className="eyebrow">Once, from your repository</p>
          <CodeSample code={`${install}\ngit push -u origin HEAD`} />
          <p className="muted small" style={{ marginTop: 12 }}>
            <code>area</code> and <code>repository</code> use letters, digits, <code>.</code>, <code>_</code> and <code>-</code>, 1 to 100 characters.
          </p>
        </div>
      </section>
    </>
  );
}
