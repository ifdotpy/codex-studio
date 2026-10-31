import { required } from "./componentTestFixtures";
import { afterEach, expect, it, vi } from "vitest";
vi.mock("../servers/ServerServicesContext", () => ({}));
vi.mock("../api", () => ({
  get: vi.fn(),
  command: { make: vi.fn(), run: vi.fn() },
  ApiError: class extends Error {},
}));
vi.mock("../sync/send", () => ({ stopRemovedMessage: vi.fn() }));
vi.mock("../hooks", () => ({ transcriptMessages: (items: unknown) => items }));
import { createSendingMessageRemoval } from "./removeSendingMessages";
import { isRemovedMessage, removedMessagesStorageKey } from "./removedMessages";
import { scopedStorage } from "../servers/storage";
import type { Message } from "../types";

afterEach(() => vi.unstubAllGlobals());
it("retains cancellation and recoverable copies on the initiating server after an unknown reply", async () => {
  const values = new Map<string, string>();
  const underlying = {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
  } as Storage;
  vi.stubGlobal("window", new EventTarget());
  const a = scopedStorage(() => underlying, "server-a:");
  const b = scopedStorage(() => underlying, "server-b:");
  const scope = { stateDir: "same", workspaceId: "same" };
  const message: Message = {
    id: "same",
    clientMessageId: "same",
    role: "user",
    text: "private-a",
    deliveryStatus: "sending",
  };
  const read = vi.fn().mockResolvedValue({
    items: [{ id: "same", kind: "user", text: "private-a" }],
    revision: "revision-a",
    capabilities: { receipts: true },
  });
  const sendA = vi
    .fn()
    .mockRejectedValueOnce(new Error("lost reply"))
    .mockResolvedValue({});
  const sendB = vi.fn().mockResolvedValue({});
  const stopA = vi.fn().mockResolvedValue(false),
    stopB = vi.fn().mockResolvedValue(false);
  let counter = 0;
  const make = (kind: string, target: string, body: unknown) => ({
    id: `command-${++counter}`,
    client: "web",
    number: counter,
    kind,
    target,
    body,
  });
  const ownerA = createSendingMessageRemoval({
    localStorage: a,
    get: read,
    command: { make: make as never, run: sendA },
    stopRemovedMessage: stopA,
  });
  const ownerB = createSendingMessageRemoval({
    localStorage: b,
    get: read,
    command: { make: make as never, run: sendB },
    stopRemovedMessage: stopB,
  });
  await expect(
    ownerA.removeSendingMessage(scope, { chat: "same", message }),
  ).rejects.toThrow("lost reply");
  expect(sendA).toHaveBeenCalledTimes(1);
  const first = required(required(sendA.mock.calls[0])[0]);
  expect(isRemovedMessage("same", "agent", "same", message, a)).toBe(false);
  const copies = `${removedMessagesStorageKey("same", "agent", "same")}:copies`;
  expect(a.getItem(copies)).toContain("private-a");
  expect(b.getItem(copies)).toBeNull();
  await ownerB.removeSendingMessage(scope, {
    chat: "same",
    message: { ...message, text: "private-b" },
  });
  expect(required(required(sendB.mock.calls[0])[0]).id).not.toBe(first.id);
  expect(isRemovedMessage("same", "agent", "same", message, a)).toBe(false);
  await ownerA.removeSendingMessage(scope, { chat: "same", message });
  expect(required(required(sendA.mock.calls[1])[0])).toEqual(first);
  expect(sendB).toHaveBeenCalledTimes(1);
  expect(stopA).toHaveBeenCalledTimes(2);
  expect(stopB).toHaveBeenCalledTimes(1);
  expect(isRemovedMessage("same", "agent", "same", message, a)).toBe(true);
  expect(a.getItem(copies)).toContain("private-a");
  expect(a.getItem(copies)).not.toContain("private-b");
  expect(b.getItem(copies)).toContain("private-b");
});
