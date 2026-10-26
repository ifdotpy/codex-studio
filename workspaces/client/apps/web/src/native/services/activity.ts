import { useEffect } from "react";

/** This projection describes UI waits, not native process or network activity. */
export function createNativeActivity() {
  const active = new Set<symbol>();
  const listeners = new Set<() => void>();
  const publish = () => {
    for (const listener of listeners) listener();
  };
  const beginServerActivity = () => {
    const token = Symbol();
    active.add(token);
    publish();
    return () => {
      if (active.delete(token)) publish();
    };
  };
  return {
    beginServerActivity,
    serverActivityActive: () => active.size > 0,
    subscribeServerActivity(listener: () => void) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    useServerActivity(busy: boolean) {
      useEffect(() => (busy ? beginServerActivity() : undefined), [busy]);
    },
  };
}
