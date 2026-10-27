import {
  hasTouchCapability,
  touchPointerMedia,
} from "../../hooks/touchCapability";
import { useEffect, useState } from "react";
import { layoutValues } from "../../layout/layout-contract.generated";
import { viewportTopStripAttribute } from "../../hooks/mobileViewport";
import {
  edgeRailGeometry,
  type EdgeRailGeometry,
  type EdgeRailMetrics,
  type EdgeRailViewport,
} from "./edgeRailPolicy";

export const layoutEdgeRailMetrics: EdgeRailMetrics = {
  minimumInset: layoutValues.edgeRail.minInset,
  statusReserve: layoutValues.edgeRail.statusReserve,
  targetSize: layoutValues.edgeRail.buttonSize,
  statusColumnCenter: layoutValues.edgeRail.statusColumnCenter,
  topStatusReserve: layoutValues.edgeRail.topStatusReserve,
};

type RootStyle = Pick<
  CSSStyleDeclaration,
  "height" | "paddingTop" | "paddingRight" | "paddingBottom" | "paddingLeft"
> &
  Partial<Pick<CSSStyleDeclaration, "width" | "getPropertyValue">>;

/** The native shell's status column edges (nativeViewport.ts), when set. */
function statusColumn(style: RootStyle) {
  const read = (name: string) =>
    Number.parseFloat(style.getPropertyValue?.(name) ?? "");
  const bottom = read("--native-status-column-bottom");
  const left = read("--native-status-column-left");
  return Number.isFinite(bottom) && Number.isFinite(left)
    ? { statusColumn: { bottom, left } }
    : {};
}

/** Read resolved CSS lengths. The viewport owner supplies all compensation. */
export function readEdgeRailViewport(
  style: RootStyle,
  coarsePointer: boolean,
  topStrip = false,
): EdgeRailViewport {
  return {
    coarsePointer,
    viewportHeight: Number.parseFloat(style.height),
    ...(style.width ? { viewportWidth: Number.parseFloat(style.width) } : {}),
    // The viewport owner moved a native top strip out of the page inset
    // (data-viewport-top-strip); its height is the shell's top inset.
    ...(topStrip && style.getPropertyValue
      ? {
          topStrip: Number.parseFloat(
            style.getPropertyValue("--native-safe-top"),
          ),
        }
      : {}),
    ...statusColumn(style),
    insets: {
      top: Number.parseFloat(style.paddingTop),
      right: Number.parseFloat(style.paddingRight),
      bottom: Number.parseFloat(style.paddingBottom),
      left: Number.parseFloat(style.paddingLeft),
    },
  };
}

/** Observe the host root. Do not derive another keyboard or safe-area model. */
export function useEdgeRail(
  metrics: EdgeRailMetrics = layoutEdgeRailMetrics,
  enabled = true,
) {
  const [geometry, setGeometry] = useState<EdgeRailGeometry | null>(null);
  const {
    minimumInset,
    statusReserve,
    targetSize,
    statusColumnCenter,
    topStatusReserve,
  } = metrics;
  useEffect(() => {
    if (!enabled) {
      setGeometry(null);
      return;
    }
    const root = document.getElementById("root");
    if (!root) return;
    const coarse = touchPointerMedia();
    let frame = 0;
    const update = () => {
      frame = 0;
      const next = edgeRailGeometry(
        readEdgeRailViewport(
          getComputedStyle(root),
          hasTouchCapability(coarse.matches, navigator.maxTouchPoints),
          document.documentElement.hasAttribute(viewportTopStripAttribute),
        ),
        {
          minimumInset,
          statusReserve,
          targetSize,
          statusColumnCenter,
          topStatusReserve,
        },
      );
      setGeometry((current) =>
        JSON.stringify(current) === JSON.stringify(next) ? current : next,
      );
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    const resize = new ResizeObserver(schedule);
    resize.observe(root);
    const mutation = new MutationObserver(schedule);
    mutation.observe(document.documentElement, { attributes: true });
    mutation.observe(root, { attributes: true });
    coarse.addEventListener("change", schedule);
    window.addEventListener("resize", schedule);
    window.addEventListener("orientationchange", schedule);
    window.visualViewport?.addEventListener("resize", schedule);
    window.visualViewport?.addEventListener("scroll", schedule);
    update();
    return () => {
      cancelAnimationFrame(frame);
      resize.disconnect();
      mutation.disconnect();
      coarse.removeEventListener("change", schedule);
      window.removeEventListener("resize", schedule);
      window.removeEventListener("orientationchange", schedule);
      window.visualViewport?.removeEventListener("resize", schedule);
      window.visualViewport?.removeEventListener("scroll", schedule);
    };
  }, [
    enabled,
    minimumInset,
    statusReserve,
    targetSize,
    statusColumnCenter,
    topStatusReserve,
  ]);
  return enabled ? geometry : null;
}
