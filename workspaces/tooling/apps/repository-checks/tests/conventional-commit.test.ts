import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { commitTypes, subjectError } from "../conventional-commit.ts";

test("accepts Conventional Commit subjects", () => {
  for (const subject of [
    "feat: Add the release gate",
    "fix(runtime): Keep the request identity",
    "feat(client)!: Drop the old sync path",
    "fix(providers): Keep the steer",
    "build(layr): Warm the line",
    "Merge pull request #1 from ifdotpy/x",
    'Revert "feat: Add x"',
  ]) {
    assert.equal(subjectError(subject), undefined, subject);
  }
});

test("rejects other subjects", () => {
  for (const subject of [
    "Add the release gate",
    "feature: Add x",
    "fix(unknown): Add x",
    "fix:Add x",
    "fix: ",
    "Fix: Add x",
  ]) {
    assert.match(subjectError(subject) ?? "", /Conventional Commit/, subject);
  }
});

test("release-please sections list the same types", () => {
  const config: unknown = JSON.parse(
    readFileSync(
      new URL("../../../../../release-please-config.json", import.meta.url),
      "utf8",
    ),
  );
  assert.ok(
    typeof config === "object" &&
      config !== null &&
      "changelog-sections" in config,
  );
  const sections = config["changelog-sections"];
  assert.ok(Array.isArray(sections));
  assert.deepEqual(
    sections
      .map((section: unknown) => (section as { type: string }).type)
      .sort(),
    commitTypes.filter((type) => type !== "style").sort(),
  );
});

// The manifest owns the product version. Every extra file of release-please must carry it.
function field(value: unknown, key: string): unknown {
  assert.ok(typeof value === "object" && value !== null && key in value, key);
  return Object.entries(value).find(([name]) => name === key)?.[1];
}

test("every release-please version field equals the manifest version", () => {
  const read = (path: string) =>
    readFileSync(new URL(`../../../../../${path}`, import.meta.url), "utf8");
  const version = field(JSON.parse(read(".release-please-manifest.json")), ".");
  const packageConfig = field(
    field(JSON.parse(read("release-please-config.json")), "packages"),
    ".",
  );
  const files = field(packageConfig, "extra-files");
  assert.ok(Array.isArray(files) && files.length > 0);
  for (const file of files) {
    const path = String(field(file, "path"));
    const jsonpath = String(field(file, "jsonpath"));
    const text = read(path);
    let found: unknown;
    if (field(file, "type") === "json") {
      found = field(JSON.parse(text), "version");
    } else if (path === "Cargo.toml") {
      found = /\[workspace\.package\][^[]*?\nversion = "([^"]+)"/.exec(
        text,
      )?.[1];
    } else {
      const crate = /@\.name\.value=="([^"]+)"/.exec(jsonpath)?.[1] ?? "";
      found = new RegExp(`name = "${crate}"\\nversion = "([^"]+)"`).exec(
        text,
      )?.[1];
    }
    assert.equal(found, version, `${path} ${jsonpath}`);
  }
});
