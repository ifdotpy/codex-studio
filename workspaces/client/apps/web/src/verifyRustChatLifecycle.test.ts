import type { components } from "./generated/api.ts";
import {
  parseVerifierApi,
  readVerifierRequest,
  readVerifierResponse,
} from "../scripts/verifierApi.ts";
import { expect, it, vi } from "vitest";
import {
  isAcceptedMessageReceiptStatus,
  selectedChatRowSelector,
  readPendingCreationIds,
} from "../scripts/verify-rust-chat-lifecycle.ts";

it("accepts durable pending inputs without treating uncertainty as delivery", () => {
  for (const status of ["pending", "queued", "accepted", "delivered", "sent"])
    expect(isAcceptedMessageReceiptStatus(status), status).toBe(true);
  for (const status of [
    undefined,
    null,
    "failed",
    "cancelled",
    "uncertain",
    "reserved",
    "dispatching",
  ])
    expect(isAcceptedMessageReceiptStatus(status), String(status)).toBe(false);
});

it("matches selected chat row values used by project and sidebar rows", () => {
  expect(selectedChatRowSelector("new-chat")).toBe(
    '[data-chat="new-chat"][aria-current="true"], [data-chat="new-chat"][aria-current="page"]',
  );
});

it("keeps generated API and request JSON failures at their named boundaries", async () => {
  const agent = {
    id: "lead",
  } satisfies components["schemas"]["AgentEntityDto"];
  expect(
    parseVerifierApi<components["schemas"]["AgentEntityDto"]>(
      JSON.stringify(agent),
    ),
  ).toEqual(agent);
  expect(() =>
    parseVerifierApi<components["schemas"]["AgentEntityDto"]>("{"),
  ).toThrow(SyntaxError);
  await expect(
    readVerifierResponse<components["schemas"]["AgentEntityDto"]>(
      new Response(JSON.stringify(agent)),
    ),
  ).resolves.toEqual(agent);
  await expect(
    readVerifierResponse<components["schemas"]["AgentEntityDto"]>(
      new Response("{"),
    ),
  ).rejects.toBeInstanceOf(SyntaxError);
  const request = {
    id: "message",
    room: "lead",
    text: "hello",
  } satisfies components["schemas"]["ManagedMessageBody"];
  expect(
    readVerifierRequest<components["schemas"]["ManagedMessageBody"]>({
      postDataJSON: () => request,
    }),
  ).toEqual(request);
  expect(() =>
    readVerifierRequest<components["schemas"]["ManagedMessageBody"]>({
      postDataJSON: () => {
        throw new SyntaxError("Malformed submitted JSON");
      },
    }),
  ).toThrow(SyntaxError);
});

it("reads browser-owned pending identities and retains malformed-storage failure", () => {
  const records = new Map([
    ["unrelated", "{"],
    ["codex-pending-creation:lead", JSON.stringify({ id: "lead" })],
    ["codex-pending-creation:empty", ""],
  ]);
  const storage = {
    get length() {
      return records.size;
    },
    key: (index: number) => [...records.keys()][index] ?? null,
    getItem: (key: string) => records.get(key) ?? null,
  } satisfies Pick<Storage, "length" | "key" | "getItem">;
  vi.stubGlobal("localStorage", storage);
  try {
    expect(readPendingCreationIds()).toEqual(["lead"]);
    records.set("codex-pending-creation:malformed", "{");
    expect(() => readPendingCreationIds()).toThrow(SyntaxError);
  } finally {
    vi.unstubAllGlobals();
  }
});
