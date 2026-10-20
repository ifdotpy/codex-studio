import { useServerApi } from "../servers/ServerServicesContext";
import {
  useCallback,
  useLayoutEffect,
  useRef,
  useSyncExternalStore,
} from "react";
import "../api";

type Position = { top: number; following: boolean; distance: number };

/** Distance from the bottom, in CSS pixels, inside which the transcript keeps
 * following new content. */
export const FOLLOW_THRESHOLD_PX = 32;
/** Time, in milliseconds, for which wheel, touch, or key input is recent. */
const RECENT_INPUT_MS = 600;
/** Scroll distance, in CSS pixels, of one wheel line (`deltaMode` 1). */
const WHEEL_LINE_PX = 40;
/** Rounding allowance, in CSS pixels, between input deltas and a move. */
const INPUT_SLACK_PX = 4;

export function useFollowState(
  subscribe: (listener: () => void) => () => void,
  getSnapshot: () => boolean,
) {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

// One owner for scroll anchoring. Browser anchoring and React effects must not
// both compensate for the same composer resize or streamed paragraph.
export function useConversationScroll(id: string, ready: boolean) {
  const { saved, save } = useServerApi();

  const scroll = useRef<HTMLDivElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const following = useRef(true);
  const followListeners = useRef(new Set<() => void>());
  const getFollow = useCallback(() => following.current, []);
  const subscribeFollow = useCallback((listener: () => void) => {
    followListeners.current.add(listener);
    return () => {
      followListeners.current.delete(listener);
    };
  }, []);
  const updateFollow = (value: boolean) => {
    if (following.current === value) return;
    following.current = value;
    followListeners.current.forEach((listener) => listener());
  };
  const positions = useRef(new Map<string, Position>());
  const current = useRef("");
  const available = useRef(ready);
  available.current = ready;
  const lastTop = useRef(0);
  const lastHeight = useRef(0);
  const bottomDistance = useRef(0);
  const lastScrollInput = useRef(-Infinity);
  const hasRecentScrollInput = useCallback(
    () => performance.now() - lastScrollInput.current < RECENT_INPUT_MS,
    [],
  );
  // CSS pixels that the recent input can scroll up and down.
  const inputReach = useRef({ up: 0, down: 0 });
  const anchor = useRef<{ element: HTMLElement; offset: number } | null>(null);
  // False until the first restore of the current chat applies its position.
  const restored = useRef(false);

  const persist = () => {
    const position = positions.current.get(current.current);
    if (position) save(`studio-chat-scroll:${current.current}`, position);
  };
  const remember = () => {
    const root = scroll.current;
    if (!root) return;
    lastTop.current = root.scrollTop;
    lastHeight.current = root.scrollHeight;
    bottomDistance.current = Math.max(
      0,
      root.scrollHeight - root.clientHeight - root.scrollTop,
    );
    positions.current.set(current.current, {
      top: root.scrollTop,
      following: following.current,
      distance: bottomDistance.current,
    });
    if (following.current) {
      anchor.current = null;
      return;
    }
    const bounds = root.getBoundingClientRect();
    const visible = (node: HTMLElement) => {
      const box = node.getBoundingClientRect();
      return (
        box.height > 0 && box.bottom > bounds.top && box.top < bounds.bottom
      );
    };
    // Keep the existing visible anchor. Input and stream renders must not
    // measure every preceding paragraph just to remember the same position.
    const retained = anchor.current?.element;
    const element =
      (retained?.isConnected && visible(retained) ? retained : null) ||
      Array.from(
        root.querySelectorAll<HTMLElement>(
          "[data-message] p, [data-message] pre, [data-message] li",
        ),
      ).find(visible) ||
      Array.from(
        root.querySelectorAll<HTMLElement>("[data-message], .tool-group"),
      ).find(visible);
    anchor.current = element
      ? { element, offset: element.getBoundingClientRect().top - bounds.top }
      : null;
  };
  // Update follow state from the reader's current position.
  const track = () => {
    const root = scroll.current;
    if (!root) return;
    const atBottom =
      root.scrollHeight - root.scrollTop - root.clientHeight <
      FOLLOW_THRESHOLD_PX;
    // A layout change can move scrollTop. A downward move over unchanged
    // content comes from the user, including momentum and scrollbar drags.
    const value =
      atBottom &&
      (following.current ||
        (root.scrollTop > lastTop.current &&
          root.scrollHeight === lastHeight.current) ||
        hasRecentScrollInput());
    updateFollow(value);
    remember();
  };
  // WebKit (the macOS client) applies a wheel or trackpad step to scrollTop
  // before it dispatches the scroll event. A render or observer in that gap
  // must keep the reader's new position, not restore the old one. Code that
  // moves scrollTop calls remember(). An unrecorded move belongs to the
  // reader only when recent input can explain its direction and distance:
  // WebKit also moves scrollTop by itself while the transcript changes, by
  // much more than the input.
  const trackUnseenScroll = () => {
    const root = scroll.current;
    if (
      !root ||
      !restored.current ||
      root.scrollTop === lastTop.current ||
      !hasRecentScrollInput()
    )
      return;
    const moved = root.scrollTop - lastTop.current;
    const reach = moved < 0 ? inputReach.current.up : inputReach.current.down;
    if (Math.abs(moved) > reach + INPUT_SLACK_PX) return;
    track();
    // The scroll event that follows sees no change and does not persist.
    persist();
  };
  // An explicit follow request wins over an unseen reader move.
  const restore = (explicit = false) => {
    const root = scroll.current;
    if (!root || !available.current) return;
    if (!explicit) trackUnseenScroll();
    restored.current = true;
    const hadAnchor = !!anchor.current;
    if (following.current)
      root.scrollTop =
        root.scrollHeight - root.clientHeight - bottomDistance.current;
    else if (
      anchor.current?.element.isConnected &&
      anchor.current.element.getClientRects().length
    ) {
      root.scrollTop +=
        anchor.current.element.getBoundingClientRect().top -
        root.getBoundingClientRect().top -
        anchor.current.offset;
    } else root.scrollTop = lastTop.current;
    remember();
    // A first restore uses the saved offset. Do not replace it with a layout anchor.
    if (!hadAnchor && !following.current) anchor.current = null;
  };
  const setFollow = (value: boolean) => {
    updateFollow(value);
    if (value) {
      bottomDistance.current = 0;
      restore(true);
    } else remember();
    persist();
  };

  useLayoutEffect(() => {
    if (current.current !== id) {
      persist();
      current.current = id;
      lastScrollInput.current = -Infinity;
      anchor.current = null;
      const stored =
        positions.current.get(id) ||
        saved<Position | null>(`studio-chat-scroll:${id}`, null);
      const valid =
        stored &&
        Number.isFinite(stored.top) &&
        Number.isFinite(stored.distance) &&
        typeof stored.following === "boolean";
      lastTop.current = valid ? stored.top : 0;
      bottomDistance.current = valid ? stored.distance : 0;
      updateFollow(valid ? stored.following : true);
      // The old chat's scrollTop is not a reader move in this chat.
      restored.current = false;
    }
    restore();
  });

  useLayoutEffect(() => {
    const root = scroll.current;
    const body = content.current;
    if (!root || !body) return;
    let touchY: number | null = null;
    const touchStart = (event: TouchEvent) => {
      touchY = event.touches[0]?.clientY ?? null;
    };
    const input = (event: Event) => {
      if (event instanceof KeyboardEvent) {
        if (
          ![
            "ArrowUp",
            "ArrowDown",
            "PageUp",
            "PageDown",
            "Home",
            "End",
            " ",
          ].includes(event.key)
        )
          return;
        if (
          (event.target as HTMLElement)?.closest(
            "input, textarea, [contenteditable=true]",
          )
        )
          return;
      }
      if (event.type === "pointerdown" && event.target !== root) return;
      const now = performance.now();
      if (now - lastScrollInput.current >= RECENT_INPUT_MS)
        inputReach.current = { up: 0, down: 0 };
      lastScrollInput.current = now;
      const extend = (delta: number) => {
        if (delta < 0) inputReach.current.up -= delta;
        else inputReach.current.down += delta;
      };
      if (event instanceof WheelEvent)
        extend(
          event.deltaY *
            (event.deltaMode === 1
              ? WHEEL_LINE_PX
              : event.deltaMode === 2
                ? root.clientHeight
                : 1),
        );
      else if (event instanceof KeyboardEvent) {
        const distance = ["Home", "End"].includes(event.key)
          ? Infinity
          : root.clientHeight;
        const up =
          ["ArrowUp", "PageUp", "Home"].includes(event.key) ||
          (event.key === " " && event.shiftKey);
        extend(up ? -distance : distance);
      } else if (event.type === "touchmove") {
        // Desktop WebKit has no TouchEvent constructor to test against.
        const y = (event as TouchEvent).touches[0]?.clientY;
        if (y !== undefined && touchY !== null) extend(touchY - y);
        touchY = y ?? null;
      } else inputReach.current = { up: Infinity, down: Infinity };
    };
    root.addEventListener("touchstart", touchStart, { passive: true });
    for (const type of ["wheel", "touchmove", "keydown", "pointerdown"])
      root.addEventListener(type, input, { passive: true });
    const checkpoint = () => {
      if (available.current) persist();
    };
    window.addEventListener("pagehide", checkpoint);
    document.addEventListener("visibilitychange", checkpoint);
    const observer = new ResizeObserver(() => restore());
    observer.observe(root);
    observer.observe(body);
    const mutations = new MutationObserver(() => restore());
    mutations.observe(body, { childList: true, subtree: true });
    return () => {
      checkpoint();
      window.removeEventListener("pagehide", checkpoint);
      document.removeEventListener("visibilitychange", checkpoint);
      observer.disconnect();
      mutations.disconnect();
      root.removeEventListener("touchstart", touchStart);
      for (const type of ["wheel", "touchmove", "keydown", "pointerdown"])
        root.removeEventListener(type, input);
    };
  }, [id]);

  const onScroll = () => {
    const root = scroll.current;
    if (!root || !available.current || root.scrollTop === lastTop.current)
      return;
    track();
    persist();
  };
  const revealRequest = useCallback((node: HTMLElement) => {
    const root = scroll.current;
    if (!root || !root.contains(node)) return;
    updateFollow(false);
    anchor.current = null;
    root.scrollTop +=
      node.getBoundingClientRect().top - root.getBoundingClientRect().top - 12;
    remember();
    anchor.current = {
      element: node,
      offset:
        node.getBoundingClientRect().top - root.getBoundingClientRect().top,
    };
    persist();
  }, []);
  const getAnchorId = () => {
    const root = scroll.current;
    if (!root) return null;
    const bounds = root.getBoundingClientRect();
    const visible = (node: HTMLElement) => {
      const box = node.getBoundingClientRect();
      return (
        box.height > 0 && box.bottom > bounds.top && box.top < bounds.bottom
      );
    };
    const retained = anchor.current?.element;
    const nodes = Array.from(
      root.querySelectorAll<HTMLElement>("[data-message]"),
    );
    const row = retained?.isConnected
      ? retained.closest<HTMLElement>("[data-message]")
      : null;
    const element =
      row && visible(row)
        ? row
        : (following.current ? [...nodes].reverse() : nodes).find(visible);
    return element?.dataset.message || element?.dataset.sourceMessage || null;
  };
  return {
    scroll,
    content,
    getFollow,
    subscribeFollow,
    setFollow,
    onScroll,
    hasRecentScrollInput,
    remember,
    revealRequest,
    getAnchorId,
  };
}
