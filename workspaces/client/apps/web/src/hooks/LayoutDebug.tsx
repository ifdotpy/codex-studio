import { useLayoutEffect, useState } from "react";
import { touchPointerMedia } from "./touchCapability";
import "./layout-debug.css";

/** Temporary device diagnostic. The normal application does not mount its probes. */
export function LayoutDebug() {
  const enabled =
    new URLSearchParams(window.location.search).get("layoutDebug") === "1";
  const noPan =
    new URLSearchParams(window.location.search).get("kbdNoPan") === "1";
  const [values, setValues] = useState("");
  useLayoutEffect(() => {
    if (!noPan) return;
    let intercepted: HTMLTextAreaElement | null = null;
    const focusWithoutPan = (event: Event) => {
      const target = event.target;
      if (
        !(target instanceof HTMLTextAreaElement) ||
        !target.matches("#composer textarea")
      )
        return;
      if (document.activeElement === target && intercepted !== target) return;
      event.preventDefault();
      if (intercepted !== target) {
        intercepted = target;
        target.focus({ preventScroll: true });
      }
    };
    const end = () => {
      intercepted = null;
    };
    document.addEventListener("pointerdown", focusWithoutPan, {
      capture: true,
      passive: false,
    });
    document.addEventListener("touchstart", focusWithoutPan, {
      capture: true,
      passive: false,
    });
    for (const type of [
      "pointerup",
      "pointercancel",
      "touchend",
      "touchcancel",
    ])
      document.addEventListener(type, end, true);
    return () => {
      document.removeEventListener("pointerdown", focusWithoutPan, true);
      document.removeEventListener("touchstart", focusWithoutPan, true);
      for (const type of [
        "pointerup",
        "pointercancel",
        "touchend",
        "touchcancel",
      ])
        document.removeEventListener(type, end, true);
    };
  }, [noPan]);
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
    let frame = 0,
      samples = 0,
      rootMoved = 0,
      sidebarMoved = 0,
      panEvents = 0,
      pendingPanEvents = 0;
    let keyboard = false;
    let rootBaseline: number | null = null;
    let sidebarBaseline: number[] | null = null;
    let lastMoved = "none";
    const sidebarRect = () => {
      const r = document.querySelector("#sidebar")?.getBoundingClientRect();
      return r ? [r.x, r.y, r.width, r.height].map(Math.round) : null;
    };
    const update = () => {
      const nextKeyboard = document.documentElement.hasAttribute(
        "data-viewport-keyboard",
      );
      if (nextKeyboard && !keyboard) {
        samples = 0;
        rootMoved = 0;
        sidebarMoved = 0;
        panEvents = pendingPanEvents;
        pendingPanEvents = 0;
        lastMoved = "none";
      }
      keyboard = nextKeyboard;
      const style = getComputedStyle(root);
      const safe = getComputedStyle(probe);
      const composer =
        document.querySelector<HTMLElement>(
          "#composer[data-keyboard-floating]",
        ) || document.querySelector<HTMLElement>("#composer");
      const composerBounds = composer?.getBoundingClientRect();
      const padding = (s: CSSStyleDeclaration) =>
        [s.paddingTop, s.paddingRight, s.paddingBottom, s.paddingLeft].join(
          " / ",
        );
      setValues(
        [
          `pointer coarse: ${primary.matches}`,
          `frame samples: ${samples}; root moved frames: ${rootMoved}; sidebar moved frames: ${sidebarMoved}`,
          `vv pan events this open: ${panEvents}; kbdNoPan experiment: ${noPan ? "on" : "off"}`,
          `last moved frame: ${lastMoved}`,
          `any-pointer coarse: ${any.matches}`,
          `maxTouchPoints: ${navigator.maxTouchPoints}`,
          `innerWidth: ${window.innerWidth}px`,
          `innerHeight: ${window.innerHeight}px`,
          `visualViewport: ${window.visualViewport ? `h ${window.visualViewport.height}, offTop ${window.visualViewport.offsetTop}, pageTop ${window.visualViewport.pageTop}, scrollY ${window.scrollY}` : "unavailable"}`,
          `visualViewport.width: ${window.visualViewport ? `${window.visualViewport.width}px` : "unavailable"}`,
          `display-mode: ${standalone.matches || (navigator as Navigator & { standalone?: boolean }).standalone ? "standalone" : "browser"}`,
          `root height: ${style.height}`,
          `root rect top: ${root.getBoundingClientRect().top}px`,
          `composer rect top/bottom: ${composerBounds ? `${composerBounds.top}px / ${composerBounds.bottom}px` : "absent"}`,
          `composer shift: ${composer?.style.getPropertyValue("--keyboard-composer-shift") || "none"}`,
          `root padding T/R/B/L: ${padding(style)}`,
          `safe-area T/R/B/L: ${padding(safe)}`,
          `data-viewport-surface: ${document.documentElement.hasAttribute("data-viewport-surface")}`,
        ].join("\n"),
      );
    };
    // Frames between passive counter refreshes, about half a second at 60 Hz.
    const counterRefreshFrames = 30;
    const sample = () => {
      const top = Math.round(root.getBoundingClientRect().top);
      const side = sidebarRect();
      if (rootBaseline === null && !keyboard) rootBaseline = top;
      if (sidebarBaseline === null && !keyboard && side) sidebarBaseline = side;
      samples++;
      const rootChanged = rootBaseline !== null && top !== rootBaseline;
      const baseline = sidebarBaseline;
      const sidebarChanged =
        side && baseline && side.some((v, i) => v !== baseline[i]);
      if (rootChanged) rootMoved++;
      if (sidebarChanged) sidebarMoved++;
      if (rootChanged || sidebarChanged)
        lastMoved = `offTop ${window.visualViewport?.offsetTop}, pageTop ${window.visualViewport?.pageTop}, scrollY ${window.scrollY}, root.top ${top}, sidebar ${side?.join("/")}`;
      if (rootChanged || sidebarChanged || samples % counterRefreshFrames === 0)
        update();
      frame = requestAnimationFrame(sample);
    };
    const pan = () => {
      if ((window.visualViewport?.offsetTop || 0) > 0) {
        if (keyboard) panEvents++;
        else pendingPanEvents++;
      }
      update();
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
    window.addEventListener("scroll", update);
    window.visualViewport?.addEventListener("resize", update);
    window.visualViewport?.addEventListener("scroll", pan);
    update();
    frame = requestAnimationFrame(sample);
    return () => {
      cancelAnimationFrame(frame);
      resize.disconnect();
      mutation.disconnect();
      probe.remove();
      primary.removeEventListener("change", update);
      any.removeEventListener("change", update);
      standalone.removeEventListener("change", update);
      window.removeEventListener("resize", update);
      window.removeEventListener("scroll", update);
      window.visualViewport?.removeEventListener("resize", update);
      window.visualViewport?.removeEventListener("scroll", pan);
    };
  }, [enabled, noPan]);
  return enabled ? (
    <pre className="layout-debug" aria-hidden="true">
      {values}
    </pre>
  ) : null;
}
