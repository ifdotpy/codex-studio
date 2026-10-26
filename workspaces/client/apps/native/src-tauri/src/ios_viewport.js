// Native iOS shell viewport receiver. Injected at document start into the
// main frame. The web owner is apps/web/src/native/nativeViewport.ts; keep
// the names below in step with it and with README.md.
(() => {
  if (window.__studioNativeViewport) return;
  const eventName = "studio-native-viewport";
  const sides = ["top", "right", "bottom", "left"];
  const state = { insets: null, keyboard: null };
  const finite = (value) =>
    typeof value === "number" && Number.isFinite(value) && value >= 0;
  const receive = (update) => {
    if (!update || typeof update !== "object") return;
    const { insets, keyboard } = update;
    if (insets && sides.every((side) => finite(insets[side]))) {
      state.insets = Object.freeze({ ...insets });
      for (const side of sides)
        document.documentElement.style.setProperty(
          `--native-safe-${side}`,
          `${insets[side]}px`,
        );
    }
    if (
      keyboard &&
      finite(keyboard.overlap) &&
      finite(keyboard.duration) &&
      Number.isInteger(keyboard.curve)
    )
      state.keyboard = Object.freeze({ ...keyboard });
    window.dispatchEvent(
      new CustomEvent(eventName, { detail: Object.freeze({ ...state }) }),
    );
  };
  Object.defineProperty(window, "__studioNativeViewport", {
    value: Object.freeze({
      version: 1,
      get state() {
        return Object.freeze({ ...state });
      },
      receive,
    }),
  });
  // The shell page never zooms (pinch, double tap, focus auto-zoom). Only
  // this shell injects the limit; Safari and the PWA keep page zoom.
  const lockZoom = () => {
    let meta = document.querySelector('meta[name="viewport"]');
    if (!meta) {
      if (!document.head) return;
      meta = document.createElement("meta");
      meta.name = "viewport";
      document.head.append(meta);
    }
    const kept = (meta.content || "")
      .split(",")
      .map((part) => part.trim())
      .filter(
        (part) =>
          part && !/^(maximum-scale|minimum-scale|user-scalable)\b/.test(part),
      );
    const content = [
      ...kept,
      "minimum-scale=1",
      "maximum-scale=1",
      "user-scalable=no",
    ].join(", ");
    if (meta.content !== content) meta.content = content;
  };
  if (document.readyState === "loading")
    document.addEventListener("DOMContentLoaded", lockZoom, { once: true });
  else lockZoom();
  // The shell answers each request with the current UIKit geometry.
  const request = () =>
    window.webkit?.messageHandlers?.studioNativeViewport?.postMessage("sync");
  request();
  window.addEventListener("resize", request);
  window.addEventListener("orientationchange", request);
})();
