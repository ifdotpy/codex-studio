import { useLayoutEffect, useRef, useState, type ReactNode } from "react";

/** Keep the title left of the actual top rail buttons, including native status space. */
export function TranscriptTitleBar({ children }: { children: ReactNode }) {
  const bar = useRef<HTMLElement>(null);
  const [width, setWidth] = useState<number>();
  useLayoutEffect(() => {
    const node = bar.current;
    if (!node) return;
    let rail: Element | null = null;
    const railChanges = new MutationObserver(() => update());
    const resize = new ResizeObserver(() => update());
    const update = () => {
      const nextRail = document.querySelector('.edge-rail[data-side="top"]');
      if (nextRail !== rail) {
        railChanges.disconnect();
        if (rail) resize.unobserve(rail);
        rail = nextRail;
        if (rail) {
          resize.observe(rail);
          railChanges.observe(rail, {
            childList: true,
            subtree: true,
            attributes: true,
            attributeFilter: ["style"],
          });
        }
      }
      const first = rail?.querySelector("button")?.getBoundingClientRect();
      setWidth(
        first
          ? Math.max(0, first.left - node.getBoundingClientRect().left)
          : undefined,
      );
    };
    if (node.parentElement) resize.observe(node.parentElement);
    const bodyChanges = new MutationObserver(update);
    bodyChanges.observe(document.body, { childList: true });
    const viewportChanges = new MutationObserver(update);
    viewportChanges.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["style", "data-viewport-top-strip"],
    });
    window.addEventListener("resize", update);
    update();
    return () => {
      resize.disconnect();
      railChanges.disconnect();
      bodyChanges.disconnect();
      viewportChanges.disconnect();
      window.removeEventListener("resize", update);
    };
  }, []);
  return (
    <header
      ref={bar}
      className="duo-transcript-bar"
      style={{ maxWidth: width }}
    >
      {children}
    </header>
  );
}
