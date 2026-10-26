import { hasTouchCapability, touchPointerMedia } from "./touchCapability";
import { useEffect } from "react";
import { layoutValues } from "../layout/layout-contract.generated";
import {
  nativeKeyboardDuration,
  nativeKeyboardEasing,
  nativeKeyboardMotion,
  nativeViewport,
  nativeViewportEvent,
  type NativeViewportState,
} from "../native/nativeViewport";

/** Count of touch gestures intercepted by the active viewport owner, in gestures. */
export const viewportGestureCountAttribute = "data-keyboard-gesture-count";

const fullFrames = new WeakMap<Document, { height: number; width: number }>();

/** Keep the full touch layout behind the keyboard while its composer follows the visible bottom. */
export function useMobileViewport(active: boolean, compact = false) {
  useEffect(() => {
    if (!active) return;
    const viewport = window.visualViewport;
    const root = document.documentElement;
    const coarsePointer = touchPointerMedia();
    let frame = 0;
    let focusFrame = 0;
    let focusDeadline = 0;
    let hadComposerFocus = false;
    let composerClosing = false;
    let sampledGeometry = "";
    let interceptedTextarea: HTMLTextAreaElement | null = null;
    let gestureCount = 0;
    const previousFrame = fullFrames.get(document);
    let fullHeight = previousFrame?.height || 0;
    let fullWidth = previousFrame?.width || 0;
    let keyboardRecoveryPending = false;
    // The native iOS shell reports the keyboard before UIKit animates it and
    // keeps WebKit's own keyboard viewport changes off.
    let nativeKeyboard = nativeViewport()?.keyboard ?? null;
    let composer: HTMLElement | null = null;
    let messages: HTMLElement | null = null;
    const recoveryFrames = new Set<number>();
    // In the native shell the composer moves by a Web Animation, not a CSS
    // transition. WebKit stops rendering for 80 to 260 ms when the keyboard
    // opens or closes, and the first paint after it is slow. A transition, or
    // an animation that starts before that paint is committed, counts the
    // lost time and its first presented frame jumps. The animation holds its
    // start position and starts in the second rendered frame, after the
    // first one has been committed.
    let composerMotion: Animation | null = null;
    let composerMotionFrame = 0;
    const moveComposer = (node: HTMLElement, from: number, to: number) => {
      if (
        !nativeKeyboard ||
        typeof node.animate !== "function" ||
        Math.abs(from - to) <= layoutValues.keyboard.motionTargetTolerance
      )
        return;
      let start = from;
      if (composerMotion && composerMotion.playState !== "finished") {
        // Continue from the current position when the target changes mid-motion.
        const current = /^matrix\((?:[^,]+,){5}\s*([^)]+)\)$/.exec(
          getComputedStyle(node).transform,
        );
        if (current) start = parseFloat(current[1]);
        composerMotion.cancel();
      }
      const motion = node.animate(
        [
          { transform: `translateY(${start}px)` },
          { transform: `translateY(${to}px)` },
        ],
        {
          duration: nativeKeyboardDuration(nativeKeyboard),
          easing: nativeKeyboardEasing(nativeKeyboard.curve),
        },
      );
      motion.pause();
      composerMotion = motion;
      cancelAnimationFrame(composerMotionFrame);
      composerMotionFrame = requestAnimationFrame(() => {
        composerMotionFrame = requestAnimationFrame(() => {
          composerMotionFrame = 0;
          if (composerMotion === motion) motion.play();
        });
      });
    };
    const resetComposer = () => {
      const shift = parseFloat(
        composer?.style.getPropertyValue("--keyboard-composer-shift") ?? "",
      );
      composer?.removeAttribute("data-keyboard-floating");
      composer?.style.removeProperty("--keyboard-composer-shift");
      messages?.style.removeProperty("--keyboard-scroll-clearance");
      if (composer && Number.isFinite(shift)) moveComposer(composer, shift, 0);
      composer?.getBoundingClientRect();
      messages?.getBoundingClientRect();
    };
    const clear = () => {
      resetComposer();
      cancelAnimationFrame(composerMotionFrame);
      composerMotion?.cancel();
      composerMotion = null;
      root.removeAttribute("data-viewport-native-keyboard");
      root.removeAttribute(viewportGestureCountAttribute);
      root.removeAttribute("data-viewport-surface");
      root.removeAttribute("data-viewport-keyboard");
      root.removeAttribute("data-viewport-keyboard-compact");
      root.removeAttribute("data-viewport-short-landscape");
      for (const property of [
        "--mobile-viewport-height",
        "--mobile-viewport-top",
        "--mobile-safe-area-bottom",
        "--mobile-safe-area-top",
        "--keyboard-motion",
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
      const native = nativeViewport() !== undefined;
      const nativeOverlap = Math.max(0, nativeKeyboard?.overlap ?? 0);
      const height = native
        ? layoutHeight - Math.min(nativeOverlap, layoutHeight)
        : Math.min(viewport?.height || window.innerHeight, layoutHeight);
      // iOS can shrink innerHeight and pan the visual viewport at the same time.
      // Its offset remains relative to the layout viewport after that shrink.
      const top = native ? 0 : Math.max(0, viewport?.offsetTop ?? 0);
      if (!Number.isFinite(height) || height <= 0 || !Number.isFinite(top))
        return;
      root.setAttribute("data-viewport-surface", "");
      if (native) root.setAttribute("data-viewport-native-keyboard", "");
      else root.removeAttribute("data-viewport-native-keyboard");
      const width = viewport?.width || window.innerWidth;
      const sameWidth = !fullWidth || width === fullWidth;
      const keyboard = native
        ? nativeOverlap > layoutValues.keyboard.occlusionMin
        : !!viewport &&
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
        keyboard && !overlay ? "0px" : "var(--viewport-device-safe-bottom)",
      );
      root.style.setProperty(
        "--mobile-safe-area-top",
        top > 0
          ? `max(0px, calc(var(--viewport-device-safe-top) - ${top}px))`
          : "var(--viewport-device-safe-top)",
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
      if (overlay && composer && !composerClosing) {
        const appRoot = document.getElementById("root");
        // Both rectangles use the browser's coordinate origin. pageTop may use
        // a different origin while iOS pans its layout viewport.
        const visibleTop = appRoot?.getBoundingClientRect().top ?? top;
        // Layout offsets exclude the compositor's in-flight transform. Reading
        // a presentation rectangle and a computed transform can mix two frames.
        let naturalBottom = visibleTop + composer.offsetHeight;
        for (
          let node: HTMLElement | null = composer;
          node && node !== appRoot;
        ) {
          naturalBottom += node.offsetTop;
          node = node.offsetParent as HTMLElement | null;
          if (node) naturalBottom += node.clientTop;
        }
        for (
          let node = composer.parentElement;
          node;
          node = node.parentElement
        ) {
          naturalBottom -= node.scrollTop;
          if (node === appRoot) break;
        }
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
          ) {
            node.style.setProperty(property, `${value}px`);
            if (node === composer)
              moveComposer(
                node,
                Number.isFinite(previous) ? previous : 0,
                value,
              );
          }
        };
        retainTarget(composer, "--keyboard-composer-shift", composerShift);
        composer.setAttribute("data-keyboard-floating", "");
        if (messages) {
          const clearance = Math.max(
            0,
            messages.getBoundingClientRect().bottom -
              (naturalBottom - composer.offsetHeight + composerShift),
          );
          retainTarget(messages, "--keyboard-scroll-clearance", clearance);
        }
        // Resolve the CSS transition before WebKit suspends frame callbacks.
        composer.getBoundingClientRect();
        messages?.getBoundingClientRect();
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
    const receiveNative = (event: Event) => {
      const state = (event as CustomEvent<NativeViewportState>).detail;
      if (!state || state.keyboard === nativeKeyboard) return schedule();
      nativeKeyboard = state.keyboard;
      // The composer starts with the same timing as the keyboard, in the
      // frame that receives the event, before UIKit's animation advances.
      if (nativeKeyboard)
        root.style.setProperty(
          "--keyboard-motion",
          nativeKeyboardMotion(nativeKeyboard),
        );
      update();
    };
    window.addEventListener(nativeViewportEvent, receiveNative);
    const content = new MutationObserver(() => {
      if (!composer?.isConnected) schedule();
    });
    content.observe(root, { childList: true, subtree: true });
    const sampleNativePan = (time: number) => {
      focusFrame = 0;
      if (time > focusDeadline) return;
      const geometry = [
        viewport?.height,
        viewport?.width,
        viewport?.scale,
        viewport?.offsetTop,
        viewport?.pageTop,
        window.innerHeight,
        window.scrollX,
        window.scrollY,
      ].join("|");
      if (geometry !== sampledGeometry) {
        sampledGeometry = geometry;
        update();
      }
      focusFrame = requestAnimationFrame(sampleNativePan);
    };
    const startFocusSampling = (focused: boolean) => {
      if (
        !viewport ||
        (!compact &&
          !hasTouchCapability(coarsePointer.matches, navigator.maxTouchPoints))
      )
        return;
      cancelAnimationFrame(focusFrame);
      sampledGeometry = "";
      focusDeadline = focused
        ? Infinity
        : performance.now() + layoutValues.keyboard.focusSampleWindow;
      focusFrame = requestAnimationFrame(sampleNativePan);
    };
    const sampleAfterFocus = () => {
      const target = document.activeElement;
      if (
        !(target instanceof HTMLTextAreaElement) ||
        !target.matches("#composer textarea")
      )
        return;
      hadComposerFocus = true;
      composerClosing = false;
      startFocusSampling(true);
    };
    const recoverAfterBlur = () => {
      if (hadComposerFocus) {
        hadComposerFocus = false;
        composerClosing = true;
        resetComposer();
        startFocusSampling(false);
      }
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
    const focusWithoutNativePan = (event: Event) => {
      if (
        !viewport ||
        !hasTouchCapability(coarsePointer.matches, navigator.maxTouchPoints)
      )
        return;
      if (
        event.type === "pointerdown" &&
        (event as PointerEvent).pointerType === "mouse"
      )
        return;
      const target = event.target;
      if (
        !(target instanceof HTMLTextAreaElement) ||
        !target.matches("#composer textarea")
      )
        return;
      const focused = document.activeElement === target;
      if (
        focused &&
        root.hasAttribute("data-viewport-keyboard") &&
        interceptedTextarea !== target
      )
        return;
      event.preventDefault();
      if (interceptedTextarea !== target) {
        interceptedTextarea = target;
        root.setAttribute(
          viewportGestureCountAttribute,
          String(++gestureCount),
        );
        if (focused) target.blur();
        target.focus({ preventScroll: true });
      }
    };
    const endGesture = () => {
      interceptedTextarea = null;
    };
    document.addEventListener("pointerdown", focusWithoutNativePan, {
      capture: true,
      passive: false,
    });
    document.addEventListener("touchstart", focusWithoutNativePan, {
      capture: true,
      passive: false,
    });
    const gestureEndEvents = [
      "pointerup",
      "pointercancel",
      "touchend",
      "touchcancel",
    ];
    for (const type of gestureEndEvents)
      document.addEventListener(type, endGesture, true);
    document.addEventListener("focusin", sampleAfterFocus);
    document.addEventListener("focusout", recoverAfterBlur);
    update();
    sampleAfterFocus();
    coarsePointer.addEventListener("change", schedule);
    viewport?.addEventListener("resize", update);
    viewport?.addEventListener("scroll", update);
    window.addEventListener("resize", update);
    window.addEventListener("orientationchange", update);
    window.addEventListener("scroll", update);
    return () => {
      cancelAnimationFrame(frame);
      cancelAnimationFrame(focusFrame);
      recoveryFrames.forEach(cancelAnimationFrame);
      document.removeEventListener("pointerdown", focusWithoutNativePan, {
        capture: true,
      });
      document.removeEventListener("touchstart", focusWithoutNativePan, {
        capture: true,
      });
      for (const type of gestureEndEvents)
        document.removeEventListener(type, endGesture, true);
      document.removeEventListener("focusin", sampleAfterFocus);
      document.removeEventListener("focusout", recoverAfterBlur);
      composerResize.disconnect();
      content.disconnect();
      coarsePointer.removeEventListener("change", schedule);
      viewport?.removeEventListener("resize", update);
      viewport?.removeEventListener("scroll", update);
      window.removeEventListener("resize", update);
      window.removeEventListener("orientationchange", update);
      window.removeEventListener("scroll", update);
      window.removeEventListener(nativeViewportEvent, receiveNative);
      clear();
    };
  }, [active, compact]);
}
