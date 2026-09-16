import { useEffect, useRef } from "react";
import { desktopAlerts } from "../desktopAlerts";
import type { Snapshot } from "../types";

export function useDesktopNotifications(
  data: Snapshot | null,
  opened: string | null,
) {
  const state = useRef({
    scope: "",
    initialized: false,
    seen: new Set<string>(),
  });
  useEffect(() => {
    if (!data) return;
    const scope = data.stateDir;
    if (state.current.scope !== scope)
      state.current = { scope, initialized: false, seen: new Set() };
    const current = state.current;
    const alerts = desktopAlerts(data);
    const added = alerts.filter((alert) => !current.seen.has(alert.id));
    for (const alert of alerts) current.seen.add(alert.id);
    // The initial snapshot establishes a baseline, not an old-alert backlog.
    if (!current.initialized) {
      current.initialized = true;
      return;
    }
    for (const alert of added) {
      if (
        document.visibilityState === "visible" &&
        document.hasFocus() &&
        opened === alert.target.agentId
      )
        continue;
      const { id, ...notification } = alert;
      if (window.codexDesktop) {
        void window.codexDesktop.notify(notification).catch(() => {
          current.seen.delete(id);
        });
      } else if (
        "Notification" in window &&
        Notification.permission === "granted"
      ) {
        const native = new Notification(alert.title, {
          body: alert.body,
          tag: id,
        });
        native.onclick = () => {
          native.close();
          window.focus();
          window.dispatchEvent(
            new CustomEvent("studio-navigate", { detail: alert.target }),
          );
        };
      }
    }
  }, [data, opened]);
}
