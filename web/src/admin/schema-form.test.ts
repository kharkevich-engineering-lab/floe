// Runs under `node --experimental-strip-types --test` (part of `pnpm run build`); no test framework.
import { test } from "node:test";
import assert from "node:assert/strict";
import { docsDiffer, errorsFor, listText, parseList, testToken, fieldsOf, fromInput, globMatch, groupFields, secretInput, secretState, setIn, toInput, unclaimedErrors } from "./schema-form.ts";

const schema = {
  properties: {
    github_mirror: {
      type: "object",
      properties: {
        enabled: { type: "boolean", title: "Mirror GitHub", default: false, "x-floe": { format: "", group: "General", live: true } },
        token: { oneOf: [{ type: "object" }], title: "Token", default: { env: "FLOE_GITHUB_TOKEN" }, "x-floe": { format: "secret", group: "Credential", live: true } },
        include: { type: "array", items: { type: "string" }, title: "Include", default: ["*/*"], "x-floe": { format: "glob", group: "Selection", live: true } },
        max_new_per_pass: { type: "integer", minimum: 0, default: 20, "x-floe": { format: "", group: "Schedule", live: true } },
        on_rewrite: { type: "string", enum: ["archive", "refuse"], default: "archive", "x-floe": { format: "", group: "General", live: true } },
        git_url: { type: "string", default: "https://github.com", "x-floe": { format: "url", group: "Credential", live: false } },
      },
    },
    catalog: { type: "object", properties: { uri: { type: ["string", "null"], default: null } } },
  },
};

test("fields map schema types, formats and groups", () => {
  const f = fieldsOf(schema, "github_mirror");
  assert.deepEqual(
    f.map((x) => [x.key, x.kind]),
    [
      ["enabled", "boolean"],
      ["token", "secret"],
      ["include", "list"],
      ["max_new_per_pass", "integer"],
      ["on_rewrite", "enum"],
      ["git_url", "string"],
    ],
  );
  assert.equal(f[1]?.format, "secret");
  assert.equal(f[5]?.live, false);
  assert.deepEqual(f[4]?.options, ["archive", "refuse"]);
  assert.deepEqual(
    groupFields(f).map(([g, xs]) => [g, xs.length]),
    [
      ["General", 2],
      ["Credential", 2],
      ["Selection", 1],
      ["Schedule", 1],
    ],
  );
  const c = fieldsOf(schema, "catalog");
  assert.equal(c[0]?.kind, "nullable-string");
  assert.equal(c[0]?.group, "Other");
  assert.deepEqual(fieldsOf(schema, "missing"), []);
});

test("inputs round-trip values", () => {
  const [enabled, , include, max, , git] = fieldsOf(schema, "github_mirror");
  assert.ok(enabled && include && max && git);
  assert.equal(toInput(include, ["a/*", "b/c"]), "a/*\nb/c");
  assert.deepEqual(fromInput(include, "a/*\n b/c ,\n\n"), ["a/*", "b/c"]);
  assert.equal(fromInput(max, "42"), 42);
  assert.equal(fromInput(max, "4x"), "4x", "left for validation to name");
  assert.equal(fromInput(enabled, true), true);
  assert.equal(toInput(git, null), "");
  const [uri] = fieldsOf(schema, "catalog");
  assert.ok(uri);
  assert.equal(fromInput(uri, "  "), null);
});

test("secrets are never echoed", () => {
  const [, token] = fieldsOf(schema, "github_mirror");
  assert.ok(token);
  assert.equal(toInput(token, { redacted: true }), "");
  assert.deepEqual(secretState({ redacted: true }), { kind: "sealed" });
  assert.deepEqual(secretState({ env: "X" }), { kind: "env", env: "X" });
  assert.deepEqual(secretState(null), { kind: "unset" });
  assert.deepEqual(secretInput("keep", "ignored"), { redacted: true });
  assert.deepEqual(secretInput("env", " X "), { env: "X" });
  assert.deepEqual(secretInput("value", "ghp"), { value: "ghp" });
  assert.equal(secretInput("unset", ""), null);
});

test("list text keeps what is being typed", () => {
  assert.equal(listText("a/*\n", ["a/*"]), "a/*\n", "a trailing Enter survives");
  assert.equal(listText("a/*,", ["a/*"]), "a/*,", "a trailing comma survives");
  assert.equal(listText("a/*\nb", ["a/*"]), "a/*", "an outside change (reset) wins");
  assert.equal(listText("", ["x", "y"]), "x\ny");
  assert.deepEqual(parseList(" a , b\n\n c "), ["a", "b", "c"]);
});

test("tests send only a typed token", () => {
  assert.deepEqual(testToken({ value: "ghp" }), { value: "ghp" });
  assert.equal(testToken({ env: "FLOE_GITHUB_TOKEN" }), undefined);
  assert.equal(testToken({ redacted: true }), undefined);
  assert.equal(testToken(undefined), undefined);
});

test("errors attach to their fields", () => {
  const fields = fieldsOf(schema, "github_mirror");
  const errors = [
    { path: "github_mirror.include", message: "bad glob" },
    { path: "github_mirror.token", message: "set FLOE_CONFIG_KEY" },
    { message: "the effective config" },
  ];
  assert.deepEqual(errorsFor(errors, "github_mirror.include"), ["bad glob"]);
  assert.deepEqual(errorsFor(errors, "github_mirror.enabled"), []);
  assert.deepEqual(unclaimedErrors(errors, fields), ["the effective config"]);
});

test("documents and globs", () => {
  const d = setIn({ github_mirror: { enabled: false } }, "github_mirror", "enabled", true);
  assert.deepEqual(d, { github_mirror: { enabled: true } });
  assert.equal(docsDiffer({ a: 1, b: [1] }, { b: [1], a: 1 }), false);
  assert.equal(docsDiffer({ a: 1 }, { a: 2 }), true);
  assert.ok(globMatch("acme/*", "Acme/Widgets"));
  assert.ok(!globMatch("acme/*", "acme/a/b"));
  assert.ok(globMatch("*/*", "x/y"));
  assert.ok(!globMatch("acme/w.dgets", "acme/widgets"), "dots are literal");
});
