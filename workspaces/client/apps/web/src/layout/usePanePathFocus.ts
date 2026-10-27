import { useLayoutEffect, useRef } from "react";
import type { PanePathLayout } from "./deriveWorkspaceLayout";

/** Keep focus in a visible pane when a retained pane becomes hidden. */
export function usePanePathFocus(layout: PanePathLayout | null) {
  const last = useRef<HTMLElement | null>(null);
  useLayoutEffect(() => {
    const remember = () => {
      const node = document.activeElement;
      if (node instanceof HTMLElement && node !== document.body)
        last.current = node.closest(
          ".workspace, #sidebar, .duo-pane-panel, #team",
        )
          ? node
          : null;
    };
    remember();
    document.addEventListener("focusin", remember);
    return () => document.removeEventListener("focusin", remember);
  }, []);
  const placement = JSON.stringify(layout);
  useLayoutEffect(() => {
    if (!layout) return;
    const visible = (node: HTMLElement) =>
      node.isConnected &&
      node.getClientRects().length > 0 &&
      !node.closest("[hidden]");
    const restore = () => {
      const active = document.activeElement;
      const previous =
        active instanceof HTMLElement && active !== document.body
          ? active
          : last.current;
      if (!previous || visible(previous)) return;
      const selector = layout.visible.includes("panel")
        ? ".duo-pane-panel:not([hidden]), .terminal-pane-path:not([hidden])"
        : layout.visible.includes("conversation")
          ? ".workspace-frame[data-pane-path] > .workspace:not([hidden])"
          : '#sidebar[data-presentation="path"]:not([hidden])';
      const pane = document.querySelector<HTMLElement>(selector);
      if (!pane || !visible(pane)) return;
      const control = [
        ...pane.querySelectorAll<HTMLElement>(
          'input, textarea, button:not(:disabled), a[href], [tabindex]:not([tabindex="-1"])',
        ),
      ].find(visible);
      if (control) control.focus({ preventScroll: true });
      else {
        pane.tabIndex = -1;
        pane.focus({ preventScroll: true });
      }
    };
    restore();
    const frame = requestAnimationFrame(restore);
    return () => cancelAnimationFrame(frame);
  }, [placement]);
}
