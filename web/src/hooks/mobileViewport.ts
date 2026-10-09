import { useEffect } from "react";

const REQUESTS_VISIBLE_HEIGHT_RATIO = 0.5;

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
      const conversation = document.querySelector<HTMLElement>("#conversation");
      const header = document.querySelector<HTMLElement>(".workspace-header");
      const composer = conversation?.querySelector<HTMLElement>("#composer");
      const input = composer?.querySelector<HTMLTextAreaElement>("#message");
      const footer = conversation?.querySelector<HTMLElement>(".usage-footer");
      const measuredRequestsLimit = () => {
        if (!conversation || !header || !composer || !input || !footer)
          return height * REQUESTS_VISIBLE_HEIGHT_RATIO;
        const inputStyle = getComputedStyle(input);
        const pixels = (value: string) => Number.parseFloat(value) || 0;
        const lineHeight =
          pixels(inputStyle.lineHeight) || pixels(inputStyle.fontSize) * 1.2;
        const minimumInputHeight = Math.max(
          pixels(inputStyle.minHeight),
          lineHeight * input.rows +
            pixels(inputStyle.paddingTop) +
            pixels(inputStyle.paddingBottom) +
            pixels(inputStyle.borderTopWidth) +
            pixels(inputStyle.borderBottomWidth),
        );
        const composerChrome =
          composer.getBoundingClientRect().height -
          input.getBoundingClientRect().height;
        const minimumComposerHeight = composerChrome + minimumInputHeight;
        const margins = [composer, footer].reduce((total, element) => {
          const style = getComputedStyle(element);
          return (
            total +
            (Number.parseFloat(style.marginTop) || 0) +
            (Number.parseFloat(style.marginBottom) || 0)
          );
        }, 0);
        const conversationBounds = conversation.getBoundingClientRect();
        const headerBounds = header.getBoundingClientRect();
        // Derive the visible conversation area directly because the root's
        // mobile height custom property is updated later in this frame.
        const conversationHeight = Math.max(
          0,
          top + height - Math.max(conversationBounds.top, headerBounds.bottom),
        );
        const remaining =
          conversationHeight -
          minimumComposerHeight -
          footer.getBoundingClientRect().height -
          margins -
          pixels(getComputedStyle(conversation).paddingTop) -
          pixels(getComputedStyle(conversation).paddingBottom);
        return Math.max(
          0,
          Math.min(height * REQUESTS_VISIBLE_HEIGHT_RATIO, remaining),
        );
      };
      const requestsLimit = `${measuredRequestsLimit()}px`;
      if (
        root.style.getPropertyValue("--requests-visible-max-height") !==
        requestsLimit
      )
        root.style.setProperty("--requests-visible-max-height", requestsLimit);
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
        "--requests-visible-max-height",
      ])
        root.style.removeProperty(property);
    };
  }, [enabled]);
}
