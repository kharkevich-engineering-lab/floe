/**
 * Line-level editing of a D24 settings document (TOML), for the per-repository
 * form: read and replace `key = value` lines under a `[section]` header while
 * every other line (comments, keys the form does not manage, array-of-table
 * strategies) is kept verbatim. Values are the TOML subset the form writes:
 * basic strings, booleans, integers and arrays of basic strings — all of which
 * are also JSON, so reading uses `JSON.parse`. Pure; tested by `toml-lines.test.ts`.
 */

export type TomlValue = string | number | boolean | string[];

interface Line {
  text: string;
  /** The `[section]` this line sits under ("" before the first header). */
  section: string;
  key?: string;
}

const HEADER = /^\s*\[\s*([A-Za-z0-9_.-]+)\s*\]\s*(#.*)?$/;
const KEY = /^\s*([A-Za-z0-9_-]+)\s*=/;

function lines(text: string): Line[] {
  let section = "";
  return text.split("\n").map((t) => {
    const h = HEADER.exec(t);
    if (h?.[1]) {
      section = h[1];
      return { text: t, section };
    }
    if (/^\s*\[\[/.test(t)) {
      section = "\u0000array";
      return { text: t, section };
    }
    const k = KEY.exec(t);
    return { text: t, section, key: k?.[1] };
  });
}

/** Strip a trailing `# comment` that is outside a string. */
function valuePart(line: string): string {
  const eq = line.indexOf("=");
  const rest = line.slice(eq + 1);
  let inString = false;
  for (let i = 0; i < rest.length; i++) {
    const c = rest[i];
    if (c === '"' && rest[i - 1] !== "\\") inString = !inString;
    if (c === "#" && !inString) return rest.slice(0, i).trim();
  }
  return rest.trim();
}

/** The value of `[section] key`, or `undefined` when absent or not in the subset. */
export function tomlGet(text: string, section: string, key: string): TomlValue | undefined {
  const l = lines(text).find((x) => x.section === section && x.key === key);
  if (!l) return undefined;
  const raw = valuePart(l.text);
  if (raw === "true" || raw === "false") return raw === "true";
  if (/^-?\d+$/.test(raw)) return Number(raw);
  try {
    const v: unknown = JSON.parse(raw.replace(/'([^']*)'/g, (_m, s: string) => JSON.stringify(s)));
    if (typeof v === "string") return v;
    if (Array.isArray(v) && v.every((x) => typeof x === "string")) return v as string[];
  } catch {
    return undefined;
  }
  return undefined;
}

/** A value in TOML syntax. */
export function tomlLiteral(v: TomlValue): string {
  if (Array.isArray(v)) return `[${v.map((s) => JSON.stringify(s)).join(", ")}]`;
  if (typeof v === "string") return JSON.stringify(v);
  return String(v);
}

/** `text` with `[section] key = value` set (`null` removes the line); the section is appended when missing. */
export function tomlSet(text: string, section: string, key: string, value: TomlValue | null): string {
  const ls = lines(text);
  const idx = ls.findIndex((x) => x.section === section && x.key === key);
  const out = ls.map((l) => l.text);
  if (idx >= 0) {
    if (value === null) out.splice(idx, 1);
    else out[idx] = `${key} = ${tomlLiteral(value)}`;
    return out.join("\n");
  }
  if (value === null) return text;
  const line = `${key} = ${tomlLiteral(value)}`;
  const header = ls.findIndex((l) => l.section === section && HEADER.test(l.text));
  if (header >= 0) {
    let end = header + 1;
    while (end < ls.length && ls[end]?.section === section) end++;
    while (end > header + 1 && (out[end - 1] ?? "").trim() === "") end--;
    out.splice(end, 0, line);
    return out.join("\n");
  }
  const base = text.replace(/\s*$/, "");
  return `${base}${base ? "\n\n" : ""}[${section}]\n${line}\n`;
}
