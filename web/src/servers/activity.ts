import { useEffect } from "react";
import { serverViewId, serverParentOrigin } from "./environment";
const active = new Set<symbol>();
function publish() {
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
        busy: active.size > 0,
      },
      serverParentOrigin,
    );
}
export function beginServerActivity() {
  const token = Symbol();
  active.add(token);
  publish();
  return () => {
    active.delete(token);
    publish();
  };
}
export function useServerActivity(busy: boolean) {
  useEffect(() => {
    if (busy) return beginServerActivity();
    publish();
  }, [busy]);
}
