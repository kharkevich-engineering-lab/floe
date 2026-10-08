/**
 * Schema → form mapping for the admin area (D62): pure functions over the JSON
 * Schema that `GET /api/v1/admin/config/schema` serves, so the pages render
 * every key of the runtime config document without hard-coding it. No React,
 * no DOM: `schema-form.test.ts` runs it under `node --test`.
 */
import type { ConfigDocument, ConfigFieldError, JsonSchema, SecretValue } from "../../sdk/repos";

export type FieldKind = "boolean" | "integer" | "number" | "string" | "nullable-string" | "list" | "enum" | "secret";

export interface Field {
  /** `section.key` */
  path: string;
  section: string;
  key: string;
  kind: FieldKind;
  title: string;
  description: string;
  /** `duration`, `bytesize`, `url`, `glob`, `env`, `refpattern`, `secret`, or "" */
  format: string;
  group: string;
  /** Applies without a restart. */
  live: boolean;
  default: unknown;
  options: string[];
  /** Folded away by default (`x-floe.advanced`). */
  advanced: boolean;
  /** Display order within the section (`x-floe.order`; schema order when absent). */
  order: number;
  /** Shown only while each named sibling key holds one of the values (`x-floe.when`). */
  when: Record<string, unknown[]> | null;
}

function kindOf(node: JsonSchema): FieldKind {
  if (node["x-floe"]?.format === "secret" || node.oneOf) return "secret";
  if (node.enum && node.enum.length > 0) return "enum";
  const t = node.type;
  if (Array.isArray(t)) return t.includes("null") ? "nullable-string" : "string";
  switch (t) {
    case "boolean":
      return "boolean";
    case "integer":
      return "integer";
    case "number":
      return "number";
    case "array":
      return "list";
    default:
      return "string";
  }
}

/** The fields of one section, in schema order. */
export function fieldsOf(schema: JsonSchema, section: string): Field[] {
  const sec = schema.properties?.[section];
  const props = sec?.properties ?? {};
  return Object.entries(props)
    // Nested tables (a sub-object without a secret's oneOf) are not form fields.
    .filter(([, node]) => node.type !== "object" || node.oneOf !== undefined)
    .map(([key, node], i) => ({
    path: `${section}.${key}`,
    section,
    key,
    kind: kindOf(node),
    title: node.title ?? key,
    description: node.description ?? "",
    format: node["x-floe"]?.format ?? "",
    group: node["x-floe"]?.group ?? "Other",
    live: node["x-floe"]?.live ?? true,
    default: node.default,
    options: node.enum ?? [],
    advanced: node["x-floe"]?.advanced ?? false,
    order: node["x-floe"]?.order ?? 10_000 + i,
    when: node["x-floe"]?.when ?? null,
  }))
    .toSorted((a, b) => a.order - b.order);
}

/** A section's own presentation: title, plain-language summary, whether it applies live, its group order. */
export function sectionMeta(schema: JsonSchema, section: string): { title: string; description: string; live: boolean; groups: string[] } {
  const sec = schema.properties?.[section];
  return {
    title: sec?.title ?? section,
    description: sec?.description ?? "",
    live: sec?.["x-floe"]?.live ?? true,
    groups: sec?.["x-floe"]?.groups ?? [],
  };
}

/** Whether a field applies to the values being edited (its `when` conditions hold). */
export function fieldVisible(field: Field, values: Record<string, unknown>): boolean {
  if (!field.when) return true;
  return Object.entries(field.when).every(([key, allowed]) => allowed.some((v) => !docsDiffer(v, values[key])));
}

/**
 * Fields grouped for display: groups in the section's declared order (then
 * first appearance), advanced groups last, and fields hidden by their `when`
 * left out.
 */
export function displayGroups(fields: Field[], order: string[], values: Record<string, unknown>): { name: string; advanced: boolean; fields: Field[] }[] {
  const rank = (g: string) => {
    const i = order.indexOf(g);
    return i < 0 ? order.length : i;
  };
  return groupFields(fields)
    .map(([name, fs]) => ({ name, advanced: fs.every((f) => f.advanced), fields: fs.filter((f) => fieldVisible(f, values)) }))
    .filter((g) => g.fields.length > 0)
    .toSorted((a, b) => Number(a.advanced) - Number(b.advanced) || rank(a.name) - rank(b.name));
}

/** The fields whose value differs between two versions of a section. */
export function changedFields(fields: Field[], current: Record<string, unknown>, saved: Record<string, unknown>): Field[] {
  return fields.filter((f) => docsDiffer(current[f.key], saved[f.key]));
}

/** Of `paths` (e.g. the instance's pending restarts), the fields of this section they name. */
export function fieldsAt(fields: Field[], paths: string[]): Field[] {
  return fields.filter((f) => paths.some((p) => p === f.path || p === f.section || p.startsWith(`${f.path}.`)));
}

/** Fields grouped by `x-floe.group`, groups in order of first appearance. */
export function groupFields(fields: Field[]): [string, Field[]][] {
  const out = new Map<string, Field[]>();
  for (const f of fields) {
    const g = out.get(f.group);
    if (g) g.push(f);
    else out.set(f.group, [f]);
  }
  return [...out.entries()];
}

/** The value an input shows for a document value. */
export function toInput(field: Field, value: unknown): string | boolean {
  switch (field.kind) {
    case "boolean":
      return value === true;
    case "list":
      return Array.isArray(value) ? value.map(String).join("\n") : "";
    case "secret":
      return "";
    default:
      return value === null || value === undefined ? "" : String(value);
  }
}

/** The document value for what an input holds. An integer that does not parse stays a string, so validation names it. */
export function fromInput(field: Field, raw: string | boolean): unknown {
  switch (field.kind) {
    case "boolean":
      return raw === true;
    case "integer": {
      const s = String(raw).trim();
      return /^\d+$/.test(s) ? Number(s) : s;
    }
    case "number": {
      const s = String(raw).trim();
      const n = Number(s);
      return s !== "" && Number.isFinite(n) ? n : s;
    }
    case "list":
      return parseList(String(raw));
    case "nullable-string": {
      const s = String(raw).trim();
      return s === "" ? null : s;
    }
    default:
      return String(raw);
  }
}

/** Split a list textarea's text into items (newline or comma separated, trimmed, no empties). */
export function parseList(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((s) => s.trim())
    .filter((s) => s !== "");
}

/**
 * The text a list textarea shows: the user's own text while it still parses
 * to `value` — so a trailing Enter or "," survives the round trip through the
 * document — else `value`, one item per line (a reset, a reload).
 */
export function listText(text: string, value: unknown): string {
  const items = Array.isArray(value) ? value.map(String) : [];
  return docsDiffer(parseList(text), items) ? items.join("\n") : text;
}

/** A secret field's state, for display (the value itself is never shown). */
export function secretState(value: unknown): { kind: "env" | "sealed" | "value" | "unset"; env?: string } {
  if (value && typeof value === "object") {
    const v = value as Record<string, unknown>;
    if (typeof v.env === "string") return { kind: "env", env: v.env };
    if (v.redacted === true || typeof v.sealed === "string") return { kind: "sealed" };
    if (typeof v.value === "string") return { kind: "value" };
  }
  return { kind: "unset" };
}

/** A secret for the wire: an env reference, a new value, or "keep the stored one". */
export function secretInput(mode: "env" | "value" | "keep" | "unset", text: string): SecretValue | null {
  switch (mode) {
    case "env":
      return { env: text.trim() };
    case "value":
      return { value: text };
    case "keep":
      return { redacted: true };
    default:
      return null;
  }
}

/**
 * The token a credential test or dry run may send: only a freshly typed
 * `{value}`; anything else (an env reference, the stored/redacted one) means
 * "use the stored token", which the server sends only to the configured URL.
 */
export function testToken(value: unknown): SecretValue | undefined {
  const s = secretState(value);
  return s.kind === "value" ? (value as SecretValue) : undefined;
}

/** What an editor started from: the saved document's version and text. */
export interface DraftBase<V> {
  version: V;
  text: string;
  /** Right after our own save: the version it replaced, which a poll may still return (never taken). */
  replaced?: V;
}

/**
 * Whether an editor takes a newer saved document: only when it has no edits
 * of its own (its text is still the one it started from). With edits it keeps
 * them, and its conditional save is refused with a 409 instead of silently
 * reverting the other change. `null` = keep the current draft.
 */
export function nextDraft<V>(base: DraftBase<V>, saved: DraftBase<V>, text: string): DraftBase<V> | null {
  if (saved.version === base.version && saved.text === base.text) return null;
  if (base.replaced !== undefined && saved.version === base.replaced) return null;
  return text === base.text ? { version: saved.version, text: saved.text } : null;
}

/** `doc` with `section.key = value` (immutably). */
export function setIn(doc: ConfigDocument, section: string, key: string, value: unknown): ConfigDocument {
  return { ...doc, [section]: { ...doc[section], [key]: value } };
}

/** The validation errors that belong to a field (its path, or a path below it). */
export function errorsFor(errors: ConfigFieldError[], path: string): string[] {
  return errors.filter((e) => e.path === path || (e.path ?? "").startsWith(`${path}.`) || (e.path ?? "").startsWith(`${path}[`)).map((e) => e.message);
}

/** Errors that no (shown) field claims — displayed above the form so none is lost. */
export function unclaimedErrors(errors: ConfigFieldError[], fields: Field[]): string[] {
  return errors.filter((e) => !e.path || !fields.some((f) => e.path === f.path || e.path?.startsWith(`${f.path}.`))).map((e) => e.message);
}

/** Whether two documents differ (key order ignored). */
export function docsDiffer(a: unknown, b: unknown): boolean {
  return canonical(a) !== canonical(b);
}

function canonical(v: unknown): string {
  if (Array.isArray(v)) return `[${v.map(canonical).join(",")}]`;
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    return `{${Object.keys(o)
      .toSorted()
      .map((k) => `${JSON.stringify(k)}:${canonical(o[k])}`)
      .join(",")}}`;
  }
  return JSON.stringify(v ?? null);
}

/** `owner/name` glob match as the mirror does it (`*` stops at `/`, ASCII case-insensitive). */
const escapeRegex = (part: string) => part.replace(/[.+?^$|()[\]{}\\]/g, "\\$&");

export function globMatch(glob: string, s: string): boolean {
  const pattern = glob.toLowerCase().split("*").map(escapeRegex).join("[^/]*");
  return new RegExp("^" + pattern + "$").test(s.toLowerCase());
}
