import { useEffect, useId, useMemo, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { api, ApiError, type AdminConfig, type ConfigDocument, type ConfigFieldError, type JsonSchema, type ValidationResult } from "../api";
import { invalidate, useData } from "../data";
import { relTime } from "../format";
import { Icon, type IconName } from "../components/Icon";
import { KeyHint, Notice } from "../components/ui";
import { ConfigDiff } from "./ConfigDiff";
import { useUnsavedGuard } from "./useUnsavedGuard";
import { ListTextarea } from "./ListTextarea";
import {
  changedFields,
  displayGroups,
  docsDiffer,
  errorsFor,
  fieldVisible,
  fieldsAt,
  fieldsOf,
  fromInput,
  secretInput,
  secretState,
  sectionMeta,
  toInput,
  unclaimedErrors,
  type Field,
} from "./schema-form";

type SecretMode = "env" | "value" | "unset";
interface SecretDraft {
  mode: SecretMode;
  text: string;
}

/** The schema (rarely changes) and the current document (revalidated every few seconds). */
export function useAdminConfig(): { schema: JsonSchema; config: AdminConfig } {
  const schema = useData<JsonSchema>("admin:schema", () => api.admin.config.schema(), 300_000);
  const config = useData<AdminConfig>("admin:config", () => api.admin.config.get(), 5_000);
  return { schema, config };
}

export function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** `{error, errors[]}` from a 400 of the admin API. */
export function detailsErrors(details: unknown): ConfigFieldError[] {
  if (details && typeof details === "object" && Array.isArray((details as { errors?: unknown }).errors)) {
    return (details as { errors: ConfigFieldError[] }).errors;
  }
  return [];
}

function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setV(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return v;
}

/** What a stored secret looks like after a publish of `draft` (values are sealed, then redacted on read). */
function afterPublish(v: unknown): unknown {
  if (v && typeof v === "object" && "value" in (v as object)) return { redacted: true };
  return v;
}

/** Move focus to the first control of a section's form (the "Configure" action). */
export function focusForm(section: string) {
  const form = document.getElementById(`${section}-form`);
  const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  form?.scrollIntoView({ behavior: reduce ? "auto" : "smooth", block: "start" });
  form?.querySelector<HTMLElement>("input:not([type=hidden]), select, textarea, button")?.focus({ preventScroll: true });
}

/** Human labels for enumerated values (the stored value stays the key). */
const OPTION_LABELS: Record<string, Record<string, string>> = {
  "catalog.auth": { none: "None", bearer: "Bearer token or OAuth2", sigv4: "AWS Signature V4" },
  "github_mirror.on_rewrite": { archive: "Keep old commits under refs/archive/", refuse: "Refuse rewrites" },
};

const MONO_FORMATS = new Set(["url", "env", "glob", "refpattern", "duration", "bytesize"]);
const PLACEHOLDER: Record<string, string> = {
  duration: "e.g. 30s, 5m, 1h",
  bytesize: "e.g. 512 MiB, 2 GiB",
  env: "VARIABLE_NAME",
  url: "https://",
};

type Result = { kind: "ok" | "error" | "conflict"; text: string; restart?: string[] };

/**
 * One section of the runtime config document (D60–D62), laid out as:
 * a header card that leads with the section's status (`summary`, from the
 * page's status endpoint) and its primary action; one notice when saved
 * changes wait for a restart; the settings as numbered steps rendered from
 * the schema (conditional fields only when they apply, Advanced folded); and
 * a sticky save bar with the unsaved changes, a review diff, Discard and
 * Publish (CAS'd on `base_revision`). Raw JSON and history are secondary.
 * `renderAfter(path, current)` adds controls under a field.
 */
export function SectionEditor({
  section,
  icon,
  title: titleOverride,
  summary,
  renderAfter,
}: {
  section: string;
  icon: IconName;
  title?: string;
  summary?: (current: Record<string, unknown>, saved: Record<string, unknown>) => ReactNode;
  renderAfter?: (path: string, current: Record<string, unknown>) => ReactNode;
}) {
  const { schema, config } = useAdminConfig();
  const meta = useMemo(() => sectionMeta(schema, section), [schema, section]);
  const fields = useMemo(() => fieldsOf(schema, section), [schema, section]);
  const saved = useMemo(() => config.document[section] ?? {}, [config.document, section]);
  const [values, setValues] = useState<Record<string, unknown>>(saved);
  const [secrets, setSecrets] = useState<Record<string, SecretDraft>>({});
  const [base, setBase] = useState(config.revision);
  const [loaded, setLoaded] = useState(config.revision);
  const [forceReset, setForceReset] = useState(false);
  const [raw, setRaw] = useState(false);
  const [rawText, setRawText] = useState("");
  const [rawError, setRawError] = useState("");
  const [message, setMessage] = useState("");
  const [validation, setValidation] = useState<ValidationResult | null>(null);
  const [serverErrors, setServerErrors] = useState<ConfigFieldError[]>([]);
  const [saving, setSaving] = useState(false);
  const [result, setResult] = useState<Result | null>(null);
  const [review, setReview] = useState(false);

  const current = useMemo(() => {
    const out: Record<string, unknown> = { ...values };
    for (const f of fields) {
      const s = secrets[f.path];
      if (f.kind === "secret" && s) out[f.key] = secretInput(s.mode, s.text);
    }
    return out;
  }, [values, secrets, fields]);
  const dirty = docsDiffer(current, saved);

  // A newer revision arrived (another admin, a pause, a rollback): take it
  // when nothing is edited here, or right after our own save.
  if (loaded !== config.revision && (!dirty || forceReset)) {
    setLoaded(config.revision);
    setBase(config.revision);
    setValues(saved);
    setSecrets({});
    setForceReset(false);
  }

  const document: ConfigDocument = useMemo(() => ({ ...config.document, [section]: current }), [config.document, section, current]);
  const docText = JSON.stringify(document);
  const debounced = useDebounced(docText, 400);
  useEffect(() => {
    if (!dirty) return;
    let cancelled = false;
    api.admin.config
      .validate(JSON.parse(debounced) as ConfigDocument)
      .then((v) => {
        if (!cancelled) setValidation(v);
      })
      .catch((e: unknown) => {
        if (!cancelled) setValidation({ ok: false, errors: [{ message: errorMessage(e) }], diff: [], restart_required: [] });
      });
    return () => {
      cancelled = true;
    };
  }, [debounced, dirty]);
  useUnsavedGuard(dirty);

  const shown = dirty ? validation : null;
  const errors = serverErrors.length > 0 ? serverErrors : (shown?.errors ?? []);
  const stale = dirty && debounced !== docText;
  const visible = fields.filter((f) => fieldVisible(f, current));

  const setValue = (key: string, v: unknown) => {
    setValues((prev) => ({ ...prev, [key]: v }));
    setServerErrors([]);
    setResult(null);
  };
  const setSecret = (path: string, s: SecretDraft | null) => {
    setSecrets((prev) => {
      const next = { ...prev };
      if (s) next[path] = s;
      else delete next[path];
      return next;
    });
    setServerErrors([]);
    setResult(null);
  };

  const discard = () => {
    setValues(saved);
    setSecrets({});
    setValidation(null);
    setServerErrors([]);
    setResult(null);
    setRaw(false);
    setReview(false);
  };

  const reloadLatest = () => {
    discard();
    setForceReset(true);
    setLoaded(-1);
    invalidate("admin:config");
  };

  const save = async () => {
    setSaving(true);
    setResult(null);
    setServerErrors([]);
    try {
      const r = await api.admin.config.put(document, { base_revision: base, message: message.trim() || `edit ${section}` });
      setResult({ kind: "ok", text: `Saved as revision ${r.revision}.`, restart: r.restart_required });
      const next: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(current)) next[k] = afterPublish(v);
      setValues(next);
      setSecrets({});
      setMessage("");
      setValidation(null);
      setReview(false);
      setForceReset(true);
      invalidate("admin:");
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        setResult({ kind: "conflict", text: `${e.message}. Your edits are kept here; reload to start from the latest revision.` });
      } else if (e instanceof ApiError && e.status === 400) {
        setServerErrors(detailsErrors(e.details));
        setResult({ kind: "error", text: e.message });
      } else {
        setResult({ kind: "error", text: errorMessage(e) });
      }
    } finally {
      setSaving(false);
    }
  };

  const toggleRaw = () => {
    if (!raw) {
      setValues(current);
      setSecrets({});
      setRawText(JSON.stringify(current, null, 2));
      setRawError("");
    }
    setRaw(!raw);
  };

  const changed = changedFields(fields, current, saved);
  const changedNeedingRestart = changed.filter((f) => !f.live);
  const pending = fieldsAt(fields, config.applied.restart_required);
  const groups = displayGroups(fields, meta.groups, current);
  const toggles = groups.find((g) => g.name === "General" && g.fields.every((f) => f.kind === "boolean"));
  const steps = groups.filter((g) => g !== toggles && !g.advanced);
  const advanced = groups.filter((g) => g.advanced);
  const valid = !dirty || (shown?.ok ?? false);
  const allRestart = fields.length > 0 && fields.every((f) => !f.live);
  const someRestart = !allRestart && fields.some((f) => !f.live);
  const title = titleOverride ?? meta.title;

  const fieldRow = (f: Field) => (
    <FieldRow
      key={f.path}
      field={f}
      value={values[f.key]}
      secret={secrets[f.path]}
      errors={errorsFor(errors, f.path)}
      sealingKey={config.sealing_key}
      keyEnv={config.key_env}
      onChange={(v) => setValue(f.key, v)}
      onSecret={(s) => setSecret(f.path, s)}
      after={renderAfter?.(f.path, current)}
    />
  );

  return (
    <div className="section-editor">
      <section className="section-card" aria-labelledby={`${section}-title`}>
        <div className="section-card-head">
          <span className="section-icon" aria-hidden>
            <Icon name={icon} size={22} />
          </span>
          <div className="section-card-titles">
            <h2 id={`${section}-title`}>{title}</h2>
            {meta.description && <p className="section-card-description">{meta.description}</p>}
          </div>
          <div className="section-card-secondary">
            <Link to="/_admin/history" className="btn small ghost">
              <Icon name="history" /> History
            </Link>
            <button type="button" className="btn small ghost" onClick={toggleRaw} aria-pressed={raw}>
              <Icon name="code" /> {raw ? "Back to the form" : "Edit as JSON"}
            </button>
          </div>
        </div>
        {summary?.(current, saved)}
        <p className="section-card-meta">
          {allRestart && (
            <span>
              <Icon name="restart" /> Changes here apply after every floe instance restarts.
            </span>
          )}
          {someRestart && (
            <span>
              <Icon name="restart" /> Most changes apply within seconds; floe says when one needs a restart.
            </span>
          )}
          {!allRestart && !someRestart && (
            <span>
              <Icon name="check" /> Changes apply within seconds, without a restart.
            </span>
          )}
          <span className="muted">
            {config.revision === 0
              ? "Built-in defaults — nothing saved yet."
              : `Revision ${config.revision}${config.updated_at ? `, saved ${relTime(config.updated_at)}` : ""}${config.author ? ` by ${config.author}` : ""}`}
          </span>
        </p>
      </section>

      {pending.length > 0 && (
        <Notice tone="warn" title="Saved changes are waiting for a restart">
          This instance still runs the previous {pending.length === 1 ? "value" : "values"} of {listTitles(pending)}. Restart every floe instance to apply{" "}
          {pending.length === 1 ? "it" : "them"}.
        </Notice>
      )}

      {unclaimedErrors(errors, visible).map((m) => (
        <Notice key={m} tone="danger" title="This configuration cannot be saved">
          {m}
        </Notice>
      ))}

      <form
        id={`${section}-form`}
        className="section-form"
        aria-label={`${title} settings`}
        onSubmit={(e) => {
          e.preventDefault();
          if (dirty && !saving && valid && !stale && rawError === "") void save();
        }}
      >
        {raw ? (
          <div className="raw-editor">
            <label htmlFor={`${section}-raw`}>{title} as JSON</label>
            <textarea
              id={`${section}-raw`}
              className="code-input"
              spellCheck={false}
              rows={Math.min(40, Math.max(12, rawText.split("\n").length + 1))}
              value={rawText}
              aria-invalid={rawError !== ""}
              aria-describedby={`${section}-raw-help`}
              onChange={(e) => {
                setRawText(e.target.value);
                try {
                  const v: unknown = JSON.parse(e.target.value);
                  if (!v || typeof v !== "object" || Array.isArray(v)) throw new Error("a JSON object is expected");
                  setValues(v as Record<string, unknown>);
                  setRawError("");
                } catch (err) {
                  setRawError(errorMessage(err));
                }
              }}
            />
            <p id={`${section}-raw-help`} className="help" aria-live="polite">
              {rawError ? (
                <span className="field-error">{rawError}</span>
              ) : (
                <>
                  Secrets: <code>{'{"env": "NAME"}'}</code>, <code>{'{"value": "…"}'}</code> (encrypted on save) or <code>{'{"redacted": true}'}</code> (keep the stored one).
                </>
              )}
            </p>
          </div>
        ) : (
          <>
            {toggles && <div className="toggle-panel">{toggles.fields.map(fieldRow)}</div>}
            <ol className="steps">
              {steps.map((g, i) => (
                <li key={g.name} className="step">
                  <fieldset>
                    <legend className="step-legend">
                      <span className="step-number" aria-hidden>
                        {i + 1}
                      </span>
                      {g.name}
                    </legend>
                    <div className="step-fields">{g.fields.map(fieldRow)}</div>
                  </fieldset>
                </li>
              ))}
            </ol>
            {advanced.map((g) => (
              <details key={g.name} className="advanced" open={g.fields.some((f) => errorsFor(errors, f.path).length > 0) || undefined}>
                <summary>
                  <Icon name="chevron" className="icon chevron" />
                  <span>
                    <strong>{g.name}</strong>
                    <span className="muted"> — {g.fields.length} settings; the defaults suit most installations</span>
                  </span>
                </summary>
                <fieldset>
                  <legend className="sr-only">{g.name}</legend>
                  <div className="step-fields">{g.fields.map(fieldRow)}</div>
                </fieldset>
              </details>
            ))}
          </>
        )}

        {result && (
          <Notice
            tone={result.kind === "ok" ? "ok" : result.kind === "conflict" ? "warn" : "danger"}
            title={result.kind === "ok" ? result.text : result.kind === "conflict" ? "Someone else saved first" : "Not saved"}
            action={
              result.kind === "conflict" ? (
                <button type="button" className="btn small" onClick={reloadLatest}>
                  Reload latest
                </button>
              ) : undefined
            }
          >
            {result.kind !== "ok" && result.text}
            {result.kind === "ok" && result.restart && result.restart.length > 0 && <>Restart every floe instance to apply: {result.restart.join(", ")}.</>}
          </Notice>
        )}

        {dirty && (
          <div className="save-bar" role="region" aria-label="Unsaved changes">
            {review && shown?.ok && (
              <div className="save-review">
                <p className="eyebrow">Changes against revision {config.revision}</p>
                <ConfigDiff diff={shown.diff} />
              </div>
            )}
            {changedNeedingRestart.length > 0 && (
              <p className="save-restart">
                <Icon name="restart" /> Saving {listTitles(changedNeedingRestart)} needs a restart of every floe instance to take effect.
              </p>
            )}
            <div className="save-row">
              <span className="save-state" role="status" aria-live="polite">
                <span className="save-count">
                  {changed.length} unsaved {changed.length === 1 ? "change" : "changes"}
                </span>
                {(stale || shown === null) && <span className="muted"> · checking…</span>}
                {!stale && shown?.ok && <span className="ok-text"> · valid</span>}
                {!stale && shown && !shown.ok && (
                  <span className="field-error">
                    {" "}
                    · {shown.errors.length} {shown.errors.length === 1 ? "problem" : "problems"} to fix
                  </span>
                )}
              </span>
              {shown?.ok && (
                <button type="button" className="btn small ghost" aria-expanded={review} onClick={() => setReview(!review)}>
                  {review ? "Hide changes" : "Review changes"}
                </button>
              )}
              <input
                type="text"
                className="text save-message"
                placeholder="Why? (kept in the history)"
                aria-label="Reason for this change"
                value={message}
                onChange={(e) => setMessage(e.target.value)}
              />
              <button type="button" className="btn" disabled={saving} onClick={discard}>
                Discard
              </button>
              <button type="submit" className="btn primary" disabled={saving || !valid || stale || rawError !== ""}>
                {saving ? "Saving…" : "Save changes"}
              </button>
            </div>
          </div>
        )}
      </form>
    </div>
  );
}

function listTitles(fields: Field[]): string {
  const t = fields.map((f) => f.title);
  if (t.length <= 1) return t.join("");
  return `${t.slice(0, -1).join(", ")} and ${t[t.length - 1]}`;
}

function FieldRow({
  field,
  value,
  secret,
  errors,
  sealingKey,
  keyEnv,
  onChange,
  onSecret,
  after,
}: {
  field: Field;
  value: unknown;
  secret: SecretDraft | undefined;
  errors: string[];
  sealingKey: boolean;
  keyEnv: string;
  onChange: (v: unknown) => void;
  onSecret: (s: SecretDraft | null) => void;
  after?: ReactNode;
}) {
  const id = useId();
  const helpId = `${id}-help`;
  const errId = `${id}-err`;
  const invalid = errors.length > 0;
  const describedBy = [field.description ? helpId : "", invalid ? errId : ""].filter(Boolean).join(" ") || undefined;
  const input = toInput(field, value);
  const mono = MONO_FORMATS.has(field.format) ? " mono" : "";
  const help = field.description && (
    <p id={helpId} className="help">
      {field.description}
    </p>
  );
  const error = invalid && (
    <p id={errId} className="field-error" role="alert">
      <Icon name="alert" /> {errors.join(" · ")}
    </p>
  );

  if (field.kind === "boolean") {
    return (
      <div className="field field-switch">
        <div className="switch-row">
          <input
            id={id}
            type="checkbox"
            role="switch"
            className="switch"
            checked={input === true}
            aria-invalid={invalid}
            aria-describedby={describedBy}
            onChange={(e) => onChange(fromInput(field, e.target.checked))}
          />
          <label htmlFor={id}>{field.title}</label>
          <KeyHint path={field.path} />
        </div>
        {help}
        {error}
        {after}
      </div>
    );
  }

  if (field.kind === "enum" && field.options.length <= 4) {
    const labels = OPTION_LABELS[field.path] ?? {};
    return (
      <fieldset className="field" aria-describedby={describedBy}>
        <div className="field-label-row">
          <legend className="field-label">{field.title}</legend>
          <KeyHint path={field.path} />
        </div>
        {help}
        <div className="choice-group">
          {field.options.map((o) => (
            <label key={o} className={`choice${input === o ? " selected" : ""}`}>
              <input type="radio" name={id} value={o} checked={input === o} aria-invalid={invalid} onChange={() => onChange(fromInput(field, o))} />
              <span>{labels[o] ?? o}</span>
            </label>
          ))}
        </div>
        {error}
        {after}
      </fieldset>
    );
  }

  let control: ReactNode;
  switch (field.kind) {
    case "enum":
      control = (
        <select id={id} value={String(input)} aria-invalid={invalid} aria-describedby={describedBy} onChange={(e) => onChange(fromInput(field, e.target.value))}>
          {field.options.map((o) => (
            <option key={o} value={o}>
              {OPTION_LABELS[field.path]?.[o] ?? o}
            </option>
          ))}
        </select>
      );
      break;
    case "list":
      control = (
        <ListTextarea
          id={id}
          className="mono"
          rows={Math.max(2, String(input).split("\n").length + 1)}
          value={value}
          placeholder="One per line"
          aria-invalid={invalid}
          aria-describedby={describedBy}
          onChange={onChange}
        />
      );
      break;
    case "secret":
      control = <SecretControl id={id} field={field} value={value} draft={secret} invalid={invalid} describedBy={describedBy} sealingKey={sealingKey} keyEnv={keyEnv} onSecret={onSecret} />;
      break;
    default:
      control = (
        <input
          id={id}
          type="text"
          className={`text${mono}`}
          inputMode={field.kind === "integer" ? "numeric" : undefined}
          spellCheck={mono ? false : undefined}
          value={String(input)}
          placeholder={field.kind === "nullable-string" ? (PLACEHOLDER[field.format] ? `Not set — ${PLACEHOLDER[field.format]}` : "Not set") : PLACEHOLDER[field.format]}
          aria-invalid={invalid}
          aria-describedby={describedBy}
          onChange={(e) => onChange(fromInput(field, e.target.value))}
        />
      );
  }
  return (
    <div className="field">
      <div className="field-label-row">
        {field.kind === "secret" ? (
          <span className="field-label" id={`${id}-label`}>
            {field.title}
          </span>
        ) : (
          <label className="field-label" htmlFor={id}>
            {field.title}
          </label>
        )}
        <KeyHint path={field.path} />
      </div>
      {help}
      <div className="control">{control}</div>
      {error}
      {after}
    </div>
  );
}

/**
 * A write-only secret: its state (set and encrypted, read from an environment
 * variable, or not set) and explicit actions — enter a new value, point at an
 * environment variable, remove. The stored value is never shown.
 */
function SecretControl({
  id,
  field,
  value,
  draft,
  invalid,
  describedBy,
  sealingKey,
  keyEnv,
  onSecret,
}: {
  id: string;
  field: Field;
  value: unknown;
  draft: SecretDraft | undefined;
  invalid: boolean;
  describedBy?: string;
  sealingKey: boolean;
  keyEnv: string;
  onSecret: (s: SecretDraft | null) => void;
}) {
  const state = secretState(value);
  const nullable = field.default === null;
  const labelledBy = `${id}-label`;
  return (
    <div className="secret" role="group" aria-labelledby={labelledBy} aria-describedby={describedBy}>
      <div className="secret-state">
        {state.kind === "sealed" && (
          <span className="status-badge tone-ok">
            <span className="status-dot" aria-hidden />
            Set — encrypted, never shown again
          </span>
        )}
        {state.kind === "env" && (
          <span className="status-badge tone-info">
            <span className="status-dot" aria-hidden />
            Set — read from <code>${state.env}</code> on every host
          </span>
        )}
        {state.kind === "unset" && (
          <span className="status-badge tone-neutral">
            <span className="status-dot" aria-hidden />
            Not set
          </span>
        )}
        {state.kind === "value" && (
          <span className="status-badge tone-info">
            <span className="status-dot" aria-hidden />
            New value — encrypted when you save
          </span>
        )}
      </div>
      {!draft && (
        <div className="secret-actions">
          <button type="button" id={id} className="btn small" disabled={!sealingKey} onClick={() => onSecret({ mode: "value", text: "" })}>
            {state.kind === "unset" ? "Enter a value" : "Replace the value"}
          </button>
          <button type="button" className="btn small" onClick={() => onSecret({ mode: "env", text: state.kind === "env" ? (state.env ?? "") : "" })}>
            {state.kind === "env" ? "Change the variable" : "Use an environment variable"}
          </button>
          {nullable && state.kind !== "unset" && (
            <button type="button" className="btn small danger" onClick={() => onSecret({ mode: "unset", text: "" })}>
              Remove
            </button>
          )}
          {!sealingKey && (
            <p className="help">
              Typed values need the encryption key <code>{keyEnv}</code> on this host; until then, use an environment variable.
            </p>
          )}
        </div>
      )}
      {draft && (
        <div className="secret-edit">
          {draft.mode === "value" && (
            <input
              id={id}
              type="password"
              className="text mono"
              autoComplete="new-password"
              aria-label={`${field.title}: new value (write-only)`}
              aria-invalid={invalid}
              placeholder="Paste the secret"
              value={draft.text}
              // oxlint-disable-next-line jsx-a11y/no-autofocus -- the user just asked to type a value
              autoFocus
              onChange={(e) => onSecret({ mode: "value", text: e.target.value })}
            />
          )}
          {draft.mode === "env" && (
            <input
              id={id}
              type="text"
              className="text mono"
              spellCheck={false}
              aria-label={`${field.title}: environment variable name`}
              aria-invalid={invalid}
              placeholder="VARIABLE_NAME"
              value={draft.text}
              // oxlint-disable-next-line jsx-a11y/no-autofocus -- the user just asked to name a variable
              autoFocus
              onChange={(e) => onSecret({ mode: "env", text: e.target.value })}
            />
          )}
          {draft.mode === "unset" && <span className="muted small">Will be removed when you save.</span>}
          <button type="button" className="btn small ghost" onClick={() => onSecret(null)}>
            Cancel
          </button>
        </div>
      )}
    </div>
  );
}

/** A label/value list for a section card's summary. */
export function Facts({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <dl className="facts">
      {rows.map(([k, v]) => (
        <div key={k}>
          <dt>{k}</dt>
          <dd>{v}</dd>
        </div>
      ))}
    </dl>
  );
}
