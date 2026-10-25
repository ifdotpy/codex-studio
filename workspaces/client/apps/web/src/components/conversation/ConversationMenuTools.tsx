import { useSyncExternalStore, type ReactNode } from "react";

/** One live tool element, rendered inside the existing action menu's context.
 * Publishing must not rerender the conversation that supplies its handlers. */
export function createConversationMenuTools() {
  let current: ReactNode = null;
  const listeners = new Set<() => void>();
  return {
    read: () => current,
    publish: (next: ReactNode) => {
      if (current === next) return;
      current = next;
      listeners.forEach((listener) => listener());
    },
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}

export function ConversationMenuTools({
  source,
}: {
  source: ReturnType<typeof createConversationMenuTools>;
}) {
  return useSyncExternalStore(source.subscribe, source.read, source.read);
}
