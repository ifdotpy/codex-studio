// Native iOS shell viewport receiver. Injected at document start into the
// main frame. The web owner is apps/web/src/native/nativeViewport.ts; keep
// the names below in step with it and with README.md.
(() => {
  if (window.__studioNativeViewport) return;
  const eventName = "studio-native-viewport";
  const sides = ["top", "right", "bottom", "left"];
  const state = {
    insets: null,
    keyboard: null,
    hinge: null,
    statusColumn: null,
    hingeBand: null,
  };
  const rectKeys = ["x", "y", "width", "height"];
  const isRect = (value) =>
    value && rectKeys.every((key) => finite(value[key]));
  const hingeStates = ["closed", "partial", "open", "unknown"];
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
    const { hinge } = update;
    if (hinge && hingeStates.includes(hinge.status)) {
      // UIHingeInteraction (iOS 27.1). UIHinge.angle is in radians (the
      // device reports pi when fully open); null when UIKit reports no hinge.
      const angle = Number.isFinite(hinge.angle) ? hinge.angle : null;
      const degrees = angle === null ? null : (angle * 180) / Math.PI;
      state.hinge = Object.freeze({ status: hinge.status, angle, degrees });
      const root = document.documentElement;
      root.setAttribute("data-native-hinge", hinge.status);
      if (degrees === null) root.style.removeProperty("--native-hinge-angle");
      else root.style.setProperty("--native-hinge-angle", `${degrees}deg`);
    }
    if ("statusColumn" in update) {
      // iOS 27.1 reserved occlusion regions in the strip: the clock, Wi-Fi
      // and any call or recording pill. Null when UIKit reports none.
      const column = update.statusColumn;
      const root = document.documentElement;
      if (isRect(column)) {
        state.statusColumn = Object.freeze({ ...column });
        root.style.setProperty(
          "--native-status-column-bottom",
          `${column.y + column.height}px`,
        );
        root.style.setProperty("--native-status-column-left", `${column.x}px`);
      } else {
        state.statusColumn = null;
        root.style.removeProperty("--native-status-column-bottom");
        root.style.removeProperty("--native-status-column-left");
      }
    }
    if ("hingeBand" in update) {
      // iOS 27.1 division region: the band that content should not cross
      // (the hinge), margins included. Null when UIKit reports none.
      const band = update.hingeBand;
      const root = document.documentElement;
      const names = ["left", "top", "width", "height"];
      if (isRect(band)) {
        state.hingeBand = Object.freeze({
          ...band,
          active: band.active === true,
        });
        [band.x, band.y, band.width, band.height].forEach((value, index) =>
          root.style.setProperty(
            `--native-hinge-${names[index]}`,
            `${value}px`,
          ),
        );
      } else {
        state.hingeBand = null;
        for (const name of names)
          root.style.removeProperty(`--native-hinge-${name}`);
      }
    }
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
