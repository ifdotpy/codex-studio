import assert from "node:assert/strict";
import { it } from "vitest";
import { navigationSnapshot } from "./navigation.ts";

it("shows the active target chat and hides the source move record", () => {
  const chat = {
    id: "same-id",
    source: "managed",
    isLead: true,
    cwd: "/project",
  };
  const source = navigationSnapshot(
    { threads: [{ ...chat, movedTo: { server: "target" } }] },
    null,
    "",
  );
  const target = navigationSnapshot(
    { threads: [{ ...chat, movedFrom: { server: "source" } }] },
    null,
    "",
  );
  assert.deepEqual(source.chats, []);
  assert.deepEqual(
    navigationSnapshot(
      { threads: [{ ...chat, moveImportPending: true }] },
      null,
      "",
    ).chats,
    [],
  );
  assert.deepEqual(
    target.chats.map((row) => row.id),
    ["same-id"],
  );
});
