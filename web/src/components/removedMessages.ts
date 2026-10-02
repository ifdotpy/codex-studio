import { useCallback, useMemo, useSyncExternalStore } from "react";
import type { Message } from "../types";

const changed = "studio-removed-messages-changed";

export function messageRemovalKeys(message: Message, chat: string) {
  const keys = [`item:${message.id}`];
  const clientId = message.clientMessageId;
  if (clientId) {
    // The same receipt can become a native input after a delayed acknowledgement.
    keys.push(
      `client:${clientId}`,
      `item:${clientId}`,
      `item:${chat}:${clientId}`,
      `item:${chat}:${clientId}:0`,
    );
  }
  return keys;
}

function parseKeys(value: string) {
  try {
    const keys: unknown = JSON.parse(value);
    return new Set(
      Array.isArray(keys)
        ? keys.filter((key): key is string => typeof key === "string")
        : [],
    );
  } catch {
    return new Set<string>();
  }
}

export const removedMessagesStorageKey = (
  workspace: string,
  kind: string,
  chat: string | null,
) => `studio-removed-messages:${JSON.stringify([workspace, kind, chat])}`;

export function isRemovedMessage(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
) {
  const keys = parseKeys(
    localStorage.getItem(removedMessagesStorageKey(workspace, kind, chat)) ||
      "[]",
  );
  return messageRemovalKeys(message, chat).some((key) => keys.has(key));
}

export function removeMessageFromDevice(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
) {
  const storageKey = removedMessagesStorageKey(workspace, kind, chat);
  saveRemovedMessage(workspace, kind, chat, message);
  const next = parseKeys(localStorage.getItem(storageKey) || "[]");
  for (const key of messageRemovalKeys(message, chat)) next.add(key);
  localStorage.setItem(storageKey, JSON.stringify([...next]));
  window.dispatchEvent(new CustomEvent(changed, { detail: storageKey }));
}

export function saveRemovedMessage(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
) {
  const key = `${removedMessagesStorageKey(workspace, kind, chat)}:copies`;
  const copies: Message[] = JSON.parse(localStorage.getItem(key) || "[]");
  const same = (item: Message) =>
    item.id === message.id ||
    (message.clientMessageId &&
      item.clientMessageId === message.clientMessageId);
  localStorage.setItem(
    key,
    JSON.stringify([...copies.filter((item) => !same(item)), message]),
  );
}

export function useRemovedMessages(
  workspace: string,
  kind: string,
  chat: string | null,
) {
  const storageKey = removedMessagesStorageKey(workspace, kind, chat);
  const snapshot = useCallback(() => {
    try {
      return localStorage.getItem(storageKey) || "[]";
    } catch {
      return "[]";
    }
  }, [storageKey]);
  const subscribe = useCallback(
    (notify: () => void) => {
      const storage = (event: StorageEvent) => {
        if (event.key === storageKey || event.key === null) notify();
      };
      const local = (event: Event) => {
        if ((event as CustomEvent<string>).detail === storageKey) notify();
      };
      window.addEventListener("storage", storage);
      window.addEventListener(changed, local);
      return () => {
        window.removeEventListener("storage", storage);
        window.removeEventListener(changed, local);
      };
    },
    [storageKey],
  );
  const serialized = useSyncExternalStore(subscribe, snapshot);
  const keys = useMemo(() => parseKeys(serialized), [serialized]);
  const hidden = useCallback(
    (message: Message) =>
      messageRemovalKeys(message, chat || "").some((key) => keys.has(key)),
    [chat, keys],
  );
  return {
    hidden,
    restored: useMemo<Message[]>(() => {
      // Restored copies are display records. They never return to the outbox.
      void serialized;
      let copies: Message[];
      try {
        const saved: unknown = JSON.parse(
          localStorage.getItem(`${storageKey}:restored`) || "[]",
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
    }, [serialized, storageKey]),
    hasRemoved: keys.size > 0,
    restore: () => {
      localStorage.setItem(
        `${storageKey}:restored`,
        localStorage.getItem(`${storageKey}:copies`) || "[]",
      );
      localStorage.removeItem(storageKey);
      window.dispatchEvent(new CustomEvent(changed, { detail: storageKey }));
    },
    remove: (message: Message) => {
      // Report a storage failure instead of hiding a card that reappears on reload.
      removeMessageFromDevice(workspace, kind, chat || "", message);
    },
  };
}
