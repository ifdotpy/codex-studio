import { afterEach, expect, it, vi } from "vitest";
vi.mock("./client", () => {
  const nativeRequest = vi.fn();
  const listenNativeEvents = vi.fn();
  return {
    nativeRequest,
    listenNativeEvents,
    hasNativeClient: () => true,
    localCore: {
      server: "local",
      request: nativeRequest,
      listen: listenNativeEvents,
    },
  };
});
vi.mock("./frame", () => ({ isNativeFrame: () => false }));
import { nativeRequest } from "./client";
import { createNativeOutbox, prepareMessage, releaseMessage } from "./outbox";

afterEach(() => {
  vi.resetAllMocks();
});

const body = { id: "message-one", room: "room", text: "Hello" };
const legacy = async () => ({
  workspaceId: "w",
  db: { outbox: { find: () => ({ exec: async () => [] }) } },
});

it("a lost delivery reply keeps the message queued under its one identity", async () => {
  const stored = {
    id: body.id,
    body,
    status: "queued",
    attempted: true,
    created: 1,
    error: "offline",
  };
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "identity.get") return { workspaceId: "w" };
    if (method === "outbox.deliver") throw new Error("lost reply");
    if (method === "outbox.list") return [stored];
    return stored;
  });
  const persisted = vi.fn();
  const result = await createNativeOutbox(legacy).durableSend(
    body,
    [],
    persisted,
  );
  expect(result).toMatchObject({
    kind: "local",
    queued: true,
    error: "offline",
  });
  expect(persisted).toHaveBeenCalledOnce();
  const calls = vi.mocked(nativeRequest).mock.calls;
  // The page never repeats the send; the core retries the stored message.
  expect(calls.filter(([method]) => method === "outbox.deliver")).toHaveLength(
    1,
  );
  expect(calls.find(([method]) => method === "outbox.enqueue")).toEqual([
    "outbox.enqueue",
    { workspace: "w", body, attachments: [] },
    "outbox.enqueue:message-one",
  ]);
});

it("a failed message reports the stored failure", async () => {
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "identity.get") return { workspaceId: "w" };
    if (method === "outbox.deliver") throw new Error("Message failed.");
    if (method === "outbox.list")
      return [
        { id: body.id, body, status: "failed", created: 1, error: "Too long" },
      ];
    return null;
  });
  const result = await createNativeOutbox(legacy).durableSend(body);
  expect(result).toMatchObject({ status: "failed", error: "Too long" });
});

it("removing a card asks the core to stop the message identity", async () => {
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "identity.get") return { workspaceId: "w" };
    if (method === "outbox.stop") return { cancelled: true };
    return null;
  });
  expect(await createNativeOutbox(legacy).stopRemovedMessage("one")).toBe(true);
  expect(vi.mocked(nativeRequest).mock.calls.at(-1)).toEqual([
    "outbox.stop",
    { workspace: "w", id: "one" },
  ]);
});

const message = {
  room: "room",
  text: "Hello",
  assets: [],
  delivery: "queue" as const,
};

it("the pending send comes from the core, with its identity", async () => {
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "identity.get") return { workspaceId: "w" };
    return { id: "core-id", ...message };
  });
  expect(await prepareMessage(message)).toEqual({ id: "core-id", ...message });
  expect(vi.mocked(nativeRequest)).toHaveBeenCalledWith("outbox.prepare", {
    workspace: "w",
    ...message,
  });
});

it("releasing a pending send names its room and identity", async () => {
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "identity.get") return { workspaceId: "w" };
    return { released: true };
  });
  await releaseMessage("room", "core-id");
  expect(vi.mocked(nativeRequest)).toHaveBeenLastCalledWith("outbox.release", {
    workspace: "w",
    room: "room",
    id: "core-id",
  });
});

it("a pending send reply without a message identity is refused", async () => {
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "identity.get") return { workspaceId: "w" };
    return { room: "room" };
  });
  await expect(prepareMessage(message)).rejects.toThrow("message identity");
});
