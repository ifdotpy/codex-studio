import { layoutValues } from "./layout-contract.generated";
import { useCallback, useLayoutEffect, useState } from "react";
import {
  deriveWorkspaceLayout,
  type LayoutRect,
} from "./deriveWorkspaceLayout";

const emptyRect: LayoutRect = { x: 0, y: 0, width: 0, height: 0 };
/** Observe the common row. Its parent already removes navigation and safe areas. */
export function useWorkspaceLayout(options: {
  workspaceOpen?: boolean;
  teamPreferred?: boolean;
  preferredPanelWidthRem?: number;
}) {
  const [host, setHost] = useState<HTMLDivElement | null>(null);
  const [geometry, setGeometry] = useState<{
    host: LayoutRect;
    viewport: LayoutRect;
    rootFontPx: number;
  }>({
    host: emptyRect,
    viewport: emptyRect,
    rootFontPx: layoutValues.viewport.rootFontDefault,
  });
  const rowRef = useCallback(
    (node: HTMLDivElement | null) => setHost(node),
    [],
  );
  useLayoutEffect(() => {
    if (!host) return;
    const document = host.ownerDocument;
    const view = document.defaultView!;
    const update = () => {
      const bounds = host.getBoundingClientRect();
      const next = {
        host: {
          x: bounds.x,
          y: bounds.y,
          width: bounds.width,
          height: bounds.height,
        },
        // The viewport hook owns keyboard and safe-area adjustments to the root.
        viewport: {
          x: 0,
          y: 0,
          width: view.innerWidth,
          height: view.innerHeight,
        },
        rootFontPx: Number.parseFloat(
          view.getComputedStyle(document.documentElement).fontSize,
        ),
      };
      setGeometry((old) =>
        JSON.stringify(old) === JSON.stringify(next) ? old : next,
      );
    };
    const resize = new ResizeObserver(update);
    resize.observe(host);
    resize.observe(document.documentElement);
    const preferences = new MutationObserver(update);
    preferences.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["style", "class"],
    });
    view.addEventListener("resize", update);
    update();
    return () => {
      resize.disconnect();
      preferences.disconnect();
      view.removeEventListener("resize", update);
    };
  }, [host]);
  return {
    rowRef,
    row: host,
    ...deriveWorkspaceLayout({
      ...geometry,
      ...options,
      hostExcludesSafeInsets: true,
    }),
  };
}
