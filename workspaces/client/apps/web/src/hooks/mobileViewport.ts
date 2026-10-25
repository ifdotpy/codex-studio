import { hasTouchCapability, touchPointerQuery } from "./touchCapability";
import { useEffect } from "react";
import { layoutValues } from "../layout/layout-contract.generated";

/** Keep compact and touch layouts inside the visible area without resizing for pinch zoom. */
export function useMobileViewport(active: boolean, compact = false) {
  useEffect(() => {
    if (!active) return;
    const viewport = window.visualViewport;
    const root = document.documentElement;
    const coarsePointer = window.matchMedia(touchPointerQuery);
    let frame = 0;
    const clear = () => {
      root.removeAttribute("data-viewport-surface");
      for (const property of [
        "--mobile-viewport-height",
        "--mobile-viewport-top",
        "--mobile-safe-area-bottom",
        "--mobile-safe-area-top",
      ])
        root.style.removeProperty(property);
    };
    const update = () => {
      frame = 0;
      if (
        !compact &&
        !hasTouchCapability(coarsePointer.matches, navigator.maxTouchPoints)
      ) {
        clear();
        return;
      }
      // During zoom the browser owns viewport movement and scaling.
      if (
        viewport &&
        Math.abs(viewport.scale - 1) > layoutValues.viewport.zoomTolerance
      )
        return;
      const layoutHeight = Math.max(root.clientHeight, window.innerHeight);
      const height = Math.min(
        viewport?.height || window.innerHeight,
        layoutHeight,
      );
      const top = Math.min(
        Math.max(0, viewport?.offsetTop || 0),
        Math.max(0, layoutHeight - height),
      );
      if (!Number.isFinite(height) || height <= 0 || !Number.isFinite(top))
        return;
      root.setAttribute("data-viewport-surface", "");
      const keyboard =
        layoutHeight - height - top > layoutValues.keyboard.occlusionMin;
      root.style.setProperty("--mobile-viewport-height", `${height}px`);
      root.style.setProperty("--mobile-viewport-top", `${top}px`);
      root.style.setProperty(
        "--mobile-safe-area-bottom",
        keyboard ? "0px" : "env(safe-area-inset-bottom)",
      );
      root.style.setProperty(
        "--mobile-safe-area-top",
        top > 0
          ? `max(0px, calc(env(safe-area-inset-top) - ${top}px))`
          : "env(safe-area-inset-top)",
      );
      // Safari can retain a document scroll offset after it reveals a focused input.
      const activeElement = document.activeElement;
      const composerFocused =
        activeElement instanceof HTMLTextAreaElement &&
        activeElement.matches("#composer textarea");
      // Keep native keyboard/picker interactions in the browser's control
      // while the composer is focused, on every platform.
      if ((window.scrollX || window.scrollY) && !composerFocused)
        window.scrollTo(0, 0);
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    update();
    coarsePointer.addEventListener("change", schedule);
    viewport?.addEventListener("resize", schedule);
    viewport?.addEventListener("scroll", schedule);
    window.addEventListener("resize", schedule);
    window.addEventListener("orientationchange", schedule);
    window.addEventListener("scroll", schedule);
    return () => {
      cancelAnimationFrame(frame);
      coarsePointer.removeEventListener("change", schedule);
      viewport?.removeEventListener("resize", schedule);
      viewport?.removeEventListener("scroll", schedule);
      window.removeEventListener("resize", schedule);
      window.removeEventListener("orientationchange", schedule);
      window.removeEventListener("scroll", schedule);
      clear();
    };
  }, [active, compact]);
}
