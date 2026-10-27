import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { checkRatchet, fingerprintMatches, readBaseline } from "../ratchet.ts";
import { run } from "../ast-grep.ts";

const rules = "workspaces/tooling/apps/repository-checks/ast-grep/rules";
const fixtures: readonly (readonly [string, string, string, string])[] = [
  [
    "rust-json-value-roundtrip",
    "rs",
    'let x = serde_json::from_value(json!({"a": 1}));',
    "let x = serde_json::from_value(value);",
  ],
  [
    "rust-json-string-roundtrip",
    "rs",
    "let x = serde_json::from_str(&serde_json::to_string(value)?);",
    "let x = serde_json::from_str(&source);",
  ],
  [
    "rust-json-dispatch",
    "rs",
    'let x = v["status"].as_str() == Some("ok");',
    'let x = v["name"].as_str() == Some("ok");',
  ],
  [
    "rust-serde-default-field",
    "rs",
    "struct S { #[serde(default)] id: String }",
    "struct S { #[serde(default)] name: String }",
  ],
  [
    "ts-json-parse-cast",
    "ts",
    "const x = JSON.parse(raw) as Thing;",
    "const x: unknown = JSON.parse(raw);",
  ],
  [
    "ts-double-cast",
    "ts",
    "const x = value as unknown as Thing;",
    "const x = value as Thing;",
  ],
  ["ts-as-any", "ts", "const x = value as any;", "const x = value as unknown;"],
];
const cases = fixtures.flatMap((fixture) =>
  fixture[1] === "ts"
    ? [fixture, [`${fixture[0]}-tsx`, "tsx", fixture[2], fixture[3]] as const]
    : [fixture],
);
for (const [rule, ext, invalid, valid] of cases) {
  test(`${rule} catches its violation and accepts a nearby valid form`, () => {
    const dir = mkdtempSync(path.join(os.tmpdir(), "studio-ast-test-"));
    try {
      const file = path.join(dir, `fixture.${ext}`);
      const match = (source: string) => {
        writeFileSync(file, source);
        const output: unknown = JSON.parse(
          execFileSync(
            "node_modules/.bin/ast-grep",
            ["scan", "--rule", `${rules}/${rule}.yml`, "--json=compact", file],
            { encoding: "utf8" },
          ),
        );
        assert.ok(Array.isArray(output));
        return output.length;
      };
      assert.equal(match(invalid), 1);
      assert.equal(match(valid), 0);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
}
test("JSON dispatch also catches matches macro", () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), "studio-ast-test-"));
  try {
    const file = path.join(dir, "fixture.rs");
    writeFileSync(
      file,
      'fn f() { let x = matches!(v["kind"].as_str(), Some("ok")); }',
    );
    const output: unknown = JSON.parse(
      execFileSync(
        "node_modules/.bin/ast-grep",
        [
          "scan",
          "--rule",
          `${rules}/rust-json-dispatch.yml`,
          "--json=compact",
          file,
        ],
        { encoding: "utf8" },
      ),
    );
    assert.ok(Array.isArray(output));
    assert.equal(output.length, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
test("ratchet rejects new and removed matches until baseline shrinks", () => {
  const [one, two] = fingerprintMatches([
    { file: "a.ts", text: "x  as any", message: "fix" },
    { file: "a.ts", text: "x as any", message: "fix" },
  ]);
  assert.ok(one && two);
  assert.notEqual(one.fingerprint, two.fingerprint);
  assert.equal(checkRatchet("rule", [one, two], { rule: [one] }).length, 1);
  assert.equal(checkRatchet("rule", [], { rule: [one] }).length, 1);
  assert.deepEqual(checkRatchet("rule", [], { rule: [] }), []);
});
test("baseline decoder rejects malformed entries", () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), "studio-ratchet-test-"));
  try {
    const file = path.join(dir, "baseline.json");
    writeFileSync(
      file,
      JSON.stringify({ rule: [{ file: "a.ts", fingerprint: 42 }] }),
    );
    assert.throws(() => readBaseline(file), /Malformed ratchet baseline/);
    writeFileSync(
      file,
      JSON.stringify({ rule: [{ file: "a.ts", fingerprint: "abc:0" }] }),
    );
    assert.deepEqual(readBaseline(file), {
      rule: [{ file: "a.ts", fingerprint: "abc:0" }],
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
test("repository ast-grep baseline is current", () => assert.equal(run(), 0));
