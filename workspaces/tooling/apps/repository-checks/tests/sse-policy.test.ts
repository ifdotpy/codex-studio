import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { checkBaseline, scan, violations } from "../sse-policy.ts";

test("detects new request and persistence access while permitting transport owners", () => {
  const source = `fetch('/'); api.syncGet('/'); client.POST('/'); window.localStorage.setItem('x', 'y'); indexedDB.open('x');`;
  const found = violations("components/view.tsx", source);
  assert.deepEqual(
    found.map((item) => item.callee),
    [
      "fetch",
      "api.syncGet",
      "client.POST",
      "window.localStorage",
      "indexedDB.open",
    ],
  );
  assert.deepEqual(
    violations(
      "component.ts",
      "window['fetch']('/'); window['localStorage'].clear();",
    ).map((item) => item.callee),
    ["window['fetch']", "window['localStorage']"],
  );
  assert.deepEqual(
    violations("component.ts", "const store = localStorage;").map(
      (item) => item.callee,
    ),
    ["localStorage"],
  );
  assert.deepEqual(violations("api.ts", "fetch('/');"), []);
  assert.deepEqual(
    violations("browserPersistence.ts", "indexedDB.open('x');"),
    [],
  );
  assert.deepEqual(
    violations(
      "components/view.tsx",
      "const text = 'fetch() localStorage'; // fetch()\n const value = 1;",
    ),
    [],
  );
});

test("baseline fails on new, removed, and duplicate sites", () => {
  const baseline = violations("component.ts", "fetch('/a');");
  assert.deepEqual(checkBaseline(baseline, baseline), []);
  assert.match(
    checkBaseline(
      violations("component.ts", "fetch('/a'); fetch('/b');"),
      baseline,
    ).join("\n"),
    /new/,
  );
  assert.match(checkBaseline([], baseline).join("\n"), /stale/);
});

// Every retained site must disappear from the baseline when its caller moves.
test("web request and persistence sites match the shrinking baseline", () => {
  const root = new URL("../../../../../", import.meta.url);
  const baseline = JSON.parse(
    readFileSync(
      new URL(
        "workspaces/tooling/apps/repository-checks/sse-policy-baseline.json",
        root,
      ),
      "utf8",
    ),
  );
  const actual = scan(
    fileURLToPath(new URL("workspaces/client/apps/web/src/", root)),
  );
  assert.deepEqual(checkBaseline(actual, baseline), []);
});
