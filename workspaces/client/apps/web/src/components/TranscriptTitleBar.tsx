import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { layoutValues } from "../layout/layout-contract.generated";

/** Keep the title text left of the actual top rail buttons, including native status space. */
export function TranscriptTitleBar({ children }: { children: ReactNode }) {
  const bar = useRef<HTMLElement>(null);
  const [clearance, setClearance] = useState<number>();
  const [start, setStart] = useState<number>();
  const [textHidden, setTextHidden] = useState(false);
  useLayoutEffect(() => {
    const node = bar.current;
    if (!node) return;
    let rail: Element | null = null;
    const railChanges = new MutationObserver(() => update());
    const resize = new ResizeObserver((entries) => {
      const band = entries.find((entry) => entry.target === node);
      if (band)
        setTextHidden(band.contentRect.width < layoutValues.duo.titleMinWidth);
      update();
    });
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
      const pane = rail && node.closest(".workspace-frame[data-pane-path]");
      setStart(
        pane
          ? Math.max(0, node.parentElement!.getBoundingClientRect().left)
          : undefined,
      );
      setClearance(
        first
          ? Math.max(0, node.getBoundingClientRect().right - first.left)
          : undefined,
      );
    };
    resize.observe(node);
    if (node.parentElement) resize.observe(node.parentElement);
    const bodyChanges = new MutationObserver(update);
    bodyChanges.observe(document.body, { childList: true });
    const viewportChanges = new MutationObserver(update);
    viewportChanges.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-viewport-top-strip"],
    });
    window.addEventListener("resize", update);
    window.addEventListener("studio-native-viewport", update);
    update();
    return () => {
      resize.disconnect();
      railChanges.disconnect();
      bodyChanges.disconnect();
      viewportChanges.disconnect();
      window.removeEventListener("resize", update);
      window.removeEventListener("studio-native-viewport", update);
    };
  }, []);
  return (
    <header
      ref={bar}
      className="duo-transcript-bar"
      data-title-hidden={textHidden || undefined}
      style={{
        paddingInlineStart:
          start === undefined
            ? undefined
            : `calc(${start}px + var(--layout-duo-column-gutter))`,
        paddingInlineEnd:
          clearance === undefined
            ? undefined
            : `calc(${clearance}px + var(--mantine-spacing-xs))`,
      }}
    >
      {children}
    </header>
  );
}
