import { hasTouchCapability, touchPointerMedia } from "./touchCapability";
import { useEffect } from "react";
import { layoutValues } from "../layout/layout-contract.generated";

const fullFrames = new WeakMap<Document, { height: number; width: number }>();

/** Keep the full touch layout behind the keyboard while its composer follows the visible bottom. */
export function useMobileViewport(active: boolean, compact = false) {
  useEffect(() => {
    if (!active) return;
    const viewport = window.visualViewport;
    const root = document.documentElement;
    const coarsePointer = touchPointerMedia();
    let frame = 0;
    const previousFrame = fullFrames.get(document);
    let fullHeight = previousFrame?.height || 0;
    let fullWidth = previousFrame?.width || 0;
    let keyboardRecoveryPending = false;
    let composer: HTMLElement | null = null;
    let messages: HTMLElement | null = null;
    const recoveryFrames = new Set<number>();
    const resetComposer = () => {
      composer?.removeAttribute("data-keyboard-floating");
      composer?.style.removeProperty("--keyboard-composer-shift");
      messages?.style.removeProperty("--keyboard-scroll-clearance");
    };
    const clear = () => {
      resetComposer();
      root.removeAttribute("data-viewport-surface");
      root.removeAttribute("data-viewport-keyboard");
      root.removeAttribute("data-viewport-keyboard-compact");
      root.removeAttribute("data-viewport-short-landscape");
      for (const property of [
        "--mobile-viewport-height",
        "--mobile-viewport-top",
        "--mobile-safe-area-bottom",
        "--mobile-safe-area-top",
      ])
        root.style.removeProperty(property);
    };
    const update = () => {
      cancelAnimationFrame(frame);
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
      // iOS can shrink innerHeight and pan the visual viewport at the same time.
      // Its offset remains relative to the layout viewport after that shrink.
      const top = Math.max(0, viewport?.offsetTop ?? 0);
      if (!Number.isFinite(height) || height <= 0 || !Number.isFinite(top))
        return;
      root.setAttribute("data-viewport-surface", "");
      const width = viewport?.width || window.innerWidth;
      const sameWidth = !fullWidth || width === fullWidth;
      const keyboard =
        !!viewport &&
        Math.max(
          layoutHeight - height,
          sameWidth ? fullHeight - height : 0,
          top,
        ) > layoutValues.keyboard.occlusionMin;
      if (!keyboard || !sameWidth) {
        fullHeight = height;
        fullWidth = width;
        fullFrames.set(document, { height: fullHeight, width: fullWidth });
      } else if (!fullHeight) {
        fullHeight = layoutHeight;
        fullWidth = width;
      }
      if (keyboard) keyboardRecoveryPending = true;
      if (keyboard) root.setAttribute("data-viewport-keyboard", "");
      else root.removeAttribute("data-viewport-keyboard");
      const overlay =
        keyboard &&
        hasTouchCapability(coarsePointer.matches, navigator.maxTouchPoints) &&
        width > fullHeight &&
        fullHeight < layoutValues.header.shortLandscapeMaxHeight;
      const surfaceHeight = overlay ? fullHeight : height;
      if (keyboard && height < layoutValues.keyboard.compactMaxHeight)
        root.setAttribute("data-viewport-keyboard-compact", "");
      else root.removeAttribute("data-viewport-keyboard-compact");
      if (
        hasTouchCapability(coarsePointer.matches, navigator.maxTouchPoints) &&
        width > fullHeight &&
        fullHeight < layoutValues.header.shortLandscapeMaxHeight
      )
        root.setAttribute("data-viewport-short-landscape", "");
      else root.removeAttribute("data-viewport-short-landscape");
      root.style.setProperty("--mobile-viewport-height", `${surfaceHeight}px`);
      root.style.setProperty("--mobile-viewport-top", `${top}px`);
      root.style.setProperty(
        "--mobile-safe-area-bottom",
        keyboard && !overlay ? "0px" : "env(safe-area-inset-bottom)",
      );
      root.style.setProperty(
        "--mobile-safe-area-top",
        top > 0
          ? `max(0px, calc(env(safe-area-inset-top) - ${top}px))`
          : "env(safe-area-inset-top)",
      );
      const nextComposer =
        Array.from(document.querySelectorAll<HTMLElement>("#composer")).find(
          (node) => node.getClientRects().length > 0,
        ) || null;
      if (composer !== nextComposer) {
        resetComposer();
        composerResize.disconnect();
        composer = nextComposer;
        messages =
          composer
            ?.closest("#conversation")
            ?.querySelector<HTMLElement>("#messages") || null;
        if (composer) composerResize.observe(composer);
      }
      if (overlay && composer) {
        const bounds = composer.getBoundingClientRect();
        const transform = getComputedStyle(composer).transform;
        const translation =
          transform === "none" ? 0 : new DOMMatrixReadOnly(transform).m42;
        const naturalBottom = bounds.bottom - translation;
        // Both rectangles use the browser's coordinate origin. pageTop may use
        // a different origin while iOS pans its layout viewport.
        const visibleTop =
          document.getElementById("root")?.getBoundingClientRect().top ?? top;
        const composerShift = Math.min(0, visibleTop + height - naturalBottom);
        const retainTarget = (
          node: HTMLElement,
          property: string,
          value: number,
        ) => {
          const previous = parseFloat(node.style.getPropertyValue(property));
          if (
            !Number.isFinite(previous) ||
            Math.abs(previous - value) >
              layoutValues.keyboard.motionTargetTolerance
          )
            node.style.setProperty(property, `${value}px`);
        };
        retainTarget(composer, "--keyboard-composer-shift", composerShift);
        composer.setAttribute("data-keyboard-floating", "");
        if (messages) {
          const clearance = Math.max(
            0,
            messages.getBoundingClientRect().bottom -
              (naturalBottom - bounds.height + composerShift),
          );
          retainTarget(messages, "--keyboard-scroll-clearance", clearance);
        }
      } else resetComposer();
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
    const composerResize = new ResizeObserver(schedule);
    const content = new MutationObserver(() => {
      if (!composer?.isConnected) schedule();
    });
    content.observe(root, { childList: true, subtree: true });
    const recoverAfterBlur = () => {
      if (!keyboardRecoveryPending || recoveryFrames.size) return;
      const next = (remaining: number) => {
        const id = requestAnimationFrame(() => {
          recoveryFrames.delete(id);
          const focused = document.activeElement;
          if (
            focused instanceof HTMLTextAreaElement &&
            focused.matches("#composer textarea")
          )
            return;
          window.scrollTo(0, 0);
          update();
          if (remaining > 0) next(remaining - 1);
          else keyboardRecoveryPending = false;
        });
        recoveryFrames.add(id);
      };
      next(1);
    };
    document.addEventListener("focusout", recoverAfterBlur);
    update();
    coarsePointer.addEventListener("change", schedule);
    viewport?.addEventListener("resize", update);
    viewport?.addEventListener("scroll", update);
    window.addEventListener("resize", update);
    window.addEventListener("orientationchange", update);
    window.addEventListener("scroll", update);
    return () => {
      cancelAnimationFrame(frame);
      recoveryFrames.forEach(cancelAnimationFrame);
      document.removeEventListener("focusout", recoverAfterBlur);
      composerResize.disconnect();
      content.disconnect();
      coarsePointer.removeEventListener("change", schedule);
      viewport?.removeEventListener("resize", update);
      viewport?.removeEventListener("scroll", update);
      window.removeEventListener("resize", update);
      window.removeEventListener("orientationchange", update);
      window.removeEventListener("scroll", update);
      clear();
    };
  }, [active, compact]);
}
