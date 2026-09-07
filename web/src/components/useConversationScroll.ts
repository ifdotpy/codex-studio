import { useLayoutEffect, useRef, useState } from "react";

type Position = { top: number; following: boolean };

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
  const anchor = useRef<{ element: HTMLElement; offset: number } | null>(null);

  const remember = () => {
    const root = scroll.current;
    if (!root) return;
    lastTop.current = root.scrollTop;
    positions.current.set(current.current, {
      top: root.scrollTop,
      following: following.current,
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
    if (following.current) root.scrollTop = root.scrollHeight;
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
    if (value) restore();
    else remember();
  };

  useLayoutEffect(() => {
    if (current.current !== id) {
      current.current = id;
      anchor.current = null;
      const saved = positions.current.get(id);
      lastTop.current = saved?.top || 0;
      following.current = saved?.following ?? true;
      updateFollow(following.current);
    }
    restore();
  });

  useLayoutEffect(() => {
    const root = scroll.current;
    const body = content.current;
    if (!root || !body) return;
    const observer = new ResizeObserver(restore);
    observer.observe(root);
    observer.observe(body);
    return () => observer.disconnect();
  }, [id]);

  const onScroll = () => {
    const root = scroll.current;
    if (!root || !available.current || root.scrollTop === lastTop.current)
      return;
    const value = root.scrollHeight - root.scrollTop - root.clientHeight < 32;
    following.current = value;
    updateFollow(value);
    remember();
  };
  return { scroll, content, follow, setFollow, onScroll, remember };
}
