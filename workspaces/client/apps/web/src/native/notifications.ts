import type { DesktopBridge } from "../desktop";
import { hasNativeClient, nativeHost, nativeRequest } from "./client";
import { isNativeFrame } from "./frame";
export function hasDesktopNotifications() {
  return !!window.codexDesktop || (hasNativeClient() && !isNativeFrame());
}
export function notifyDesktop(value: Parameters<DesktopBridge["notify"]>[0]) {
  return window.codexDesktop
    ? window.codexDesktop.notify(value)
    : nativeRequest<boolean>("desktop.notify", value);
}
export function onDesktopNavigate(
  callback: Parameters<DesktopBridge["onNavigate"]>[0],
) {
  if (window.codexDesktop) return window.codexDesktop.onNavigate(callback);
  let disposed = false;
  let unsubscribe: (() => void) | undefined;
  const listening = nativeHost()?.event?.listen(
    "studio-native-navigate",
    (event) => {
      if (!disposed) callback(event.payload as Parameters<typeof callback>[0]);
    },
  );
  void listening
    ?.then((stop) => {
      if (disposed) stop();
      else unsubscribe = stop;
    })
    .catch((error) =>
      window.dispatchEvent(
        new CustomEvent("desktop-error", { detail: String(error) }),
      ),
    );
  return () => {
    disposed = true;
    unsubscribe?.();
  };
}
