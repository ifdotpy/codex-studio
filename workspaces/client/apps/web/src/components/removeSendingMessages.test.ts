import type { Message } from "../types";
import type { get, syncPost, PostBody } from "../api";
import type { stopRemovedMessage } from "../sync/send";
import type {
  saveRemovedMessage,
  removeMessageFromDevice,
} from "./removedMessages";
import { required } from "./componentTestFixtures";
import { serverLocalStorage } from "../servers/storage";
import assert from "node:assert/strict";
import { afterEach, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  get: vi.fn<typeof get>(),
  syncPost: vi.fn<typeof syncPost<"/api/queue">>(),
  stopRemovedMessage: vi.fn<typeof stopRemovedMessage>(),
  saveRemovedMessage: vi.fn<typeof saveRemovedMessage>(),
  removeMessageFromDevice: vi.fn<typeof removeMessageFromDevice>(),
}));

vi.mock("../api", () => ({
  get: mocks.get,
  syncPost: mocks.syncPost,
  ApiError: class ApiError extends Error {
    status: number;
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

// This test writes every saved cancellation document in the same process.
function savedRequestId(key: string): unknown {
  const saved: unknown = JSON.parse(required(storage.get(key)));
  assert.ok(saved && typeof saved === "object" && "request_id" in saved);
  return saved.request_id;
}

afterEach(() => {
  storage.clear();
  vi.clearAllMocks();
});

it("reuses the saved queue request identity and bounded typed write", async () => {
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
  const request = {
    action: "cancel",
    agent: "chat",
    id: "client-1",
    expectedText: "hello",
    expected_revision: "revision-7",
    request_id: "request-fixed",
  } satisfies PostBody<"/api/queue">;
  storage.set(key, JSON.stringify(request));
  mocks.stopRemovedMessage.mockResolvedValue(false);
  mocks.syncPost.mockResolvedValue(
    {} satisfies Awaited<ReturnType<typeof syncPost<"/api/queue">>>,
  );

  await removeSendingMessage(scope, { chat: "chat", message });

  assert.deepEqual(mocks.syncPost.mock.calls, [
    ["/api/queue", request, { workspaceId: "workspace" }],
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
  assert.equal(savedRequestId(key), "request-fixed");
});

it("loads cancellation eligibility through the typed queue query", async () => {
  const scope = { stateDir: "state", workspaceId: "workspace" };
  const message: Message = {
    id: "chat:client-1",
    clientMessageId: "client-1",
    role: "user",
    text: "hello",
    deliveryStatus: "sending",
  };
  mocks.stopRemovedMessage.mockResolvedValue(false);
  mocks.get.mockResolvedValue({
    items: [
      {
        id: "client-1",
        kind: "user",
        text: "hello",
        created: 1,
        status: "pending",
      },
    ],
    revision: "revision-7",
    capabilities: { receipts: true, reorder: true },
  });
  mocks.syncPost.mockResolvedValue(
    {} satisfies Awaited<ReturnType<typeof syncPost<"/api/queue">>>,
  );

  await removeSendingMessage(scope, { chat: "chat", message });

  assert.deepEqual(mocks.get.mock.calls, [
    ["/api/queue", { query: { agent: "chat" }, workspaceId: "workspace" }],
  ]);
  const [path, request, options] = required(mocks.syncPost.mock.calls[0]);
  assert.equal(path, "/api/queue");
  assert.equal(required(options).workspaceId, "workspace");
  assert.equal(request.action, "cancel");
  assert.deepEqual(
    {
      action: request.action,
      agent: request.agent,
      id: request.id,
      expectedText: request.expectedText,
      expected_revision: request.expected_revision,
    },
    {
      action: "cancel",
      agent: "chat",
      id: "client-1",
      expectedText: "hello",
      expected_revision: "revision-7",
    },
  );
  assert.equal(typeof request.request_id, "string");
  const key = `studio-remove-sending:${JSON.stringify([
    scope.stateDir,
    scope.workspaceId,
    "chat",
    "client-1",
  ])}`;
  assert.equal(savedRequestId(key), request.request_id);
});

it("retains the same request when its write outcome is unknown", async () => {
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
  const request = {
    action: "cancel",
    agent: "chat",
    id: "client-1",
    expectedText: "hello",
    expected_revision: "revision-7",
    request_id: "request-fixed",
  } satisfies PostBody<"/api/queue">;
  storage.set(key, JSON.stringify(request));
  mocks.stopRemovedMessage.mockResolvedValue(false);
  const unknown = new TypeError("connection lost");
  mocks.syncPost.mockRejectedValue(unknown);

  await assert.rejects(
    removeSendingMessage(scope, { chat: "chat", message }),
    (error) => error === unknown,
  );

  assert.equal(savedRequestId(key), "request-fixed");
  assert.deepEqual(mocks.removeMessageFromDevice.mock.calls, []);
});

it("does not replace an unrecognized saved cancellation with a new identity", async () => {
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
  storage.set(key, JSON.stringify({ request_id: "request-unknown" }));

  await assert.rejects(removeSendingMessage(scope, { chat: "chat", message }), {
    message: "The saved message cancellation cannot be verified.",
  });

  assert.equal(savedRequestId(key), "request-unknown");
  assert.deepEqual(mocks.syncPost.mock.calls, []);
  assert.deepEqual(mocks.removeMessageFromDevice.mock.calls, []);
});
