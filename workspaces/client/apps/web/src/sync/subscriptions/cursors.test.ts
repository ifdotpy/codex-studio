import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { afterEach, expect, it, vi } from "vitest";
import {
  cursorDocumentId,
  parseAppliedCursor,
  subscriptionKey,
} from "./cursors";

afterEach(() => vi.unstubAllGlobals());

it("keeps exact canonical cursor hashes on HTTP without crypto.subtle", async () => {
  vi.stubGlobal("crypto", {});
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(
          readFileSync(
            new URL("../../wasm/generated/client_bg.wasm", import.meta.url),
          ),
          { headers: { "Content-Type": "application/wasm" } },
        ),
    ),
  );
  const resource = { kind: "transcript", agentId: "Привет 🦀" } as const;
  const expected = `subscription:cursor:${createHash("sha256")
    .update(subscriptionKey(resource))
    .digest("hex")}`;
  expect(await cursorDocumentId(resource)).toBe(expected);
  expect(
    await cursorDocumentId({ agentId: resource.agentId, kind: resource.kind }),
  ).toBe(expected);
});

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
