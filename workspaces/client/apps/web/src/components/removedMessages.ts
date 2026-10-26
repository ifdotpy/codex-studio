import {
  useServerStorage,
  useServerStorageEventKey,
} from "../servers/ServerServicesContext";
import { serverLocalStorage as localStorage } from "../servers/storage";
import { useMemo } from "react";
import type { Message } from "../types";
import {
  isRemovedMessage as readRemoved,
  removedMessagesStorageKey,
  removeMessageFromDeviceWithStorage,
  saveRemovedMessageWithStorage,
  restoreRemovedMessagesWithStorage,
} from "./removedMessageStorage";
import { useRemovedMessagesView } from "./removedMessagesView";
export {
  messageRemovalKeys,
  removedMessagesStorageKey,
} from "./removedMessageStorage";
const changed = "studio-removed-messages-changed";
const announce = (key: string) =>
  window.dispatchEvent(new CustomEvent(changed, { detail: key }));
export function isRemovedMessage(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
  storage = localStorage,
) {
  return readRemoved(workspace, kind, chat, message, storage);
}
export function saveRemovedMessage(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
  storage = localStorage,
) {
  saveRemovedMessageWithStorage(workspace, kind, chat, message, storage);
}
export function removeMessageFromDevice(
  workspace: string,
  kind: string,
  chat: string,
  message: Message,
  storage = localStorage,
) {
  removeMessageFromDeviceWithStorage(workspace, kind, chat, message, storage);
  announce(removedMessagesStorageKey(workspace, kind, chat));
}
export function useRemovedMessages(
  workspace: string,
  kind: string,
  chat: string | null,
) {
  const storage = useServerStorage();
  const eventKey = useServerStorageEventKey();
  const changes = useMemo(
    () => ({
      subscribe(notify: (key: string | null) => void) {
        const remote = (event: StorageEvent) => {
          const key = eventKey(event);
          if (key !== undefined) notify(key);
        };
        const local = (event: Event) =>
          notify((event as CustomEvent<string>).detail);
        window.addEventListener("storage", remote);
        window.addEventListener(changed, local);
        return () => {
          window.removeEventListener("storage", remote);
          window.removeEventListener(changed, local);
        };
      },
    }),
    [eventKey],
  );
  const view = useRemovedMessagesView(workspace, kind, chat, storage, changes);
  return {
    ...view,
    restore: () => {
      restoreRemovedMessagesWithStorage(workspace, kind, chat || "", storage);
      announce(removedMessagesStorageKey(workspace, kind, chat));
    },
    remove: (message: Message) =>
      removeMessageFromDevice(workspace, kind, chat || "", message, storage),
  };
}
