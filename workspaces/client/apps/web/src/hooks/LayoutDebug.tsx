import { useLayoutEffect, useState } from "react";
import { touchPointerMedia } from "./touchCapability";
import "./layout-debug.css";

/** Temporary device diagnostic. The normal application does not mount its probes. */
export function LayoutDebug() {
  const enabled =
    new URLSearchParams(window.location.search).get("layoutDebug") === "1";
  const [values, setValues] = useState("");
  useLayoutEffect(() => {
    if (!enabled) return;
    const root = document.getElementById("root");
    if (!root) return;
    const primary = window.matchMedia("(pointer: coarse)");
    const any = touchPointerMedia();
    const standalone = window.matchMedia("(display-mode: standalone)");
    const probe = document.createElement("div");
    probe.className = "layout-debug-probe";
    probe.setAttribute("aria-hidden", "true");
    document.body.append(probe);
    const update = () => {
      const style = getComputedStyle(root);
      const safe = getComputedStyle(probe);
      const padding = (s: CSSStyleDeclaration) =>
        [s.paddingTop, s.paddingRight, s.paddingBottom, s.paddingLeft].join(
          " / ",
        );
      setValues(
        [
          `pointer coarse: ${primary.matches}`,
          `any-pointer coarse: ${any.matches}`,
          `maxTouchPoints: ${navigator.maxTouchPoints}`,
          `innerWidth: ${window.innerWidth}px`,
          `visualViewport.width: ${window.visualViewport ? `${window.visualViewport.width}px` : "unavailable"}`,
          `display-mode: ${standalone.matches || (navigator as Navigator & { standalone?: boolean }).standalone ? "standalone" : "browser"}`,
          `root height: ${style.height}`,
          `root padding T/R/B/L: ${padding(style)}`,
          `safe-area T/R/B/L: ${padding(safe)}`,
          `data-viewport-surface: ${document.documentElement.hasAttribute("data-viewport-surface")}`,
        ].join("\n"),
      );
    };
    const resize = new ResizeObserver(update);
    resize.observe(root);
    const mutation = new MutationObserver(update);
    mutation.observe(document.documentElement, { attributes: true });
    mutation.observe(root, { attributes: true });
    primary.addEventListener("change", update);
    any.addEventListener("change", update);
    standalone.addEventListener("change", update);
    window.addEventListener("resize", update);
    window.visualViewport?.addEventListener("resize", update);
    update();
    return () => {
      resize.disconnect();
      mutation.disconnect();
      probe.remove();
      primary.removeEventListener("change", update);
      any.removeEventListener("change", update);
      standalone.removeEventListener("change", update);
      window.removeEventListener("resize", update);
      window.visualViewport?.removeEventListener("resize", update);
    };
  }, [enabled]);
  return enabled ? (
    <pre className="layout-debug" aria-hidden="true">
      {values}
    </pre>
  ) : null;
}
