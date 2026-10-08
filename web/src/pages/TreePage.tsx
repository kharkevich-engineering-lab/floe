import { Link, useParams } from "react-router-dom";
import { api } from "../api";
import { useResolved } from "../use-resolved";
import { useRepo } from "./RepoLayout";
import { Box } from "../components/Layout";
import { fmtSize, relTime } from "../format";
import { RefBar } from "../components/RefBar";
import { Markdown } from "../components/Markdown";
import { Avatar } from "../components/CommitRow";
import { Icon } from "../components/Icon";
import { EmptyState } from "../components/ui";
import { CodeSample } from "../components/CopyButton";

export function TreePage() {
  const { full, refs } = useRepo();
  const rest = useParams()["*"] ?? "";
  if (!refs.head) {
    return (
      <Box>
        <EmptyState icon="repo" title="This repository is empty">
          <p>Push your first commit to start it:</p>
        </EmptyState>
        <div className="pad" style={{ paddingTop: 0, maxWidth: 720, margin: "0 auto" }}>
          <CodeSample code={`git remote add origin ${location.origin}/${full}.git\ngit push -u origin HEAD`} />
        </div>
      </Box>
    );
  }
  return <TreeView full={full} rest={rest} />;
}

function TreeView({ full, rest }: { full: string; rest: string }) {
  const { r, data: t } = useResolved(full, rest, (res) => api.tree(full, res.sha, res.path));
  const base = `/${full}`;
  const up = t.path.split("/").slice(0, -1).join("/");
  return (
    <>
      <RefBar refname={r.ref} refKind={r.kind} path={r.path} page="tree" />
      <Box
        className="tree"
        title={
          t.commit && (
            <div className="tree-commit">
              <Avatar name={t.commit.author} />
              <strong>{t.commit.author}</strong>
              <Link to={`${base}/commit/${t.commit.sha}`} className="commit-subject ellipsis">
                {t.commit.subject}
              </Link>
              <span className="spacer" />
              <Link to={`${base}/commit/${t.commit.sha}`} className="sha">
                {t.commit.sha.slice(0, 7)}
              </Link>
              <span className="muted small">{relTime(t.commit.commit_date)}</span>
            </div>
          )
        }
      >
        <table className="files">
          <caption className="sr-only">Files in {t.path || "the repository root"}</caption>
          <tbody>
            {t.path && (
              <tr>
                <td className="icon" aria-hidden />
                <td colSpan={2}>
                  <Link to={`${base}/tree/${t.ref}${up ? "/" + up : ""}`}>..</Link>
                </td>
              </tr>
            )}
            {t.entries.map((e) => (
              <tr key={e.name}>
                <td className="icon">{e.type === "tree" ? <DirIcon /> : e.type === "commit" ? <Icon name="submodule" /> : <FileIcon />}</td>
                <td>
                  {e.type === "commit" ? (
                    <span title={`submodule @ ${e.sha}`}>{e.name}</span>
                  ) : (
                    <Link to={`${base}/${e.type === "tree" ? "tree" : "blob"}/${t.ref}/${t.path ? t.path + "/" : ""}${e.name}`}>
                      {e.name}
                    </Link>
                  )}
                </td>
                <td className="muted small right">{e.size >= 0 ? fmtSize(e.size) : ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </Box>
      {t.readme && (
        <Box title={<span className="strong">{t.readme.name}</span>} className="readme">
          <div className="pad">
            <Markdown source={t.readme.contents} />
          </div>
        </Box>
      )}
    </>
  );
}

function DirIcon() {
  return <Icon name="folder" className="icon dir" />;
}

function FileIcon() {
  return <Icon name="file" />;
}
