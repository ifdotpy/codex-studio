import { useEffect, useState } from "react";
import { layoutValues } from "../../layout/layout-contract.generated";
import {
  edgeRailGeometry,
  type EdgeRailGeometry,
  type EdgeRailMetrics,
  type EdgeRailViewport,
} from "./edgeRailPolicy";

export const layoutEdgeRailMetrics: EdgeRailMetrics = {
  minimumInset: layoutValues.edgeRail.minInset,
  statusReserve: layoutValues.edgeRail.statusReserve,
  targetSize: layoutValues.controls.touchMin,
};

type RootStyle = Pick<
  CSSStyleDeclaration,
  "height" | "paddingTop" | "paddingRight" | "paddingBottom" | "paddingLeft"
>;

/** Read resolved CSS lengths. The viewport owner supplies all compensation. */
export function readEdgeRailViewport(
  style: RootStyle,
  coarsePointer: boolean,
): EdgeRailViewport {
  return {
    coarsePointer,
    viewportHeight: Number.parseFloat(style.height),
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
  const { minimumInset, statusReserve, targetSize } = metrics;
  useEffect(() => {
    if (!enabled) {
      setGeometry(null);
      return;
    }
    const root = document.getElementById("root");
    if (!root) return;
    const coarse = window.matchMedia("(pointer: coarse)");
    let frame = 0;
    const update = () => {
      frame = 0;
      const next = edgeRailGeometry(
        readEdgeRailViewport(getComputedStyle(root), coarse.matches),
        { minimumInset, statusReserve, targetSize },
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
  }, [enabled, minimumInset, statusReserve, targetSize]);
  return enabled ? geometry : null;
}
