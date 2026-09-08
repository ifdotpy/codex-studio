import { useLayoutEffect, useRef, useState } from "react";

type Position = { top: number; following: boolean; distance: number };

// One owner for scroll anchoring. Browser anchoring and React effects must not
// both compensate for the same composer resize or streamed paragraph.
export function useConversationScroll(id: string, ready: boolean) {
  const scroll = useRef<HTMLDivElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const [follow, updateFollow] = useState(true);
  const following = useRef(true);
  const positions = useRef(new Map<string, Position>());
  const current = useRef("");
  const available = useRef(ready);
  available.current = ready;
  const lastTop = useRef(0);
  const bottomDistance = useRef(0);
  const lastScrollInput = useRef(-Infinity);
  const anchor = useRef<{ element: HTMLElement; offset: number } | null>(null);

  const remember = () => {
    const root = scroll.current;
    if (!root) return;
    lastTop.current = root.scrollTop;
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
    const element =
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
  const restore = () => {
    const root = scroll.current;
    if (!root || !available.current) return;
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
  };
  const setFollow = (value: boolean) => {
    following.current = value;
    updateFollow(value);
    if (value) {
      bottomDistance.current = 0;
      restore();
    } else remember();
  };

  useLayoutEffect(() => {
    if (current.current !== id) {
      current.current = id;
      lastScrollInput.current = -Infinity;
      anchor.current = null;
      const saved = positions.current.get(id);
      lastTop.current = saved?.top || 0;
      bottomDistance.current = saved?.distance || 0;
      following.current = saved?.following ?? true;
      updateFollow(following.current);
    }
    restore();
  });

  useLayoutEffect(() => {
    const root = scroll.current;
    const body = content.current;
    if (!root || !body) return;
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
      lastScrollInput.current = performance.now();
    };
    for (const type of ["wheel", "touchmove", "keydown", "pointerdown"])
      root.addEventListener(type, input, { passive: true });
    const observer = new ResizeObserver(restore);
    observer.observe(root);
    observer.observe(body);
    return () => {
      observer.disconnect();
      for (const type of ["wheel", "touchmove", "keydown", "pointerdown"])
        root.removeEventListener(type, input);
    };
  }, [id]);

  const onScroll = () => {
    const root = scroll.current;
    if (!root || !available.current || root.scrollTop === lastTop.current)
      return;
    const atBottom =
      root.scrollHeight - root.scrollTop - root.clientHeight < 32;
    // A layout change can clamp scrollTop. Only user input or Latest resumes following.
    const value =
      atBottom &&
      (following.current || performance.now() - lastScrollInput.current < 600);
    following.current = value;
    updateFollow(value);
    remember();
  };
  return { scroll, content, follow, setFollow, onScroll, remember };
}
