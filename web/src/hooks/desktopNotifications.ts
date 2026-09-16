import { useEffect, useRef, useState } from "react";
import { saved } from "../api";
import { desktopAlerts } from "../desktopAlerts";
import type { Snapshot } from "../types";

export function useDesktopNotifications(
  data: Snapshot | null,
  opened: string | null,
) {
  const [enabled, setEnabled] = useState(false);
  const state = useRef({
    scope: "",
    initialized: false,
    seen: new Set<string>(),
  });
  useEffect(() => {
    let active = true;
    let changed = false;
    const change = (event: Event) => {
      changed = true;
      setEnabled((event as CustomEvent<boolean>).detail);
    };
    window.addEventListener("studio-notifications", change);
    const load = window.codexDesktop
      ? window.codexDesktop.getNotifications()
      : Promise.resolve(saved("workspace-notifications", false));
    void load
      .then((value) => {
        if (active && !changed) setEnabled(value);
      })
      .catch(() => {});
    return () => {
      active = false;
      window.removeEventListener("studio-notifications", change);
    };
  }, []);
  useEffect(() => {
    if (!data) return;
    const scope = data.stateDir;
    if (state.current.scope !== scope)
      state.current = { scope, initialized: false, seen: new Set() };
    const current = state.current;
    const alerts = desktopAlerts(data);
    const added = alerts.filter((alert) => !current.seen.has(alert.id));
    for (const alert of alerts) current.seen.add(alert.id);
    // The initial snapshot and opt-in establish a baseline, not an old-alert backlog.
    if (!enabled || !current.initialized) {
      current.initialized = enabled;
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
  }, [data, opened, enabled]);
}
