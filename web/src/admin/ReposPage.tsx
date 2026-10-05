import { Suspense, useEffect, useId, useState, type ReactNode } from "react";
import { useSearchParams, Link } from "react-router-dom";
import { api, type Policy, type PolicyValidation, type RepoSettings, type SettingsValidation } from "../api";
import { invalidate, useData } from "../data";
import { Box } from "../components/Layout";
import { useUnsavedGuard } from "./useUnsavedGuard";
import { ListTextarea } from "./ListTextarea";
import { errorMessage } from "./SectionEditor";
import { tomlGet, tomlSet, type TomlValue } from "./toml-lines";

/** Page 4: per-repository settings (D24) and push policy (D16), each as a form plus the raw document. */
export function ReposPage() {
  const [params, setParams] = useSearchParams();
  const repo = params.get("repo") ?? "";
  const [text, setText] = useState(repo);
  const valid = /^[^/\s]+\/[^/\s]+$/.test(text.trim());
  return (
    <>
      <Box title="Repository">
        <form
          className="pad secret-row"
          onSubmit={(e) => {
            e.preventDefault();
            if (valid) setParams({ repo: text.trim() });
          }}
        >
          <label htmlFor="admin-repo">owner/name</label>
          <input id="admin-repo" type="text" className="text" list="admin-repos" value={text} onChange={(e) => setText(e.target.value)} placeholder="acme/monorepo" />
          <Suspense fallback={null}>
            <RepoOptions />
          </Suspense>
          <button type="submit" className="btn small" disabled={!valid}>
            Open
          </button>
          {repo && (
            <Link to={`/${repo}/settings`} className="small">
              repository Settings tab
            </Link>
          )}
        </form>
      </Box>
      {repo ? (
        <div key={repo}>
          <Suspense fallback={<Box title="Settings">loading…</Box>}>
            <SettingsForm repo={repo} />
          </Suspense>
          <Suspense fallback={<Box title="Push policy">loading…</Box>}>
            <PolicyForm repo={repo} />
          </Suspense>
        </div>
      ) : (
        <p className="muted pad">Pick a repository to edit its settings and push policy.</p>
      )}
    </>
  );
}

/** Suggestions for the picker: every owner's repositories (bounded). */
function RepoOptions() {
  const owners = useData("owners", () => api.owners(), 60_000);
  return (
    <datalist id="admin-repos">
      {owners.slice(0, 20).map((o) => (
        <Suspense key={o} fallback={null}>
          <OwnerOptions owner={o} />
        </Suspense>
      ))}
    </datalist>
  );
}

function OwnerOptions({ owner }: { owner: string }) {
  const repos = useData(`repos:${owner}`, () => api.repos(owner), 60_000);
  return (
    <>
      {repos.map((r) => (
        <option key={r} value={`${owner}/${r}`} />
      ))}
    </>
  );
}

/** The `[upstream]`/`[maintenance]`/`[bundles]`/`[compaction]` keys the form manages; everything else stays in the raw TOML. */
const SETTINGS_FIELDS: { section: string; key: string; label: string; kind: "string" | "list" | "bool" | "enum"; options?: string[]; help: string }[] = [
  { section: "upstream", key: "git", label: "Upstream git URL", kind: "string", help: "https:// only; tokens come from the host's env (never here)." },
  { section: "upstream", key: "follow", label: "Followed refs", kind: "list", help: "Ref patterns (refs/heads/*, ^refs/heads/tmp/*)." },
  { section: "upstream", key: "on_rewrite", label: "On rewrite", kind: "enum", options: ["", "archive", "refuse"], help: "archive keeps rewritten tips under refs/archive/." },
  { section: "upstream", key: "follow_interval", label: "Follow interval", kind: "string", help: "e.g. 10m; empty = the host's maintenance.follow_interval." },
  { section: "upstream", key: "lfs", label: "Upstream LFS URL", kind: "string", help: "Read-through for missing LFS objects." },
  { section: "bundles", key: "enabled", label: "Bundles", kind: "bool", help: "Cut bundle-uri bundles for this repository." },
  { section: "bundles", key: "main_only", label: "Bundles: main only", kind: "bool", help: "HEAD + refs/heads/main (+ extra_refs)." },
  { section: "compaction", key: "enabled", label: "Compaction", kind: "bool", help: "Geometric folding of fresh packs." },
];

function SettingsForm({ repo }: { repo: string }) {
  const saved: RepoSettings = useData(`settings-doc:${repo}`, () => api.settings(repo).get(), 10_000);
  const [text, setText] = useState(saved.toml);
  const [raw, setRaw] = useState(false);
  const [message, setMessage] = useState("");
  const [validation, setValidation] = useState<SettingsValidation | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const dirty = text !== saved.toml;
  useUnsavedGuard(dirty);
  useEffect(() => {
    if (!dirty) return;
    let cancelled = false;
    const t = setTimeout(() => {
      api
        .settings(repo)
        .validate(text)
        .then((v) => {
          if (!cancelled) setValidation(v);
        })
        .catch((e: unknown) => {
          if (!cancelled) setValidation({ ok: false, errors: [errorMessage(e)] });
        });
    }, 400);
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
  }, [text, dirty, repo]);
  const set = (section: string, key: string, v: TomlValue | null) => {
    setText((t) => tomlSet(t, section, key, v));
    setNote(null);
  };
  const save = async () => {
    setBusy(true);
    setNote(null);
    try {
      const r = await api.settings(repo).put(text, message);
      setNote({ ok: true, text: `Published settings revision ${r.revision}.` });
      setMessage("");
      invalidate(`settings-doc:${repo}`);
      invalidate(`settings:${repo}`);
    } catch (e) {
      setNote({ ok: false, text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  const errors = dirty && validation && !validation.ok ? validation.errors : [];
  return (
    <Box
      title={
        <span className="admin-head">
          <strong>Settings (D24)</strong>
          <span className="muted">{saved.revision ? `revision ${saved.revision} by ${saved.author}` : "none — the host config applies"}</span>
          <span className="spacer" />
          <button type="button" className="btn small" onClick={() => setRaw(!raw)} aria-pressed={raw}>
            {raw ? "Form view" : "Raw TOML"}
          </button>
        </span>
      }
    >
      {errors.map((e) => (
        <div key={e} className="notice error" role="alert">
          {e}
        </div>
      ))}
      {raw ? (
        <div className="editor">
          <textarea className="code-input" spellCheck={false} rows={Math.min(30, Math.max(8, text.split("\n").length + 1))} value={text} aria-label={`${repo} settings TOML`} onChange={(e) => setText(e.target.value)} />
        </div>
      ) : (
        <div className="form-group">
          {SETTINGS_FIELDS.map((f) => (
            <SettingsField key={`${f.section}.${f.key}`} f={f} value={tomlGet(text, f.section, f.key)} onChange={(v) => set(f.section, f.key, v)} />
          ))}
          <p className="muted small">Other keys (bundle strategies, maintenance) stay as written: edit them in the raw TOML.</p>
        </div>
      )}
      <div className="save-bar">
        <input type="text" className="text" placeholder="Why (recorded in the WAL)" aria-label="Settings change message" value={message} onChange={(e) => setMessage(e.target.value)} disabled={!dirty} />
        <button type="button" className="btn primary" disabled={!dirty || busy || !(validation?.ok ?? false)} onClick={save}>
          {busy ? "Publishing…" : "Publish settings"}
        </button>
        <button type="button" className="btn" disabled={!dirty || busy} onClick={() => setText(saved.toml)}>
          Discard
        </button>
        <span className="state" role="status" aria-live="polite">
          {!dirty ? "No unsaved changes." : validation === null ? "Checking…" : validation.ok ? "Valid." : `${validation.errors.length} problem(s).`}
        </span>
      </div>
      {note && (
        <div className={`notice ${note.ok ? "ok" : "error"}`} role={note.ok ? "status" : "alert"}>
          {note.text}
        </div>
      )}
    </Box>
  );
}

function SettingsField({ f, value, onChange }: { f: (typeof SETTINGS_FIELDS)[number]; value: TomlValue | undefined; onChange: (v: TomlValue | null) => void }) {
  const id = useId();
  let control: ReactNode;
  switch (f.kind) {
    case "bool":
      control = (
        <select id={id} value={value === undefined ? "" : String(value)} onChange={(e) => onChange(e.target.value === "" ? null : e.target.value === "true")}>
          <option value="">(host default)</option>
          <option value="true">on</option>
          <option value="false">off</option>
        </select>
      );
      break;
    case "enum":
      control = (
        <select id={id} value={typeof value === "string" ? value : ""} onChange={(e) => onChange(e.target.value === "" ? null : e.target.value)}>
          {(f.options ?? []).map((o) => (
            <option key={o} value={o}>
              {o || "(host default)"}
            </option>
          ))}
        </select>
      );
      break;
    case "list":
      control = (
        <ListTextarea id={id} rows={3} value={Array.isArray(value) ? value : []} onChange={(items) => onChange(items.length > 0 ? items : null)} />
      );
      break;
    default:
      control = <input id={id} type="text" value={typeof value === "string" ? value : ""} placeholder="(host default)" onChange={(e) => onChange(e.target.value.trim() === "" ? null : e.target.value)} />;
  }
  return (
    <div className="field">
      <label htmlFor={id}>{f.label}</label>
      <div className="control">
        {control}
        <p className="help">
          {f.help} <code className="muted">
            [{f.section}] {f.key}
          </code>
        </p>
      </div>
    </div>
  );
}

const RESTRICTS = ["create", "update", "delete", "force-push"] as const;

interface Rule {
  name?: string;
  match?: { refs?: string[] } & Record<string, unknown>;
  effect?: { protect?: { restricts?: string[] } & Record<string, unknown> } & Record<string, unknown>;
  [k: string]: unknown;
}

function PolicyForm({ repo }: { repo: string }) {
  const saved = useData<Policy>(`policy:${repo}`, () => api.policy(repo).get(), 10_000);
  const savedText = JSON.stringify(saved, null, 2);
  const [text, setText] = useState(savedText);
  const [raw, setRaw] = useState(false);
  const [validation, setValidation] = useState<PolicyValidation | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const dirty = text !== savedText;
  useUnsavedGuard(dirty);
  let doc: Record<string, unknown> | null = null;
  try {
    const v: unknown = JSON.parse(text);
    if (v && typeof v === "object" && !Array.isArray(v)) doc = v as Record<string, unknown>;
  } catch {
    doc = null;
  }
  useEffect(() => {
    if (!dirty) return;
    let cancelled = false;
    const t = setTimeout(() => {
      api
        .policy(repo)
        .validate(text)
        .then((v) => {
          if (!cancelled) setValidation(v);
        })
        .catch((e: unknown) => {
          if (!cancelled) setValidation({ ok: false, errors: [errorMessage(e)] });
        });
    }, 400);
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
  }, [text, dirty, repo]);
  const rules: Rule[] = doc && Array.isArray(doc.rules) ? (doc.rules as Rule[]) : [];
  const update = (next: Rule[]) => {
    const base = doc ?? { version: 1 };
    setText(JSON.stringify({ version: 1, ...base, rules: next }, null, 2));
    setNote(null);
  };
  const save = async () => {
    setBusy(true);
    setNote(null);
    try {
      await api.policy(repo).put(JSON.parse(text) as Policy);
      setNote({ ok: true, text: "Policy saved." });
      invalidate(`policy:${repo}`);
    } catch (e) {
      setNote({ ok: false, text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Box
      title={
        <span className="admin-head">
          <strong>Push policy (D16)</strong>
          <span className="muted">{rules.length} rule(s); empty = anyone with write may move any ref</span>
          <span className="spacer" />
          <button type="button" className="btn small" onClick={() => setRaw(!raw)} aria-pressed={raw}>
            {raw ? "Form view" : "Raw JSON"}
          </button>
        </span>
      }
    >
      {dirty && validation && !validation.ok && (
        <div className="notice error" role="alert">
          {validation.errors.join(" · ")}
        </div>
      )}
      {raw || doc === null ? (
        <div className="editor">
          {doc === null && <p className="field-error">Not valid JSON: fix it here.</p>}
          <textarea className="code-input" spellCheck={false} rows={Math.min(30, Math.max(8, text.split("\n").length + 1))} value={text} aria-label={`${repo} policy.json`} onChange={(e) => setText(e.target.value)} />
        </div>
      ) : (
        <div className="form-group">
          {rules.length === 0 && <p className="muted">No rules.</p>}
          {rules.map((r, i) => (
            <fieldset key={i} className="field" style={{ border: 0 }}>
              <legend className="sr-only">Rule {i + 1}</legend>
              <div className="control">
                <input
                  type="text"
                  aria-label={`Rule ${i + 1} name`}
                  value={r.name ?? ""}
                  onChange={(e) => update(rules.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))}
                />
                <button type="button" className="btn small danger" onClick={() => update(rules.filter((_, j) => j !== i))}>
                  Remove rule
                </button>
              </div>
              <div className="control">
                <ListTextarea
                  rows={2}
                  aria-label={`Rule ${i + 1} refs (one per line)`}
                  value={r.match?.refs ?? []}
                  onChange={(refs) => update(rules.map((x, j) => (j === i ? { ...x, match: { ...x.match, refs } } : x)))}
                />
                {r.effect?.protect ? (
                  <div className="secret-row" role="group" aria-label={`Rule ${i + 1} restricts`}>
                    {RESTRICTS.map((op) => {
                      const on = (r.effect?.protect?.restricts ?? []).includes(op);
                      return (
                        <label key={op} className="small">
                          <input
                            type="checkbox"
                            checked={on}
                            onChange={() =>
                              update(
                                rules.map((x, j) => {
                                  if (j !== i) return x;
                                  const cur = x.effect?.protect?.restricts ?? [];
                                  const restricts = on ? cur.filter((o) => o !== op) : [...cur, op];
                                  return { ...x, effect: { ...x.effect, protect: { ...x.effect?.protect, restricts } } };
                                }),
                              )
                            }
                          />{" "}
                          {op}
                        </label>
                      );
                    })}
                  </div>
                ) : (
                  <p className="help">Effect: {JSON.stringify(r.effect ?? {})} (edit in the raw JSON)</p>
                )}
              </div>
            </fieldset>
          ))}
          <button
            type="button"
            className="btn small"
            onClick={() => update([...rules, { name: `protect-${rules.length + 1}`, match: { refs: ["refs/heads/main"] }, effect: { protect: { restricts: ["delete", "force-push"] } } }])}
          >
            Add protect rule
          </button>
        </div>
      )}
      <div className="save-bar">
        <button type="button" className="btn primary" disabled={!dirty || busy || !(validation?.ok ?? false)} onClick={save}>
          {busy ? "Saving…" : "Save policy"}
        </button>
        <button type="button" className="btn" disabled={!dirty || busy} onClick={() => setText(savedText)}>
          Discard
        </button>
        <span className="state" role="status" aria-live="polite">
          {!dirty ? "No unsaved changes." : validation === null ? "Checking…" : validation.ok ? "Valid." : "Invalid."}
        </span>
      </div>
      {note && (
        <div className={`notice ${note.ok ? "ok" : "error"}`} role={note.ok ? "status" : "alert"}>
          {note.text}
        </div>
      )}
    </Box>
  );
}
