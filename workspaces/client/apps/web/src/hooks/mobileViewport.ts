import {
  hasTouchCapability,
  touchPointerMedia,
  primaryTouchPointerMedia,
} from "./touchCapability";
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

const selectedComposers = new WeakSet<HTMLTextAreaElement>();

/** Mark automatic chat selection so its first touch can open the software keyboard. */
export function focusSelectedComposer(target: HTMLElement | null) {
  if (!target) return;
  if (
    target instanceof HTMLTextAreaElement &&
    primaryTouchPointerMedia().matches
  ) {
    selectedComposers.add(target);
    target.focus({ preventScroll: true });
  } else target.focus();
}

const fullFrames = new WeakMap<Document, { height: number; width: number }>();

/** Keep the full touch layout behind the keyboard while its composer follows the visible bottom. */
export function useMobileViewport(active: boolean, compact = false) {
  useEffect(() => {
    if (!active) return;
    const viewport = window.visualViewport;
    const root = document.documentElement;
    const coarsePointer = touchPointerMedia();
    const primaryPointer = primaryTouchPointerMedia();
    let frame = 0;
    let focusFrame = 0;
    let focusDeadline = 0;
    let hadComposerFocus = false;
    let composerClosing = false;
    let sampledGeometry = "";
    let stableSince = 0;
    let minimumWatchUntil = 0;
    let focusedComposer: HTMLTextAreaElement | null = null;
    let touchListeners = false;
    let interceptedTextarea: HTMLTextAreaElement | null = null;
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
    // start position and plays from the next rendered frame. A downward
    // (closing) motion plays from the second one and waits
    // keyboard.nativeMotionHold more, so its first presented frame still
    // shows the start position. An upward (opening) motion starts at once:
    // the rising keyboard covers its first frames, and a later start leaves
    // the composer under the keyboard.
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
      const closing = to > from;
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
          delay: closing ? layoutValues.keyboard.nativeMotionHold : 0,
          fill: "backwards",
        },
      );
      motion.pause();
      composerMotion = motion;
      cancelAnimationFrame(composerMotionFrame);
      const play = () => {
        composerMotionFrame = 0;
        if (composerMotion === motion) motion.play();
      };
      composerMotionFrame = requestAnimationFrame(() => {
        composerMotionFrame = closing ? requestAnimationFrame(play) : 0;
        if (!closing) play();
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
      if (hasTouchCapability(coarsePointer.matches, navigator.maxTouchPoints))
        root.setAttribute("data-touch-input", "");
      else root.removeAttribute("data-touch-input");
      syncTouchListeners();
      if (!compact && !primaryPointer.matches) {
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
      const editable = document.activeElement as HTMLElement | null;
      const hasEditableFocus =
        !!editable &&
        (editable.matches?.("input, textarea") || editable.isContentEditable);
      const keyboard = native
        ? nativeOverlap > layoutValues.keyboard.occlusionMin
        : !!viewport &&
          primaryPointer.matches &&
          (hasEditableFocus || composerClosing) &&
          Math.max(
            layoutHeight - height,
            sameWidth ? fullHeight - height : 0,
            top,
          ) > layoutValues.keyboard.occlusionMin;
      if (!keyboard || !sameWidth) {
        composerClosing = false;
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
        primaryPointer.matches &&
        width > fullHeight &&
        fullHeight < layoutValues.header.shortLandscapeMaxHeight;
      const surfaceHeight = overlay ? fullHeight : height;
      if (keyboard && height < layoutValues.keyboard.compactMaxHeight)
        root.setAttribute("data-viewport-keyboard-compact", "");
      else root.removeAttribute("data-viewport-keyboard-compact");
      if (
        primaryPointer.matches &&
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
      const keyboardActive =
        hadComposerFocus && root.hasAttribute("data-viewport-keyboard");
      if (
        (time > focusDeadline && !keyboardActive) ||
        (hadComposerFocus &&
          (!focusedComposer?.isConnected ||
            document.activeElement !== focusedComposer))
      )
        return;
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
        stableSince = time;
        focusDeadline = Math.max(
          minimumWatchUntil,
          time + layoutValues.keyboard.focusSampleWindow,
        );
        update();
      } else if (
        !keyboardActive &&
        time >= minimumWatchUntil &&
        time - stableSince >= layoutValues.keyboard.stableSampleDuration
      )
        return;
      focusFrame = requestAnimationFrame(sampleNativePan);
    };
    const startFocusSampling = () => {
      if (!viewport || !primaryPointer.matches) return;
      cancelAnimationFrame(focusFrame);
      sampledGeometry = "";
      const now = performance.now();
      stableSince = now;
      minimumWatchUntil = hadComposerFocus
        ? now + layoutValues.keyboard.focusOpenGrace
        : now;
      focusDeadline = Math.max(
        minimumWatchUntil,
        now + layoutValues.keyboard.focusSampleWindow,
      );
      focusFrame = requestAnimationFrame(sampleNativePan);
    };
    const sampleAfterFocus = () => {
      const target = document.activeElement;
      if (
        !(target instanceof HTMLTextAreaElement) ||
        !target.matches("#composer textarea")
      )
        return;
      focusedComposer = target;
      hadComposerFocus = true;
      composerClosing = false;
      startFocusSampling();
    };
    const recoverAfterBlur = () => {
      if (hadComposerFocus) {
        hadComposerFocus = false;
        composerClosing = root.hasAttribute("data-viewport-keyboard");
        resetComposer();
        startFocusSampling();
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
      // The native shell removes WebKit's keyboard pan. There a programmatic
      // refocus of an already focused composer does not show the keyboard.
      if (
        !viewport ||
        nativeViewport() !== undefined ||
        !primaryPointer.matches
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
        interceptedTextarea !== target &&
        (root.hasAttribute("data-viewport-keyboard") ||
          !selectedComposers.has(target))
      )
        return;
      event.preventDefault();
      if (interceptedTextarea !== target) {
        interceptedTextarea = target;
        selectedComposers.delete(target);
        if (focused) target.blur();
        target.focus({ preventScroll: true });
      }
    };
    const endGesture = () => {
      interceptedTextarea = null;
    };
    const gestureEndEvents = [
      "pointerup",
      "pointercancel",
      "touchend",
      "touchcancel",
    ];
    const syncTouchListeners = () => {
      const needed = !!viewport && primaryPointer.matches;
      if (needed === touchListeners) return;
      touchListeners = needed;
      if (!needed) interceptedTextarea = null;
      for (const type of ["pointerdown", "touchstart"]) {
        if (needed)
          document.addEventListener(type, focusWithoutNativePan, {
            capture: true,
            passive: false,
          });
        else
          document.removeEventListener(type, focusWithoutNativePan, {
            capture: true,
          });
      }
      for (const type of gestureEndEvents) {
        if (needed) document.addEventListener(type, endGesture, true);
        else document.removeEventListener(type, endGesture, true);
      }
    };
    const viewportChanged = () => {
      update();
      if (hadComposerFocus || composerClosing) startFocusSampling();
    };
    document.addEventListener("focusin", sampleAfterFocus);
    document.addEventListener("focusout", recoverAfterBlur);
    update();
    sampleAfterFocus();
    coarsePointer.addEventListener("change", schedule);
    primaryPointer.addEventListener("change", viewportChanged);
    viewport?.addEventListener("resize", viewportChanged);
    viewport?.addEventListener("scroll", viewportChanged);
    window.addEventListener("resize", viewportChanged);
    window.addEventListener("orientationchange", viewportChanged);
    window.addEventListener("scroll", viewportChanged);
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
      primaryPointer.removeEventListener("change", viewportChanged);
      root.removeAttribute("data-touch-input");
      viewport?.removeEventListener("resize", viewportChanged);
      viewport?.removeEventListener("scroll", viewportChanged);
      window.removeEventListener("resize", viewportChanged);
      window.removeEventListener("orientationchange", viewportChanged);
      window.removeEventListener("scroll", viewportChanged);
      window.removeEventListener(nativeViewportEvent, receiveNative);
      clear();
    };
  }, [active, compact]);
}
