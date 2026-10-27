import { useLayoutEffect, useState } from "react";
import { nativeViewport } from "../native/nativeViewport";
import { touchPointerMedia } from "./touchCapability";
import { layoutValues } from "../layout/layout-contract.generated";
import { viewportGestureCountAttribute } from "./mobileViewport";
import { createLayoutFrameBuffer } from "./layoutFrameBuffer";
import "./layout-debug.css";

/** Temporary device diagnostic. The normal application does not mount its probes. */
export function LayoutDebug() {
  const enabled =
    new URLSearchParams(window.location.search).get("layoutDebug") === "1";
  const [values, setValues] = useState("");
  const [frameValues, setFrameValues] = useState("[]");
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
          `intercepted gestures: ${document.documentElement.getAttribute(viewportGestureCountAttribute) || 0}`,
          `frame samples: ${samples}; root moved frames: ${rootMoved}; sidebar moved frames: ${sidebarMoved}`,
          `vv pan events this open: ${panEvents}; keyboard gesture interception: product`,
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
    let captureStart = 0,
      captureUntil = -1,
      capturePhase = "idle",
      capturePending = false;
    // Samples per diagnostic window, in frames: before the pan, after it, and at close.
    const recordWindowFrames = 90;
    const records = createLayoutFrameBuffer(recordWindowFrames);
    const record = (time: number) => {
      const composer = document
        .querySelector("#composer")
        ?.getBoundingClientRect();
      records.push({
        time: Math.round(time - captureStart),
        interceptedGestures: Number(
          document.documentElement.getAttribute(
            viewportGestureCountAttribute,
          ) || 0,
        ),
        phase: capturePhase,
        innerHeight: window.innerHeight,
        height: window.visualViewport?.height,
        offsetTop: window.visualViewport?.offsetTop,
        pageTop: window.visualViewport?.pageTop,
        scrollY: window.scrollY,
        rootTop: root.getBoundingClientRect().top,
        sidebar: sidebarRect(),
        composer: composer ? [composer.top, composer.bottom] : null,
        nativeKeyboard: nativeViewport()?.keyboard?.overlap ?? null,
      });
      capturePending = true;
    };
    const beginCapture = (event: FocusEvent) => {
      if (
        !(event.target instanceof HTMLTextAreaElement) ||
        !event.target.matches("#composer textarea")
      )
        return;
      const now = performance.now();
      if (event.type === "focusin") {
        records.reset();
        captureStart = now;
      }
      capturePhase = event.type === "focusin" ? "focus" : "blur";
      captureUntil =
        event.type === "focusin"
          ? Infinity
          : now + layoutValues.keyboard.focusSampleWindow;
      record(now);
    };
    document.addEventListener("focusin", beginCapture);
    document.addEventListener("focusout", beginCapture);
    // Frames between passive counter refreshes, about half a second at 60 Hz.
    const counterRefreshFrames = 30;
    const sample = (time: number) => {
      if (time <= captureUntil) record(time);
      if (
        capturePending &&
        (time > captureUntil || samples % counterRefreshFrames === 0)
      ) {
        capturePending = false;
        setFrameValues(JSON.stringify(records.snapshot()));
      }
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
      document.removeEventListener("focusin", beginCapture);
      document.removeEventListener("focusout", beginCapture);
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
  }, [enabled]);
  return enabled ? (
    <>
      <pre className="layout-debug" aria-hidden="true">
        {values}
      </pre>
      <textarea
        id="layout-debug-frames"
        className="layout-debug-frames"
        aria-label="Keyboard frame samples"
        readOnly
        tabIndex={-1}
        value={frameValues}
      />
    </>
  ) : null;
}
