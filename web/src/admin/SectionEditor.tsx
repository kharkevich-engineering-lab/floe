import { useEffect, useId, useMemo, useState, type ReactNode } from "react";
import { api, ApiError, type AdminConfig, type ConfigDocument, type ConfigFieldError, type JsonSchema, type ValidationResult } from "../api";
import { invalidate, useData } from "../data";
import { Box } from "../components/Layout";
import { ConfigDiff } from "./ConfigDiff";
import { useUnsavedGuard } from "./useUnsavedGuard";
import { docsDiffer, errorsFor, fieldsOf, fromInput, groupFields, secretInput, secretState, toInput, unclaimedErrors, type Field } from "./schema-form";

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

type Result = { kind: "ok" | "error" | "conflict"; text: string; restart?: string[] };

/**
 * One section of the runtime config document as a form rendered from the
 * schema, with a raw JSON view, validation as you type (`…/config/validate`),
 * inline errors, a diff preview, a CAS'd save (`base_revision`) and an
 * unsaved-changes guard. `renderAfter(path, section)` adds controls under a
 * field (the mirror's credential test and live preview); `header(section)`
 * renders above the form.
 */
export function SectionEditor({
  section,
  title,
  renderAfter,
  header,
}: {
  section: string;
  title: string;
  renderAfter?: (path: string, current: Record<string, unknown>) => ReactNode;
  header?: (current: Record<string, unknown>) => ReactNode;
}) {
  const { schema, config } = useAdminConfig();
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

  const setValue = (key: string, v: unknown) => {
    setValues((prev) => ({ ...prev, [key]: v }));
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
      setResult({ kind: "ok", text: `Published revision ${r.revision}.`, restart: r.restart_required });
      const next: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(current)) next[k] = afterPublish(v);
      setValues(next);
      setSecrets({});
      setMessage("");
      setValidation(null);
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

  const groups = groupFields(fields);
  const valid = !dirty || (shown?.ok ?? false);
  return (
    <>
      {header?.(current)}
      <Box
        title={
          <span className="admin-head">
            <strong>{title}</strong>
            <span className="muted">
              revision {config.revision}
              {config.author ? ` by ${config.author}` : ""}
            </span>
            <span className="spacer" />
            <button type="button" className="btn small" onClick={toggleRaw} aria-pressed={raw}>
              {raw ? "Form view" : "Raw JSON"}
            </button>
          </span>
        }
      >
        {unclaimedErrors(errors, fields).map((m) => (
          <div key={m} className="notice error" role="alert">
            {m}
          </div>
        ))}
        {raw ? (
          <div className="editor">
            <textarea
              className="code-input"
              spellCheck={false}
              rows={Math.min(40, Math.max(12, rawText.split("\n").length + 1))}
              value={rawText}
              aria-label={`${section} as JSON`}
              aria-invalid={rawError !== ""}
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
            <div className="editor-status" aria-live="polite">
              {rawError ? <span className="field-error">{rawError}</span> : <span className="muted">Secrets: {"{\"env\": \"NAME\"}"}, {"{\"value\": \"…\"}"} (sealed on save) or {"{\"redacted\": true}"} (keep).</span>}
            </div>
          </div>
        ) : (
          groups.map(([group, fs]) => (
            <fieldset key={group} className="form-group" style={{ border: 0, margin: 0 }}>
              <legend>
                <h3>{group}</h3>
              </legend>
              {fs.map((f) => (
                <FieldRow
                  key={f.path}
                  field={f}
                  value={values[f.key]}
                  secret={secrets[f.path]}
                  errors={errorsFor(errors, f.path)}
                  sealingKey={config.sealing_key}
                  keyEnv={config.key_env}
                  onChange={(v) => setValue(f.key, v)}
                  onSecret={(s) => {
                    setSecrets((prev) => {
                      const next = { ...prev };
                      if (s) next[f.path] = s;
                      else delete next[f.path];
                      return next;
                    });
                    setServerErrors([]);
                    setResult(null);
                  }}
                  after={renderAfter?.(f.path, current)}
                />
              ))}
            </fieldset>
          ))
        )}
        {dirty && shown?.ok && (
          <>
            <div className="box-header">Changes against revision {config.revision}</div>
            <ConfigDiff diff={shown.diff} />
            {shown.restart_required.length > 0 && (
              <div className="notice warn">Needs a restart of every instance to take effect: {shown.restart_required.join(", ")}</div>
            )}
          </>
        )}
        <div className="save-bar">
          <input
            type="text"
            className="text"
            placeholder="Why (recorded in the history)"
            aria-label="Change message"
            value={message}
            onChange={(e) => setMessage(e.target.value)}
            disabled={!dirty}
          />
          <button type="button" className="btn primary" disabled={!dirty || saving || !valid || stale || rawError !== ""} onClick={save}>
            {saving ? "Publishing…" : "Publish"}
          </button>
          <button type="button" className="btn" disabled={!dirty || saving} onClick={discard}>
            Discard
          </button>
          <span className="state" role="status" aria-live="polite">
            {!dirty && !result && "No unsaved changes."}
            {dirty && (stale || shown === null) && "Checking…"}
            {dirty && !stale && shown?.ok && "Valid."}
            {dirty && !stale && shown && !shown.ok && `${shown.errors.length} problem(s).`}
          </span>
        </div>
        {result && (
          <div className={`notice ${result.kind === "ok" ? "ok" : result.kind === "conflict" ? "warn" : "error"}`} role={result.kind === "ok" ? "status" : "alert"}>
            {result.text}
            {result.restart && result.restart.length > 0 && <> Restart required for: {result.restart.join(", ")}.</>}
            {result.kind === "conflict" && (
              <>
                {" "}
                <button type="button" className="btn small" onClick={reloadLatest}>
                  Reload latest
                </button>
              </>
            )}
          </div>
        )}
      </Box>
    </>
  );
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
  let control: ReactNode;
  switch (field.kind) {
    case "boolean":
      control = (
        <input id={id} type="checkbox" checked={input === true} aria-invalid={invalid} aria-describedby={describedBy} onChange={(e) => onChange(fromInput(field, e.target.checked))} />
      );
      break;
    case "enum":
      control = (
        <select id={id} value={String(input)} aria-invalid={invalid} aria-describedby={describedBy} onChange={(e) => onChange(fromInput(field, e.target.value))}>
          {field.options.map((o) => (
            <option key={o} value={o}>
              {o}
            </option>
          ))}
        </select>
      );
      break;
    case "list":
      control = (
        <textarea
          id={id}
          rows={Math.max(2, String(input).split("\n").length + 1)}
          value={String(input)}
          placeholder="one per line"
          aria-invalid={invalid}
          aria-describedby={describedBy}
          onChange={(e) => onChange(fromInput(field, e.target.value))}
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
          inputMode={field.kind === "integer" ? "numeric" : undefined}
          value={String(input)}
          placeholder={field.kind === "nullable-string" ? "(unset)" : field.format === "duration" ? "e.g. 5m, 1h, 30s" : undefined}
          aria-invalid={invalid}
          aria-describedby={describedBy}
          onChange={(e) => onChange(fromInput(field, e.target.value))}
        />
      );
  }
  return (
    <div className="field">
      <label htmlFor={id}>
        {field.title} {!field.live && <span className="pill restart" title="Applies after a restart of every instance">restart</span>}
      </label>
      <div className="control">
        {control}
        {field.description && (
          <p id={helpId} className="help">
            {field.description} <code className="muted">{field.path}</code>
          </p>
        )}
        {invalid && (
          <p id={errId} className="field-error" role="alert">
            {errors.join(" · ")}
          </p>
        )}
        {after}
      </div>
    </div>
  );
}

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
  const mode = draft?.mode ?? "keep";
  return (
    <>
      <span className="muted small">
        {state.kind === "sealed" && "Stored, sealed in the config store — never shown again."}
        {state.kind === "env" && (
          <>
            Read from <code>${state.env}</code> on every instance.
          </>
        )}
        {state.kind === "unset" && "Not set."}
        {state.kind === "value" && "A new value (sealed on publish)."}
      </span>
      <div className="secret-row">
        <select
          id={id}
          aria-label={`${field.title}: what to store`}
          value={mode}
          aria-invalid={invalid}
          aria-describedby={describedBy}
          onChange={(e) => {
            const m = e.target.value;
            if (m === "keep") onSecret(null);
            else onSecret({ mode: m as SecretMode, text: m === "env" && state.kind === "env" ? (state.env ?? "") : "" });
          }}
        >
          <option value="keep">Keep as is</option>
          <option value="env">Env reference</option>
          <option value="value" disabled={!sealingKey}>
            New value{sealingKey ? "" : ` (needs ${keyEnv})`}
          </option>
          {nullable && <option value="unset">Remove</option>}
        </select>
        {draft?.mode === "env" && (
          <input type="text" aria-label={`${field.title}: env var name`} placeholder="VARIABLE_NAME" value={draft.text} onChange={(e) => onSecret({ mode: "env", text: e.target.value })} />
        )}
        {draft?.mode === "value" && (
          <input
            type="password"
            autoComplete="new-password"
            aria-label={`${field.title}: new value (write-only)`}
            placeholder="paste the secret"
            value={draft.text}
            onChange={(e) => onSecret({ mode: "value", text: e.target.value })}
          />
        )}
      </div>
    </>
  );
}
