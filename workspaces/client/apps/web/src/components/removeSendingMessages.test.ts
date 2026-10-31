import { required } from "./componentTestFixtures";
import type { GetResult } from "../apiTypes";
import type { Message } from "../types";
import type { get } from "../api";
import type { CommandRequest } from "../commands";
import type { stopRemovedMessage } from "../sync/send";
import type {
  saveRemovedMessage,
  removeMessageFromDevice,
} from "./removedMessages";
import { serverLocalStorage } from "../servers/storage";
import assert from "node:assert/strict";
import { afterEach, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  get: vi.fn<typeof get>(),
  run: vi.fn<
    (command: CommandRequest, options?: unknown) => Promise<unknown>
  >(),
  make: vi.fn((kind: string, target: string, body: unknown) => ({
    id: "command-made",
    client: "web",
    number: 1,
    kind,
    target,
    body,
  })),
  stopRemovedMessage: vi.fn<typeof stopRemovedMessage>(),
  saveRemovedMessage: vi.fn<typeof saveRemovedMessage>(),
  removeMessageFromDevice: vi.fn<typeof removeMessageFromDevice>(),
}));

vi.mock("../api", () => ({
  get: mocks.get,
  command: { make: mocks.make, run: mocks.run },
  ApiError: class ApiError extends Error {
    declare status: number;
    constructor(message: string, status: number) {
      super(message);
      this.status = status;
    }
  },
}));
vi.mock("../sync/send", () => ({
  stopRemovedMessage: mocks.stopRemovedMessage,
}));
vi.mock("../hooks", () => ({
  transcriptMessages: (items: Message[]) => items,
}));
vi.mock("./removedMessages", () => ({
  isRemovedMessage: () => false,
  removeMessageFromDevice: mocks.removeMessageFromDevice,
  saveRemovedMessage: mocks.saveRemovedMessage,
}));

import { removeSendingMessage } from "./removeSendingMessages.ts";

const storage = new Map<string, string>();
vi.stubGlobal("localStorage", {
  getItem: (key: string) => storage.get(key) ?? null,
  setItem: (key: string, value: string) => storage.set(key, value),
});

const scope = { stateDir: "state", workspaceId: "workspace" };
const message: Message = {
  id: "chat:client-1",
  clientMessageId: "client-1",
  role: "user",
  text: "hello",
  deliveryStatus: "sending",
};
const key = `studio-remove-sending:${JSON.stringify([
  scope.stateDir,
  scope.workspaceId,
  "chat",
  "client-1",
])}`;
const saved = {
  id: "command-fixed",
  client: "web",
  number: 4,
  kind: "withdraw",
  target: "chat",
  body: { message: "client-1" },
} satisfies CommandRequest;

function savedId(): unknown {
  const value: unknown = JSON.parse(storage.get(key)!);
  return value && typeof value === "object" && "id" in value
    ? value.id
    : undefined;
}

afterEach(() => {
  storage.clear();
  vi.clearAllMocks();
});

it("sends the saved withdraw command with its identity", async () => {
  storage.set(key, JSON.stringify(saved));
  mocks.stopRemovedMessage.mockResolvedValue(false);
  mocks.run.mockResolvedValue({});

  await removeSendingMessage(scope, { chat: "chat", message });

  assert.deepEqual(mocks.run.mock.calls, [
    [saved, { workspaceId: "workspace" }],
  ]);
  assert.deepEqual(mocks.removeMessageFromDevice.mock.calls, [
    [
      "state",
      "agent",
      "chat",
      { ...message, deliveryStatus: "cancelled" },
      serverLocalStorage,
    ],
  ]);
  assert.equal(savedId(), "command-fixed");
});

it("loads cancellation eligibility through the typed queue query", async () => {
  mocks.stopRemovedMessage.mockResolvedValue(false);
  // Queue fixture omits item.created, item.status and capabilities.reorder.
  const queueFixture = {
    items: [{ id: "client-1", kind: "user", text: "hello" }],
    revision: "revision-7",
    capabilities: { receipts: true },
  } as GetResult<"/api/queue">;
  mocks.get.mockResolvedValue(queueFixture);
  mocks.run.mockResolvedValue({});

  await removeSendingMessage(scope, { chat: "chat", message });

  assert.deepEqual(mocks.get.mock.calls, [
    ["/api/queue", { query: { agent: "chat" }, workspaceId: "workspace" }],
  ]);
  assert.deepEqual(mocks.make.mock.calls, [
    ["withdraw", "chat", { message: "client-1" }],
  ]);
  const [command, options] = required(mocks.run.mock.calls[0]);
  assert.deepEqual(
    { kind: command.kind, target: command.target, body: command.body },
    { kind: "withdraw", target: "chat", body: { message: "client-1" } },
  );
  assert.deepEqual(options, { workspaceId: "workspace" });
  assert.equal(savedId(), "command-made");
});

it("retains the same command when its write outcome is unknown", async () => {
  storage.set(key, JSON.stringify(saved));
  mocks.stopRemovedMessage.mockResolvedValue(false);
  const unknown = new TypeError("connection lost");
  mocks.run.mockRejectedValue(unknown);

  await assert.rejects(
    removeSendingMessage(scope, { chat: "chat", message }),
    (error) => error === unknown,
  );

  assert.equal(savedId(), "command-fixed");
  assert.deepEqual(mocks.removeMessageFromDevice.mock.calls, []);
});

it("refuses a saved cancellation that is not a withdraw of this message", async () => {
  storage.set(
    key,
    JSON.stringify({
      ...saved,
      id: "command-unknown",
      body: { message: "other" },
    }),
  );

  await assert.rejects(removeSendingMessage(scope, { chat: "chat", message }), {
    message: "The saved message cancellation cannot be verified.",
  });

  assert.equal(savedId(), "command-unknown");
  assert.deepEqual(mocks.run.mock.calls, []);
  assert.deepEqual(mocks.removeMessageFromDevice.mock.calls, []);
});
