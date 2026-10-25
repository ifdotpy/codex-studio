import { expect, it } from "vitest";
import { cursorDocumentId, parseAppliedCursor } from "./cursors";

it("keeps a separate durable cursor for each canonical ref and database", async () => {
  const a = { kind: "transcript", agentId: "a" } as const;
  const b = { kind: "transcript", agentId: "b" } as const;
  expect(await cursorDocumentId(a)).toBe(
    await cursorDocumentId({ agentId: "a", kind: "transcript" }),
  );
  expect(await cursorDocumentId(a)).not.toBe(await cursorDocumentId(b));
  const payload = JSON.stringify({ workspaceId: "db", seq: 9 });
  expect(parseAppliedCursor(payload, "db")).toEqual({
    workspaceId: "db",
    seq: 9,
  });
  expect(parseAppliedCursor(payload, "replaced-db")).toBeUndefined();
  for (const payload of [
    "{}",
    "{",
    '{"workspaceId":"db","seq":-1}',
    '{"workspaceId":"db","seq":9007199254740992}',
  ]) {
    expect(parseAppliedCursor(payload, "db")).toBeUndefined();
  }
});
