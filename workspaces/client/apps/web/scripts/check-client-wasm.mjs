import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import init, { parse_markdown } from "../src/wasm/generated/client.js";
const binary = readFileSync(
  new URL("../src/wasm/generated/client_bg.wasm", import.meta.url),
);
await init({ module_or_path: binary });
const cases = [
  [
    "# Title\n\n🦀 Привет. Next **bold** [link][ref].\n\n[ref]: https://example.test\n\n|A|B|\n|-|-|\n|1|2|",
    false,
  ],
  ["A [file](src/test.ts:12) and ![image](./photo.png).", false],
  ["```html\n<b>unfinished</b>", true],
  ["```html\n<b>first</b>\n```\n\n```html\n<b>second</b>\n```", false],
  ["<style>body{color:red}</style>\n\n<div>preview</div>", false],
  ['Hello <a href="javascript:alert(1)" onclick="x()">unsafe</a>.', true],
  ["```rust\nfn main() {}\n```\n\n~~deleted~~ and `code`", false],
];
const native = spawnSync(
  "cargo",
  [
    "run",
    "--quiet",
    "-p",
    "studio-client-domain",
    "--example",
    "markdown-parity",
    "--locked",
  ],
  {
    cwd: fileURLToPath(new URL("../../../../../", import.meta.url)),
    input: JSON.stringify(cases),
    encoding: "utf8",
  },
);
assert.equal(native.status, 0, native.stderr);
const actual = cases.map(([text, streaming]) =>
  JSON.parse(parse_markdown(text, streaming)),
);
assert.deepEqual(actual, JSON.parse(native.stdout));
assert.equal(actual[2][0].kind, undefined);
assert.deepEqual(
  actual[3].map((block) => block.kind),
  ["html", "html"],
);
assert.ok(!JSON.stringify(actual[5]).includes("javascript:"));
assert.ok(!JSON.stringify(actual[5]).includes("onclick"));
assert.throws(() => parse_markdown("x".repeat(8 * 1024 * 1024 + 1), false));
console.log(
  "Compiled WASM: native parity, Unicode, file links, rich fences, unsafe attributes and input limits passed.",
);
