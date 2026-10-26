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
  assert.ok(
    workspace &&
      "op" in workspace &&
      workspace.op === "upsert" &&
      workspace.collection === "workspace",
  );
  assert.equal(workspace.value.stateDir, undefined);
});

test("entity projection follows the base allowlist insertion order", () => {
  const page = entityPullFixture({
    chats: [],
    edges: [],
    runtime: {
      projects: [
        {
          id: "project",
          name: "Name",
          homeServerId: "server",
          path: "/fixture",
        },
      ],
    },
  });
  assert.ok("documents" in page);
  const project = page.documents.find(
    (document) => "collection" in document && document.collection === "project",
  );
  assert.ok(
    project &&
      "op" in project &&
      project.op === "upsert" &&
      project.collection === "project",
  );
  assert.deepEqual(Object.keys(project.value), [
    "homeServerId",
    "id",
    "path",
    "name",
  ]);
});
