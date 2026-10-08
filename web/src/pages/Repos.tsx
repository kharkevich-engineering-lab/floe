import { Link, useParams } from "react-router-dom";
import { api } from "../api";
import { useData } from "../data";
import { Box } from "../components/Layout";
import { CopyButton } from "../components/CopyButton";
import { Icon } from "../components/Icon";
import { EmptyState, PageHeader } from "../components/ui";

export function Repos() {
  const { owner = "" } = useParams();
  const repos = useData(`repos:${owner}`, () => api.repos(owner));
  return (
    <>
      <PageHeader
        eyebrow={
          <Link to="/">
            Repositories
          </Link>
        }
        title={owner}
        description={repos.length === 0 ? undefined : `${repos.length} ${repos.length === 1 ? "repository" : "repositories"}`}
      />
      <Box>
        {repos.length === 0 ? (
          <EmptyState icon="repo" title={`No repositories under ${owner}`}>
            <p>
              Push to <code>{location.origin}/{owner}/repository.git</code> to create one.
            </p>
          </EmptyState>
        ) : (
          <ul className="list repo-list">
            {repos.map((r) => {
              const clone = `git -c transfer.bundleURI=true clone ${location.origin}/${owner}/${r}.git`;
              return (
                <li key={r}>
                  <Link to={`/${owner}/${r}`} className="repo-link">
                    <Icon name="repo" /> {r}
                  </Link>
                  <Link to={`/${owner}/${r}/commits`} className="small">
                    Commits
                  </Link>
                  <div className="clone-line small">
                    <code>{clone}</code>
                    <CopyButton text={clone} className="inline" />
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      </Box>
    </>
  );
}
