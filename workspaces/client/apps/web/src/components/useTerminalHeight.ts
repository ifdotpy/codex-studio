import { useLayoutEffect, useState, type RefObject } from "react";
import { terminalHeight, type TerminalHeightBounds } from "./terminalHeight";

type Geometry = { available: number; viewport: number; rootFont: number };

/** Read unreserved host space; the dock reservation must not feed its own cap. */
export function useTerminalHeight(
  dock: RefObject<HTMLElement | null>,
  preferred: unknown,
  opened: boolean,
  bounds: (rootFont: number) => TerminalHeightBounds,
) {
  const [geometry, setGeometry] = useState<Geometry>(() => ({
    available: window.innerHeight,
    viewport: window.innerHeight,
    rootFont: Number.parseFloat(
      getComputedStyle(document.documentElement).fontSize,
    ),
  }));
  useLayoutEffect(() => {
    const parent = dock.current?.parentElement;
    if (!parent) return;
    let frame = 0;
    const observer = new ResizeObserver(() => schedule());
    let observed = new Set<Element>();
    const measure = () => {
      frame = 0;
      const workspace = parent.querySelector<HTMLElement>(".workspace");
      const header = workspace?.querySelector<HTMLElement>(".workspace-header");
      const conversation =
        workspace?.querySelector<HTMLElement>("#conversation");
      // The viewport surface positions the dock inside safe areas and keyboard bounds.
      // Its bottom is independent of the terminal height, so no viewport delta is repeated.
      const bottom =
        dock.current?.getBoundingClientRect().bottom ?? window.innerHeight;
      let reserved = header?.getBoundingClientRect().height ?? 0;
      const targets = new Set<Element>([document.documentElement]);
      if (workspace) targets.add(workspace);
      if (header) targets.add(header);
      if (conversation) {
        for (const element of conversation.children) {
          if (
            !(element instanceof HTMLElement) ||
            element.classList.contains("jump-slot")
          )
            continue;
          const style = getComputedStyle(element);
          if (
            style.display === "none" ||
            style.position === "absolute" ||
            style.position === "fixed"
          )
            continue;
          const verticalBorder =
            (Number.parseFloat(style.borderTopWidth) || 0) +
            (Number.parseFloat(style.borderBottomWidth) || 0);
          const height =
            element.id === "messages"
              ? (Number.parseFloat(style.paddingTop) || 0) +
                (Number.parseFloat(style.paddingBottom) || 0) +
                verticalBorder
              : element.id === "composer"
                ? Math.max(
                    element.getBoundingClientRect().height,
                    element.scrollHeight + verticalBorder,
                  )
                : element.getBoundingClientRect().height;
          reserved +=
            height +
            (Number.parseFloat(style.marginTop) || 0) +
            (Number.parseFloat(style.marginBottom) || 0);
          targets.add(element);
        }
      }
      for (const element of observed) {
        if (!targets.has(element)) observer.unobserve(element);
      }
      for (const element of targets) {
        if (!observed.has(element)) observer.observe(element);
      }
      observed = targets;
      const hostTop = Math.max(0, workspace?.getBoundingClientRect().top ?? 0);
      const next = {
        available: Math.max(0, bottom - hostTop - reserved),
        viewport: Math.max(0, bottom - hostTop),
        rootFont: Number.parseFloat(
          getComputedStyle(document.documentElement).fontSize,
        ),
      };
      setGeometry((old) =>
        old.available === next.available &&
        old.viewport === next.viewport &&
        old.rootFont === next.rootFont
          ? old
          : next,
      );
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(measure);
    };
    const mutations = new MutationObserver(schedule);
    // A chat switch can replace the measured controls without a window resize.
    mutations.observe(parent, { childList: true, subtree: true });
    const rootStyles = new MutationObserver(schedule);
    rootStyles.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["style", "class"],
    });
    window.addEventListener("resize", schedule);
    window.visualViewport?.addEventListener("resize", schedule);
    window.visualViewport?.addEventListener("scroll", schedule);
    document.fonts?.addEventListener("loadingdone", schedule);
    measure();
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
      mutations.disconnect();
      rootStyles.disconnect();
      window.removeEventListener("resize", schedule);
      window.visualViewport?.removeEventListener("resize", schedule);
      window.visualViewport?.removeEventListener("scroll", schedule);
      document.fonts?.removeEventListener("loadingdone", schedule);
    };
  }, [dock]);
  return terminalHeight({
    ...geometry,
    preferred,
    opened,
    bounds: bounds(geometry.rootFont),
  });
}
