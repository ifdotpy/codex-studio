import { layoutValues } from "./layout-contract.generated";
import { primaryTouchPointerMedia } from "../hooks/touchCapability";
import { useCallback, useLayoutEffect, useState } from "react";
import {
  deriveWorkspaceLayout,
  type LayoutRect,
  type PanePathInput,
  type SafeInsets,
} from "./deriveWorkspaceLayout";

const emptyRect: LayoutRect = { x: 0, y: 0, width: 0, height: 0 };
/** Observe the common row. Its parent already removes navigation and safe areas. */
export function useWorkspaceLayout(options: {
  workspaceOpen?: boolean;
  teamPreferred?: boolean;
  preferredPanelWidthRem?: number;
  panePath?: Omit<
    PanePathInput,
    "primaryCoarse" | "duoDevice" | "hinge" | "available"
  >;
}) {
  const [host, setHost] = useState<HTMLDivElement | null>(null);
  const [geometry, setGeometry] = useState<{
    host: LayoutRect;
    viewport: LayoutRect;
    rootFontPx: number;
    shortTouchLandscape: boolean;
    duo: boolean;
    primaryCoarse: boolean;
    duoDevice: boolean;
    available: LayoutRect;
    safeInsets: SafeInsets;
    overlayInsetTop: number;
    hinge: NonNullable<PanePathInput["hinge"]>;
  }>({
    host: emptyRect,
    viewport: emptyRect,
    rootFontPx: layoutValues.viewport.rootFontDefault,
    shortTouchLandscape: false,
    duo: false,
    duoDevice: false,
    primaryCoarse: false,
    available: emptyRect,
    safeInsets: { top: 0, right: 0, bottom: 0, left: 0 },
    overlayInsetTop: 0,
    hinge: { status: "unknown" },
  });
  const rowRef = useCallback(
    (node: HTMLDivElement | null) => setHost(node),
    [],
  );
  useLayoutEffect(() => {
    const measureHost = host ?? window.document.getElementById("root");
    if (!measureHost) return;
    const document = measureHost.ownerDocument;
    const view = document.defaultView!;
    const update = () => {
      const bounds = measureHost.getBoundingClientRect();
      const root = document.documentElement;
      const app = document.getElementById("root");
      const appStyle = view.getComputedStyle(app || root);
      const number = (value: string) => Number.parseFloat(value) || 0;
      const left = number(appStyle.paddingLeft),
        right = number(appStyle.paddingRight);
      const top = number(appStyle.paddingTop),
        bottom = number(appStyle.paddingBottom);
      const status = root.dataset.nativeHinge;
      const rootStyle = view.getComputedStyle(root);
      const optionalLength = (name: string) => {
        const value = Number.parseFloat(rootStyle.getPropertyValue(name));
        return Number.isFinite(value) && value >= 0 ? value : undefined;
      };
      const hingeLeft = optionalLength("--native-hinge-left");
      const hingeWidth = optionalLength("--native-hinge-width");
      const hingeHeight = optionalLength("--native-hinge-height");
      const hinge: NonNullable<PanePathInput["hinge"]> = {
        status:
          status === "open" || status === "closed" || status === "partial"
            ? status
            : "unknown",
        ...(hingeLeft !== undefined ? { left: hingeLeft } : {}),
        ...(hingeWidth !== undefined ? { width: hingeWidth } : {}),
        ...(hingeHeight !== undefined ? { height: hingeHeight } : {}),
      };
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
          height:
            number(
              view
                .getComputedStyle(root)
                .getPropertyValue("--viewport-layout-height"),
            ) ||
            app?.clientHeight ||
            view.innerHeight,
        },
        rootFontPx: Number.parseFloat(
          view.getComputedStyle(document.documentElement).fontSize,
        ),
        shortTouchLandscape: document.documentElement.hasAttribute(
          "data-viewport-short-landscape",
        ),
        duo: root.hasAttribute("data-duo-proportions"),
        duoDevice: root.hasAttribute("data-duo-device"),
        primaryCoarse: primaryTouchPointerMedia().matches,
        available: {
          x: left,
          y: top,
          width: Math.max(
            0,
            (app?.clientWidth || view.innerWidth) - left - right,
          ),
          height: Math.max(
            0,
            (app?.clientHeight || view.innerHeight) - top - bottom,
          ),
        },
        safeInsets: { top, right, bottom, left },
        overlayInsetTop: number(
          view
            .getComputedStyle(root)
            .getPropertyValue("--viewport-overlay-inset-top"),
        ),
        hinge,
      };
      setGeometry((old) =>
        JSON.stringify(old) === JSON.stringify(next) ? old : next,
      );
    };
    const resize = new ResizeObserver(update);
    resize.observe(measureHost);
    resize.observe(document.documentElement);
    const preferences = new MutationObserver(update);
    preferences.observe(document.documentElement, {
      attributes: true,
      attributeFilter: [
        "style",
        "class",
        "data-viewport-short-landscape",
        "data-duo-proportions",
        "data-duo-device",
        "data-native-hinge",
        "data-viewport-top-strip",
      ],
    });
    view.addEventListener("resize", update);
    view.addEventListener("studio-native-viewport", update);
    update();
    return () => {
      resize.disconnect();
      preferences.disconnect();
      view.removeEventListener("resize", update);
      view.removeEventListener("studio-native-viewport", update);
    };
  }, [host]);
  const { panePath, ...layoutOptions } = options;
  return {
    rowRef,
    row: host,
    duo: geometry.duo,
    shortTouchLandscape: geometry.shortTouchLandscape,
    ...deriveWorkspaceLayout({
      ...geometry,
      ...layoutOptions,
      ...(panePath
        ? {
            panePath: {
              ...panePath,
              primaryCoarse: geometry.primaryCoarse,
              duoDevice: geometry.duoDevice,
              hinge: geometry.hinge,
              available: geometry.available,
            },
          }
        : {}),
      hostExcludesSafeInsets: true,
    }),
  };
}
