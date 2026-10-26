import { useCallback, useMemo, useSyncExternalStore } from "react";
import type { Message } from "../types";
import {
  messageRemovalKeys,
  parseRemovedMessageKeys,
  removedMessagesStorageKey,
} from "./removedMessageStorage";

export function useRemovedMessagesView(
  workspace: string,
  kind: string,
  chat: string | null,
  storage: Storage,
  changes: { subscribe(listener: (key: string | null) => void): () => void },
) {
  const storageKey = removedMessagesStorageKey(workspace, kind, chat);
  const snapshot = useCallback(() => {
    try {
      return storage.getItem(storageKey) || "[]";
    } catch {
      return "[]";
    }
  }, [storageKey, storage]);
  const subscribe = useCallback(
    (notify: () => void) =>
      changes.subscribe((key) => {
        if (key === null || key === storageKey) notify();
      }),
    [changes, storageKey],
  );
  const serialized = useSyncExternalStore(subscribe, snapshot, snapshot);
  const keys = useMemo(() => parseRemovedMessageKeys(serialized), [serialized]);
  const hidden = useCallback(
    (message: Message) =>
      messageRemovalKeys(message, chat || "").some((key) => keys.has(key)),
    [chat, keys],
  );
  const restored = useMemo<Message[]>(() => {
    void serialized;
    let copies: Message[];
    try {
      const saved: unknown = JSON.parse(
        storage.getItem(`${storageKey}:restored`) || "[]",
      );
      copies = Array.isArray(saved)
        ? saved.filter(
            (message): message is Message =>
              message !== null &&
              typeof message === "object" &&
              typeof message.id === "string" &&
              typeof message.text === "string",
          )
        : [];
    } catch {
      return [];
    }
    return copies.map((message) => ({
      ...message,
      pending: false,
      localDelivery: false,
      materialized: true,
      deliveryStatus: ["sending", "reserved", "dispatching"].includes(
        message.deliveryStatus || "",
      )
        ? "uncertain"
        : message.deliveryStatus,
    }));
  }, [serialized, storageKey, storage]);
  return { hidden, restored, hasRemoved: keys.size > 0 };
}
