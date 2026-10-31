import { useServerStorage } from "../servers/ServerServicesContext";
import { serverLocalStorage } from "../servers/storage";
import { useCallback, useMemo, useSyncExternalStore } from "react";
import type { Message } from "../types";

const changed = "studio-removed-messages-changed";
type Removed = {
  keys: Set<string>;
  copies: Message[];
  restored: Message[];
  revision: number;
};
const owners = new WeakMap<Storage, Map<string, Removed>>();
function record(storage: Storage, key: string): Removed {
  let records = owners.get(storage);
  if (!records) owners.set(storage, (records = new Map()));
  let value = records.get(key);
  if (!value)
    records.set(
      key,
      (value = { keys: new Set(), copies: [], restored: [], revision: 0 }),
    );
  return value;
}

export function messageRemovalKeys(message: Message, chat: string) {
  const keys = [`item:${message.id}`];
  const clientId = message.clientMessageId;
  if (clientId)
    keys.push(
      `client:${clientId}`,
      `item:${clientId}`,
      `item:${chat}:${clientId}`,
      `item:${chat}:${clientId}:0`,
    );
  return keys;
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
  storage = serverLocalStorage,
) {
  const value = record(
    storage,
    removedMessagesStorageKey(workspace, kind, chat),
  );
  return messageRemovalKeys(message, chat).some((key) => value.keys.has(key));
}
export function saveRemovedMessage(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
  storage = serverLocalStorage,
) {
  const value = record(
    storage,
    removedMessagesStorageKey(workspace, kind, chat),
  );
  value.copies = value.copies.filter(
    (item) =>
      item.id !== message.id &&
      (!message.clientMessageId ||
        item.clientMessageId !== message.clientMessageId),
  );
  value.copies.push(message);
}
export function removeMessageFromDevice(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
  storage = serverLocalStorage,
) {
  const key = removedMessagesStorageKey(workspace, kind, chat);
  saveRemovedMessage(workspace, kind, chat, message, storage);
  const value = record(storage, key);
  for (const item of messageRemovalKeys(message, chat)) value.keys.add(item);
  value.revision++;
  window.dispatchEvent(new CustomEvent(changed, { detail: key }));
}
export function useRemovedMessages(
  workspace: string,
  kind: string,
  chat: string | null,
) {
  const storage = useServerStorage();
  const key = removedMessagesStorageKey(workspace, kind, chat);
  const value = record(storage, key);
  const snapshot = useCallback(() => value.revision, [value]);
  const subscribe = useCallback(
    (notify: () => void) => {
      const local = (event: Event) => {
        if ((event as CustomEvent<string>).detail === key) notify();
      };
      window.addEventListener(changed, local);
      return () => window.removeEventListener(changed, local);
    },
    [key],
  );
  const revision = useSyncExternalStore(subscribe, snapshot);
  const hidden = useCallback(
    (message: Message) =>
      messageRemovalKeys(message, chat || "").some((item) =>
        value.keys.has(item),
      ),
    [chat, value, revision],
  );
  const restored = useMemo(
    () =>
      value.restored.map((message) => ({
        ...message,
        pending: false,
        localDelivery: false,
        materialized: true,
        deliveryStatus: ["sending", "reserved", "dispatching"].includes(
          message.deliveryStatus || "",
        )
          ? "uncertain"
          : message.deliveryStatus,
      })),
    [value, revision],
  );
  return {
    hidden,
    restored,
    hasRemoved: value.keys.size > 0,
    restore: () => {
      value.restored = value.copies;
      value.keys.clear();
      value.revision++;
      window.dispatchEvent(new CustomEvent(changed, { detail: key }));
    },
    remove: (message: Message) =>
      removeMessageFromDevice(workspace, kind, chat || "", message, storage),
  };
}
