import type { DesktopBridge } from "../desktop";
import { hasNativeClient, listenHostEvent, nativeRequest } from "./hostClient";

type NavigationCallback = Parameters<DesktopBridge["onNavigate"]>[0];
type NavigationTarget = Parameters<NavigationCallback>[0];
// ui_events.rs owns the payload contract; the focused test checks its parity.
const NAVIGATION_FIELDS = ["agentId", "section", "serverId", "itemId"] as const;
function navigationTarget(input: unknown): NavigationTarget | undefined {
  if (!input || typeof input !== "object" || Array.isArray(input)) return;
  const value = input as Record<string, unknown>;
  const optionalText = (key: "serverId" | "itemId") =>
    !Object.hasOwn(value, key) ||
    (typeof value[key] === "string" && value[key] !== "");
  if (
    !Object.keys(value).every((key) =>
      NAVIGATION_FIELDS.includes(key as (typeof NAVIGATION_FIELDS)[number]),
    ) ||
    !Object.hasOwn(value, "agentId") ||
    typeof value.agentId !== "string" ||
    !value.agentId ||
    !Object.hasOwn(value, "section") ||
    value.section !== "messages" ||
    !optionalText("serverId") ||
    !optionalText("itemId")
  )
    return;
  const target = value as NavigationTarget;
  return {
    agentId: target.agentId,
    section: target.section,
    ...(Object.hasOwn(value, "serverId") ? { serverId: target.serverId } : {}),
    ...(Object.hasOwn(value, "itemId") ? { itemId: target.itemId } : {}),
  };
}
export function hasDesktopNotifications() {
  return hasNativeClient();
}
export function notifyDesktop(value: Parameters<DesktopBridge["notify"]>[0]) {
  return nativeRequest<boolean>("desktop.notify", value);
}
export function onDesktopNavigate(callback: NavigationCallback) {
  let disposed = false;
  let unsubscribe: (() => void) | undefined;
  const stop = () => {
    if (disposed) return;
    disposed = true;
    const current = unsubscribe;
    unsubscribe = undefined;
    current?.();
  };
  const failed = (error: unknown) => {
    if (!disposed)
      window.dispatchEvent(
        new CustomEvent("desktop-error", { detail: String(error) }),
      );
  };
  try {
    void listenHostEvent("studio-native-navigate", (payload) => {
      if (disposed) return;
      const target = navigationTarget(payload);
      if (target) callback(target);
    })
      .then((release) => {
        if (disposed) release();
        else unsubscribe = release;
      })
      .catch(failed);
  } catch (error) {
    failed(error);
  }
  return stop;
}
