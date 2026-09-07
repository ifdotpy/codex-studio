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

export function useRemovedMessages(
  workspace: string,
  kind: string,
  chat: string | null,
) {
  const storageKey = `studio-removed-messages:${JSON.stringify([workspace, kind, chat])}`;
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
  return {
    hidden: (message: Message) =>
      messageRemovalKeys(message, chat || "").some((key) => keys.has(key)),
    remove: (message: Message) => {
      const next = parseKeys(snapshot());
      for (const key of messageRemovalKeys(message, chat || "")) next.add(key);
      // Report a storage failure instead of hiding a card that reappears on reload.
      localStorage.setItem(storageKey, JSON.stringify([...next]));
      window.dispatchEvent(new CustomEvent(changed, { detail: storageKey }));
    },
  };
}
