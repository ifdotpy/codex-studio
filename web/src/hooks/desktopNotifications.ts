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
    permissionReported: false,
    seen: new Set<string>(),
  });
  useEffect(() => {
    if (!data) return;
    const scope = data.stateDir;
    if (state.current.scope !== scope)
      state.current = {
        scope,
        initialized: false,
        permissionReported: false,
        seen: new Set(),
      };
    const current = state.current;
    const alerts = desktopAlerts(data);
    const added = alerts.filter((alert) => !current.seen.has(alert.id));
    for (const alert of alerts) current.seen.add(alert.id);
    // The initial snapshot establishes a baseline, not an old-alert backlog.
    if (!current.initialized) {
      current.initialized = true;
      return;
    }
    const openedShared = data.runtime.rooms.find(
      (room) => room.id === opened && room.radio,
    );
    for (const alert of added) {
      if (
        document.visibilityState === "visible" &&
        document.hasFocus() &&
        (opened === alert.target.agentId ||
          openedShared?.members.includes(alert.target.agentId))
      )
        continue;
      const { id, ...notification } = alert;
      if (window.codexDesktop) {
        void window.codexDesktop.notify(notification).catch((error) => {
          const message =
            error instanceof Error ? error.message : String(error);
          const denied =
            /STUDIO_NOTIFICATIONS_DENIED|Notifications are not allowed for this application/i.test(
              message,
            );
          if (denied && current.permissionReported) return;
          if (denied) current.permissionReported = true;
          else current.seen.delete(id);
          window.dispatchEvent(
            new CustomEvent("desktop-error", {
              detail: denied
                ? "Enable Allow notifications for Codex Studio in macOS System Settings > Notifications."
                : error instanceof Error
                  ? error.message
                  : "Could not show the desktop notification.",
            }),
          );
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
