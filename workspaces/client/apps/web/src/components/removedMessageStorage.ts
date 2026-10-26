import type { Message } from "../types";

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
export function parseRemovedMessageKeys(value: string) {
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
  storage: Pick<Storage, "getItem">,
) {
  const keys = parseRemovedMessageKeys(
    storage.getItem(removedMessagesStorageKey(workspace, kind, chat)) || "[]",
  );
  return messageRemovalKeys(message, chat).some((key) => keys.has(key));
}
export type RemovedMessageUpdate = <T>(
  key: string,
  fallback: T,
  update: (current: T) => T,
) => T | Promise<T>;
const copiesWith = (copies: Message[], message: Message) => [
  ...copies.filter(
    (item) =>
      !(
        item.id === message.id ||
        (message.clientMessageId &&
          item.clientMessageId === message.clientMessageId)
      ),
  ),
  message,
];
const keysWith = (keys: unknown, added: readonly string[]) => [
  ...new Set([
    ...(Array.isArray(keys)
      ? keys.filter((key): key is string => typeof key === "string")
      : []),
    ...added,
  ]),
];
export function saveRemovedMessageWithStorage(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
  storage: Storage,
) {
  const key = `${removedMessagesStorageKey(workspace, kind, chat)}:copies`;
  storage.setItem(
    key,
    JSON.stringify(
      copiesWith(JSON.parse(storage.getItem(key) || "[]"), message),
    ),
  );
}
export function removeMessageFromDeviceWithStorage(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
  storage: Storage,
) {
  const key = removedMessagesStorageKey(workspace, kind, chat);
  saveRemovedMessageWithStorage(workspace, kind, chat, message, storage);
  storage.setItem(
    key,
    JSON.stringify(
      keysWith(
        [...parseRemovedMessageKeys(storage.getItem(key) || "[]")],
        messageRemovalKeys(message, chat),
      ),
    ),
  );
}
export function restoreRemovedMessagesWithStorage(
  workspace: string,
  kind: string,
  chat: string,
  storage: Storage,
) {
  const key = removedMessagesStorageKey(workspace, kind, chat);
  storage.setItem(`${key}:restored`, storage.getItem(`${key}:copies`) || "[]");
  storage.removeItem(key);
}
function completed(value: unknown): void | Promise<void> {
  if (value instanceof Promise) return value.then(() => {});
}
function after(value: void | Promise<void>, next: () => void | Promise<void>) {
  return value instanceof Promise ? value.then(next) : next();
}

/** Only display records are restored. This store never recreates an outgoing message. */
export function createRemovedMessageStore(
  storage: Pick<Storage, "getItem">,
  update: RemovedMessageUpdate,
) {
  const saveRemovedMessage = (
    workspace: string,
    kind: string,
    chat: string,
    message: Message,
  ) => {
    const key = `${removedMessagesStorageKey(workspace, kind, chat)}:copies`;
    const captured: Message = JSON.parse(JSON.stringify(message));
    return completed(
      update<Message[]>(key, [], (copies) => copiesWith(copies, captured)),
    );
  };
  return {
    saveRemovedMessage,
    removeMessageFromDevice(
      workspace: string,
      kind: string,
      chat: string,
      message: Message,
    ) {
      const capturedKeys = messageRemovalKeys(message, chat);
      return after(saveRemovedMessage(workspace, kind, chat, message), () =>
        completed(
          update<string[]>(
            removedMessagesStorageKey(workspace, kind, chat),
            [],
            (keys) => keysWith(keys, capturedKeys),
          ),
        ),
      );
    },
    restore(workspace: string, kind: string, chat: string) {
      const key = removedMessagesStorageKey(workspace, kind, chat);
      const copies: Message[] = JSON.parse(
        storage.getItem(`${key}:copies`) || "[]",
      );
      return after(
        completed(update<Message[]>(`${key}:restored`, [], () => copies)),
        () => completed(update<string[] | null>(key, [], () => null)),
      );
    },
  };
}
