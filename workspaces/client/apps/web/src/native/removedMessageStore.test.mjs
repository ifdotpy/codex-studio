import { expect, it, vi } from "vitest";
vi.mock("react", () => ({
  useMemo: (read) => read(),
  useCallback: (callback) => callback,
  useSyncExternalStore: (_subscribe, read) => read(),
}));
import { createNativeRemovedMessageStore } from "./removedMessageStore";
import { useRemovedMessagesView } from "../components/removedMessagesView";
import {
  removedMessagesStorageKey,
  removeMessageFromDeviceWithStorage,
  isRemovedMessage,
} from "../components/removedMessageStorage";

function fixture() {
  const values = new Map();
  const storage = {
    get length() {
      return values.size;
    },
    key: (index) => [...values.keys()][index] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, raw) => values.set(key, raw),
    removeItem: (key) => values.delete(key),
    clear: () => values.clear(),
  };
  const updateLocalDraft = vi.fn(async (key, fallback, update) => {
    const next = update(
      JSON.parse(values.get(key) ?? JSON.stringify(fallback)),
    );
    if (next === null) values.delete(key);
    else values.set(key, JSON.stringify(next));
    return next;
  });
  return {
    values,
    storage,
    store: createNativeRemovedMessageStore(storage, { updateLocalDraft }),
  };
}
const message = {
  id: "original-item",
  clientMessageId: "original-user-request",
  role: "user",
  text: "Keep this text",
  deliveryStatus: "dispatching",
};

it("restores the exact display copy without creating another outgoing user request", async () => {
  const f = fixture();
  await f.store.removeMessageFromDevice("workspace", "agent", "chat", message);
  expect(
    isRemovedMessage("workspace", "agent", "chat", message, f.storage),
  ).toBe(true);
  await f.store.restore("workspace", "agent", "chat");
  const view = useRemovedMessagesView("workspace", "agent", "chat", f.storage, {
    subscribe: vi.fn(),
  });
  expect(view.hasRemoved).toBe(false);
  expect(view.restored).toEqual([
    {
      ...message,
      pending: false,
      localDelivery: false,
      materialized: true,
      deliveryStatus: "uncertain",
    },
  ]);
  expect(view.restored[0].clientMessageId).toBe("original-user-request");
});

it("keeps the browser malformed-hidden-key recovery behavior", () => {
  const f = fixture();
  f.storage.setItem(
    removedMessagesStorageKey("workspace", "agent", "chat"),
    "invalid JSON",
  );
  removeMessageFromDeviceWithStorage(
    "workspace",
    "agent",
    "chat",
    message,
    f.storage,
  );
  expect(
    isRemovedMessage("workspace", "agent", "chat", message, f.storage),
  ).toBe(true);
});
