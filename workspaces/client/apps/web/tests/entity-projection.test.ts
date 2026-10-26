import assert from "node:assert/strict";
import test from "node:test";
import { entityPullFixture } from "./playwright.ts";

test("entity projection does not inherit the runtime state directory", () => {
  const snapshot = {
    chats: [],
    edges: [],
    runtime: { stateDir: "runtime-only" },
  };
  const page = entityPullFixture(snapshot);
  assert.ok("documents" in page);
  const workspace = page.documents.find(
    (document) =>
      "collection" in document && document.collection === "workspace",
  );
  assert.ok(workspace && "op" in workspace && workspace.op === "upsert");
  assert.equal(Object.hasOwn(workspace.value, "stateDir"), false);
});
