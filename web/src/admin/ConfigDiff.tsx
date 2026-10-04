import type { ConfigDiffEntry } from "../api";

const show = (v: unknown) => (v === undefined ? "" : typeof v === "string" ? v : JSON.stringify(v));

/** A config diff (flattened paths; secrets arrive masked from the server). */
export function ConfigDiff({ diff, empty = "No changes." }: { diff: ConfigDiffEntry[]; empty?: string }) {
  if (diff.length === 0) return <p className="muted pad">{empty}</p>;
  return (
    <ul className="config-diff" aria-label="Changes">
      {diff.map((d) => (
        <li key={`${d.op}:${d.path}`} className={d.op}>
          {d.op === "added" && <>+ {d.path} = {show(d.new)}</>}
          {d.op === "removed" && <>− {d.path} (was {show(d.old)})</>}
          {d.op === "changed" && (
            <>
              ~ {d.path}: {show(d.old)} → {show(d.new)}
            </>
          )}
        </li>
      ))}
    </ul>
  );
}
