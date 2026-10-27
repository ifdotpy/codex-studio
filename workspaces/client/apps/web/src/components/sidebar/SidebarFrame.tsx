import { Drawer } from "@mantine/core";
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import {
  layoutQueries,
  layoutValues,
} from "../../layout/layout-contract.generated";
import type { LayoutRect } from "../../layout/deriveWorkspaceLayout";
import { resolveSidebarWidth, storedSidebarWidth } from "./sidebarWidth";
import { useSidebarResize } from "./useSidebarResize";
import { useDuoSurface } from "../../layout/useDuoSurface";
import "./sidebar-frame.css";

const widthContract = {
  minimumRem: layoutValues.sidebar.min,
  preferredRem: layoutValues.sidebar.preferred,
  conversationMinRem: layoutValues.conversation.min,
  navigationInlineMinPx: layoutValues.navigation.inlineMin,
};

export const sidebarWidthStorageKey = (scope: string) =>
  `studio-sidebar-width:${scope}`;

function readWidth(scope: string): number | null {
  try {
    return storedSidebarWidth(
      JSON.parse(localStorage.getItem(sidebarWidthStorageKey(scope)) || "null"),
    );
  } catch {
    return null;
  }
}

/** Move focus to the visible control before removing sidebar content. */
export function restoreSidebarFocus() {
  const sidebar = document.getElementById("sidebar");
  if (!sidebar?.contains(document.activeElement)) return;
  document
    .querySelector<HTMLElement>("#sidebar-toggle, .server-mobile-toggle")
    ?.focus({ preventScroll: true });
}

export type SidebarFrameProps = {
  scope: string;
  opened: boolean;
  collapsed?: boolean;
  close: () => void;
  onCompactChange?: (compact: boolean) => void;
  children: ReactNode;
  paneRect?: LayoutRect | null | undefined;
  drawerZIndex?: number;
  onEntered?: () => void;
};

export function SidebarFrame(p: SidebarFrameProps) {
  const duoSurface = useDuoSurface();
  const aside = useRef<HTMLElement | null>(null);
  const headerMinimum = useRef(0);
  const scrollTop = useRef(0);
  const [geometry, setGeometry] = useState(() => ({
    hostWidth: 0,
    rootFontSize: 0,
    headerMinimum: 0,
    duo: false,
  }));
  const [preference, setPreference] = useState(() => ({
    scope: p.scope,
    width: readWidth(p.scope),
  }));
  // A desktop width constrained by the old Duo maximum is not a Duo choice.
  const scope = geometry.duo ? `${p.scope}:duo` : p.scope;
  const preferred =
    preference.scope === scope ? preference.width : readWidth(scope);
  const policy = resolveSidebarWidth({
    ...geometry,
    preferred,
    contract: {
      ...widthContract,
      conversationMinRem: geometry.duo
        ? layoutValues.duo.conversationMin
        : widthContract.conversationMinRem,
    },
  });
  const inPath = p.paneRect !== undefined;
  const compact = !inPath && policy.mode === "drawer";
  // Measure once before paint when the saved desktop preference starts closed.
  const probing =
    !inPath && !compact && p.collapsed && headerMinimum.current === 0;
  const live = useRef({ scope, compact });
  live.current = { scope, compact };

  useLayoutEffect(() => {
    const host =
      document.querySelector<HTMLElement>(".multi-server-shell") ||
      document.getElementById("root");
    if (!host) return;
    const measure = () => {
      const rootFontSize = Number.parseFloat(
        getComputedStyle(document.documentElement).fontSize,
      );
      const hostStyle = getComputedStyle(host);
      const hostWidth =
        host.clientWidth -
        parseFloat(hostStyle.paddingLeft) -
        parseFloat(hostStyle.paddingRight);
      const header =
        aside.current?.querySelector<HTMLElement>(".sidebar-header");
      // Drawer touch controls must not raise the desktop header minimum.
      if (
        header &&
        aside.current &&
        !compact &&
        window.matchMedia(layoutQueries.navigationInline).matches
      ) {
        const style = getComputedStyle(header);
        const sideStyle = getComputedStyle(aside.current);
        const children = Array.from(header.children) as HTMLElement[];
        const minimumWidths = children.map((child) => {
          if (Number(getComputedStyle(child).flexGrow) === 0)
            return child.getBoundingClientRect().width;
          // A flexible label can wrap; its expanded width is not its minimum.
          const previous = child.style.cssText;
          try {
            child.style.flex = "none";
            child.style.inlineSize = "min-content";
            return child.getBoundingClientRect().width;
          } finally {
            child.style.cssText = previous;
          }
        });
        const contentMinimum =
          style.flexWrap === "nowrap"
            ? minimumWidths.reduce((sum, width) => sum + width, 0) +
              Math.max(0, children.length - 1) *
                (parseFloat(style.columnGap) || 0)
            : Math.max(0, ...minimumWidths);
        headerMinimum.current =
          contentMinimum +
          parseFloat(sideStyle.paddingLeft) +
          parseFloat(sideStyle.paddingRight) +
          parseFloat(sideStyle.borderLeftWidth) +
          parseFloat(sideStyle.borderRightWidth);
      }
      const next = {
        hostWidth,
        rootFontSize,
        headerMinimum: headerMinimum.current,
        duo: document.documentElement.hasAttribute("data-duo-proportions"),
      };
      const nextCompact =
        resolveSidebarWidth({
          ...next,
          preferred: null,
          contract: {
            ...widthContract,
            conversationMinRem: next.duo
              ? layoutValues.duo.conversationMin
              : widthContract.conversationMinRem,
          },
        }).mode === "drawer";
      if (nextCompact !== live.current.compact) restoreSidebarFocus();
      setGeometry((old) =>
        old.hostWidth === hostWidth &&
        old.rootFontSize === rootFontSize &&
        old.headerMinimum === next.headerMinimum &&
        old.duo === next.duo
          ? old
          : next,
      );
    };
    const observer = new ResizeObserver(measure);
    observer.observe(host);
    if (aside.current) {
      const header = aside.current.querySelector(".sidebar-header");
      if (header) {
        observer.observe(header);
        for (const child of header.children) observer.observe(child);
      }
    }
    const attributes = new MutationObserver(measure);
    attributes.observe(document.documentElement, {
      attributes: true,
      attributeFilter: [
        "style",
        "class",
        "data-studio-typography",
        "data-duo-proportions",
      ],
    });
    attributes.observe(document.body, {
      attributes: true,
      attributeFilter: ["style", "class"],
    });
    document.fonts?.addEventListener("loadingdone", measure);
    window.addEventListener("resize", measure);
    measure();
    return () => {
      observer.disconnect();
      attributes.disconnect();
      document.fonts?.removeEventListener("loadingdone", measure);
      window.removeEventListener("resize", measure);
    };
  }, [compact, p.collapsed, p.opened]);

  useLayoutEffect(() => {
    p.onCompactChange?.(compact);
  }, [compact, p.onCompactChange]);

  useLayoutEffect(() => {
    const list = aside.current?.querySelector<HTMLElement>("#chat-list");
    if (list) list.scrollTop = scrollTop.current;
    return () => {
      if (list) scrollTop.current = list.scrollTop;
      restoreSidebarFocus();
    };
  }, [compact, p.collapsed, p.opened]);

  const save = (width: number | null) => {
    if (live.current.scope !== scope) return;
    if (width !== null && storedSidebarWidth(width) === null) return;
    setPreference({ scope, width });
    try {
      if (width === null)
        localStorage.removeItem(sidebarWidthStorageKey(scope));
      else
        localStorage.setItem(
          sidebarWidthStorageKey(scope),
          JSON.stringify(width),
        );
    } catch {
      // The current window retains the preference when browser storage is unavailable.
    }
  };
  const resize = useSidebarResize({
    scope,
    enabled: !inPath && !compact && !p.collapsed,
    width: policy.width,
    minimum: policy.minimum,
    maximum: policy.maximum,
    step: layoutValues.resize.step * geometry.rootFontSize,
    reset: () => save(null),
    commit: save,
  });
  useLayoutEffect(() => {
    if (inPath || compact || p.collapsed) return;
    const root = document.documentElement;
    const previous = root.style.getPropertyValue("--studio-sidebar-width");
    root.style.setProperty("--studio-sidebar-width", `${resize.width}px`);
    return () => {
      if (previous) root.style.setProperty("--studio-sidebar-width", previous);
      else root.style.removeProperty("--studio-sidebar-width");
    };
  }, [resize.width, compact, p.collapsed, inPath]);
  const close = () => {
    restoreSidebarFocus();
    p.close();
  };
  const content = (
    <aside
      ref={aside}
      id="sidebar"
      className="sidebar-frame"
      tabIndex={compact && duoSurface ? -1 : undefined}
      data-autofocus={compact && duoSurface ? true : undefined}
      data-presentation={inPath ? "path" : compact ? "drawer" : "inline"}
      hidden={inPath && !p.paneRect}
      data-probe={probing || undefined}
      inert={probing || undefined}
      aria-hidden={probing || undefined}
      aria-label="Conversations"
      style={
        inPath
          ? {
              position: "absolute",
              left: 0,
              top: p.paneRect?.y ?? 0,
              transform: `translateX(${p.paneRect?.x ?? 0}px)`,
              width: p.paneRect?.width ?? 0,
              height: p.paneRect?.height ?? 0,
            }
          : compact
            ? undefined
            : { width: resize.width, flexBasis: resize.width }
      }
    >
      {p.children}
      {!inPath && !compact && (
        <div
          className="sidebar-resize-rail"
          role="separator"
          tabIndex={0}
          aria-label="Conversation sidebar width"
          aria-orientation="vertical"
          aria-controls="sidebar"
          aria-valuemin={Math.ceil(policy.minimum)}
          aria-valuemax={Math.floor(policy.maximum)}
          aria-valuenow={Math.round(resize.width)}
          aria-valuetext={`${Math.round(resize.width)} pixels`}
          title="Drag or use arrow keys to resize. Press Home to reset."
          {...resize.handlers}
        />
      )}
    </aside>
  );
  if (inPath) return content;
  if (!compact) return !inPath && p.collapsed && !probing ? null : content;
  return (
    <Drawer
      opened={p.opened}
      returnFocus
      onEnterTransitionEnd={() => {
        if (duoSurface) aside.current?.focus({ preventScroll: true });
        else p.onEntered?.();
      }}
      onClose={close}
      zIndex={p.drawerZIndex}
      size={`min(${layoutValues.sidebar.drawerMax}rem, 100vw)`}
      padding={0}
      withCloseButton={false}
      title="Conversations"
      classNames={{ header: "sr-only", body: "sidebar-drawer-body" }}
    >
      {content}
    </Drawer>
  );
}
