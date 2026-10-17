import { useEffect } from "react";
import { serverViewId, serverParentOrigin } from "./environment";
export function createServerActivity(changed?: (busy: boolean) => void) {
  const active = new Set<symbol>();
  const listeners = new Set<() => void>();
  const serverActivityActive = () => active.size > 0;
  function subscribeServerActivity(listener: () => void) {
    listeners.add(listener);
    return () => listeners.delete(listener);
  }
  function publish() {
    for (const listener of listeners) listener();
    changed?.(active.size > 0);
  }
  function beginServerActivity() {
    const token = Symbol();
    active.add(token);
    publish();
    return () => {
      active.delete(token);
      publish();
    };
  }
  function useServerActivity(busy: boolean) {
    useEffect(() => {
      if (busy) return beginServerActivity();
      publish();
    }, [busy]);
  }
  return {
    serverActivityActive,
    subscribeServerActivity,
    beginServerActivity,
    useServerActivity,
  };
}

export const {
  serverActivityActive,
  subscribeServerActivity,
  beginServerActivity,
  useServerActivity,
} = createServerActivity((busy) => {
  if (
    serverViewId &&
    typeof window !== "undefined" &&
    window.parent &&
    window.parent !== window
  )
    window.parent.postMessage(
      {
        kind: "studio-server-activity",
        serverId: serverViewId,
        busy: busy,
      },
      serverParentOrigin,
    );
});
