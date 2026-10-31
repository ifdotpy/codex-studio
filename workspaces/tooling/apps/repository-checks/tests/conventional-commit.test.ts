import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { commitTypes, subjectError } from "../conventional-commit.ts";

test("accepts Conventional Commit subjects", () => {
  for (const subject of [
    "feat: Add the release gate",
    "fix(runtime): Keep the request identity",
    "feat(client)!: Drop the old sync path",
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
  const config = JSON.parse(
    readFileSync(
      new URL("../../../../../release-please-config.json", import.meta.url),
      "utf8",
    ),
  ) as { "changelog-sections": { type: string }[] };
  assert.deepEqual(
    config["changelog-sections"].map((section) => section.type).sort(),
    commitTypes.filter((type) => type !== "style").sort(),
  );
});
