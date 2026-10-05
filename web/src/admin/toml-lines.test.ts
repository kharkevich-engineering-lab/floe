// Runs under `node --experimental-strip-types --test` (part of `pnpm run build`).
import { test } from "node:test";
import assert from "node:assert/strict";
import { tomlGet, tomlLiteral, tomlSet } from "./toml-lines.ts";

const doc = `# repo settings
[upstream]
git = "https://github.com/acme/widgets.git" # the remote
follow = ["refs/heads/*", "refs/tags/*"]
lfs = true

[[bundles.strategy]]
name = "weekly"

[maintenance]
follow_interval = "5m"
`;

test("reads the subset", () => {
  assert.equal(tomlGet(doc, "upstream", "git"), "https://github.com/acme/widgets.git");
  assert.deepEqual(tomlGet(doc, "upstream", "follow"), ["refs/heads/*", "refs/tags/*"]);
  assert.equal(tomlGet(doc, "upstream", "lfs"), true);
  assert.equal(tomlGet(doc, "maintenance", "follow_interval"), "5m");
  assert.equal(tomlGet(doc, "upstream", "name"), undefined);
  assert.equal(tomlGet(doc, "bundles.strategy", "name"), undefined, "array-of-tables are not form fields");
});

test("writes keep every other line", () => {
  const a = tomlSet(doc, "upstream", "lfs", false);
  assert.ok(a.includes("lfs = false"));
  assert.ok(a.includes("# repo settings") && a.includes('name = "weekly"'));
  const b = tomlSet(a, "upstream", "on_rewrite", "archive");
  assert.equal(tomlGet(b, "upstream", "on_rewrite"), "archive");
  assert.ok(b.indexOf("on_rewrite") < b.indexOf("[[bundles.strategy]]"), "inserted inside its section");
  const c = tomlSet(b, "compaction", "enabled", false);
  assert.ok(c.endsWith("[compaction]\nenabled = false\n"));
  const d = tomlSet(c, "upstream", "git", null);
  assert.equal(tomlGet(d, "upstream", "git"), undefined);
  assert.equal(tomlSet("", "upstream", "follow", ["refs/heads/main"]), '[upstream]\nfollow = ["refs/heads/main"]\n');
  assert.equal(tomlLiteral('a"b'), '"a\\"b"');
});
