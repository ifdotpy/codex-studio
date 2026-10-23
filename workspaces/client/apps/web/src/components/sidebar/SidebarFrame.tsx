import { Drawer } from "@mantine/core";
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { layoutValues } from "../../layout/layout-contract.generated";
import { resolveSidebarWidth, storedSidebarWidth } from "./sidebarWidth";
import { useSidebarResize } from "./useSidebarResize";
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
  drawerZIndex?: number;
};

export function SidebarFrame(p: SidebarFrameProps) {
  const aside = useRef<HTMLElement | null>(null);
  const headerMinimum = useRef(0);
  const scrollTop = useRef(0);
  const [geometry, setGeometry] = useState(() => ({
    hostWidth: 0,
    rootFontSize: 0,
    headerMinimum: 0,
  }));
  const [preference, setPreference] = useState(() => ({
    scope: p.scope,
    width: readWidth(p.scope),
  }));
  const preferred =
    preference.scope === p.scope ? preference.width : readWidth(p.scope);
  const policy = resolveSidebarWidth({
    ...geometry,
    preferred,
    contract: widthContract,
  });
  const compact = policy.mode === "drawer";
  const live = useRef({ scope: p.scope, compact });
  live.current = { scope: p.scope, compact };

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
      if (header && aside.current) {
        const style = getComputedStyle(header);
        const sideStyle = getComputedStyle(aside.current);
        const children = Array.from(header.children) as HTMLElement[];
        headerMinimum.current =
          children.reduce(
            (width, child) => width + child.getBoundingClientRect().width,
            0,
          ) +
          Math.max(0, children.length - 1) *
            (parseFloat(style.columnGap) || 0) +
          parseFloat(sideStyle.paddingLeft) +
          parseFloat(sideStyle.paddingRight) +
          parseFloat(sideStyle.borderLeftWidth) +
          parseFloat(sideStyle.borderRightWidth);
      }
      const next = {
        hostWidth,
        rootFontSize,
        headerMinimum: headerMinimum.current,
      };
      const nextCompact =
        resolveSidebarWidth({
          ...next,
          preferred: null,
          contract: widthContract,
        }).mode === "drawer";
      if (nextCompact !== live.current.compact) restoreSidebarFocus();
      setGeometry((old) =>
        old.hostWidth === hostWidth &&
        old.rootFontSize === rootFontSize &&
        old.headerMinimum === next.headerMinimum
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
      attributeFilter: ["style", "class", "data-studio-typography"],
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
    if (live.current.scope !== p.scope) return;
    setPreference({ scope: p.scope, width });
    try {
      if (width === null)
        localStorage.removeItem(sidebarWidthStorageKey(p.scope));
      else
        localStorage.setItem(
          sidebarWidthStorageKey(p.scope),
          JSON.stringify(width),
        );
    } catch {
      // The current window retains the preference when browser storage is unavailable.
    }
  };
  const resize = useSidebarResize({
    scope: p.scope,
    enabled: !compact && !p.collapsed,
    width: policy.width,
    minimum: policy.minimum,
    maximum: policy.maximum,
    step: layoutValues.resize.step * geometry.rootFontSize,
    reset: () => save(null),
    commit: save,
  });
  const close = () => {
    restoreSidebarFocus();
    p.close();
  };
  const content = (
    <aside
      ref={aside}
      id="sidebar"
      className="sidebar-frame"
      data-presentation={compact ? "drawer" : "inline"}
      aria-label="Conversations"
      style={
        compact ? undefined : { width: resize.width, flexBasis: resize.width }
      }
    >
      {p.children}
      {!compact && (
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
  if (!compact) return p.collapsed ? null : content;
  return (
    <Drawer
      opened={p.opened}
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
