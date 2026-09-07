import { useEffect } from "react";

/** Keep the phone layout inside the visible area without treating pinch zoom as a keyboard. */
export function useMobileViewport(enabled: boolean) {
  useEffect(() => {
    if (!enabled) return;
    const viewport = window.visualViewport;
    const root = document.documentElement;
    let frame = 0;
    const update = () => {
      frame = 0;
      // During zoom the browser owns viewport movement and scaling.
      if (viewport && Math.abs(viewport.scale - 1) > 0.01) return;
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
      const keyboard = layoutHeight - height - top > 100;
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
      if (window.scrollX || window.scrollY) window.scrollTo(0, 0);
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    update();
    viewport?.addEventListener("resize", schedule);
    viewport?.addEventListener("scroll", schedule);
    window.addEventListener("resize", schedule);
    window.addEventListener("orientationchange", schedule);
    window.addEventListener("scroll", schedule);
    return () => {
      cancelAnimationFrame(frame);
      viewport?.removeEventListener("resize", schedule);
      viewport?.removeEventListener("scroll", schedule);
      window.removeEventListener("resize", schedule);
      window.removeEventListener("orientationchange", schedule);
      window.removeEventListener("scroll", schedule);
      for (const property of [
        "--mobile-viewport-height",
        "--mobile-viewport-top",
        "--mobile-safe-area-bottom",
        "--mobile-safe-area-top",
      ])
        root.style.removeProperty(property);
    };
  }, [enabled]);
}
