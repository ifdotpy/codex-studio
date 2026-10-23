import { test } from "node:test";
import assert from "node:assert/strict";
import {
  readFileSync,
  writeFileSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { scanSource, checkConsumers } from "./check-layout-contract.mjs";
import {
  readLayoutContract,
  validateContract,
  generateLayoutContract,
} from "./generate-layout-contract.mjs";
const contract = readLayoutContract();
const check = (path, source) =>
  checkConsumers(contract, new Map([[path, source]]), {
    requireAllLegacy: false,
  });
test("rejects a changed boundary with the owner and consumer", () => {
  const errors = check(
    "src/fixture.css",
    "/* layout-query: navigation.inlineMin */\n@media (width < 762px) {}",
  );
  assert.match(errors.join("\n"), /navigation.inlineMin.*src\/fixture.css/);
});
test("rejects a changed global layer with the owner and consumer", () => {
  const errors = check(
    "src/fixture.css",
    "/* layout-layer: layers.popover */\n.fixture { z-index: 301; }",
  );
  assert.match(errors.join("\n"), /layers.popover.*src\/fixture.css/);
});
test("rejects an added query consumer", () => {
  assert.match(
    check("src/new.css", "@media (max-width: 760px) {}").join("\n"),
    /unregistered.*src\/new.css/,
  );
});
test("rejects invalid units and duplicate generated owners", () => {
  const bad = structuredClone(contract);
  bad.tokens["navigation.inlineMin"].unit = "em";
  assert.throws(() => validateContract(bad), /navigation.inlineMin/);
  const duplicate = structuredClone(contract);
  duplicate.tokens["workspace-panel.min"] =
    duplicate.tokens["workspacePanel.min"];
  assert.throws(() => validateContract(duplicate), /duplicate owner/);
});
test("scans width ranges and TypeScript query callers", () => {
  assert.equal(
    scanSource(
      "src/fixture.css",
      "@media (760px < width <= 1200px) {} @container lane (width < 50rem) {}",
    ).length,
    2,
  );
  assert.equal(
    scanSource(
      "src/fixture.ts",
      'window.matchMedia("(width < 761px)"); useMediaQuery(layoutQueries.teamCompact)',
    ).length,
    2,
  );
});
test("the desktop and compact CSS conditions cover fractional widths", () => {
  const source = readFileSync(
    new URL("../src/workspace-layout.css", import.meta.url),
    "utf8",
  );
  assert.doesNotMatch(source, /max-width:\s*760px/);
  assert.match(source, /width\s*<\s*761px/);
});

test("rejects an inclusive operator that changes the complement", () => {
  const errors = check(
    "src/fixture.css",
    "/* layout-query: navigation.inlineMin <= */\n@media (width <= 761px) {}",
  );
  assert.match(errors.join("\n"), /navigation.inlineMin.*src\/fixture.css/);
});

test("rejects stale generated output without a browser", () => {
  const root = mkdtempSync(join(tmpdir(), "layout-contract-"));
  try {
    mkdirSync(join(root, "src/layout"), { recursive: true });
    writeFileSync(
      join(root, "src/layout/layout-contract.json"),
      JSON.stringify(contract),
    );
    generateLayoutContract(root);
    generateLayoutContract(root, true);
    writeFileSync(
      join(root, "src/layout/layout-contract.generated.ts"),
      "export const layoutValues = {};\n",
    );
    assert.throws(
      () => generateLayoutContract(root, true),
      /layout-contract.json: stale generated consumer src\/layout\/layout-contract.generated.ts/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
