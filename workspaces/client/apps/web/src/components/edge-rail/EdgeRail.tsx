import type { CSSProperties, ReactNode, Ref } from "react";
import { useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { ActionIcon, Menu, Tooltip } from "@mantine/core";
import { useMergedRef } from "@mantine/hooks";
import { layoutValues } from "../../layout/layout-contract.generated";
import { MessageSquare, MoreHorizontal, PanelLeft } from "lucide-react";
import type { EdgeRailGeometry } from "./edgeRailPolicy";
import "./edge-rail.css";

interface RailAction {
  onClick: () => void;
  id?: string;
  disabled?: boolean;
  expanded?: boolean;
}

export interface EdgeRailProps {
  geometry: EdgeRailGeometry | null;
  sidebar: RailAction & { label?: string };
  messages?: RailAction & { count: number };
  /** Existing Menu.Item children retain their handlers and permissions. */
  menu?: ReactNode;
  menuButtonRef?: Ref<HTMLButtonElement>;
}

export function EdgeRail({
  geometry,
  sidebar,
  messages,
  menu,
  menuButtonRef,
}: EdgeRailProps) {
  const railRef = useRef<HTMLElement>(null);
  const menuButton = useRef<HTMLButtonElement>(null);
  const menuTargetRef = useMergedRef(menuButton, menuButtonRef);
  const [menuHeight, setMenuHeight] = useState<number>();
  useLayoutEffect(() => {
    const focused = document.activeElement;
    if (focused instanceof HTMLElement && railRef.current?.contains(focused)) {
      focused.scrollIntoView({ block: "nearest", inline: "nearest" });
    }
  }, [geometry?.height, geometry?.side, geometry?.top]);
  if (!geometry) return null;
  const position = geometry.side === "right" ? "left" : "right";
  // The button column centers on the system status column, so it sits
  // closer to the screen edge than the strip center.
  const outerPadding = geometry.center - geometry.targetSize / 2;
  const innerGap = geometry.width - geometry.center - geometry.targetSize / 2;
  const railBottom = geometry.top + geometry.height;
  // The menu starts at its button and ends at the rail bottom (above Home);
  // longer menus scroll inside.
  const measureMenu = () => {
    const top = menuButton.current?.getBoundingClientRect().top;
    const root = document.documentElement;
    const offsetTop = parseFloat(
      getComputedStyle(root).getPropertyValue("--mobile-viewport-top"),
    );
    if (top != null)
      setMenuHeight(
        Math.max(
          geometry.targetSize,
          railBottom + (Number.isFinite(offsetTop) ? offsetTop : 0) - top,
        ),
      );
  };
  function button(
    label: string,
    icon: ReactNode,
    action: RailAction,
    badge?: ReactNode,
  ) {
    return (
      <Tooltip label={label} position={position} withinPortal>
        <ActionIcon
          id={action.id}
          className="edge-rail-button"
          aria-label={label}
          title={label}
          aria-expanded={action.expanded}
          disabled={action.disabled}
          onClick={action.onClick}
        >
          {icon}
          {badge != null && (
            <span className="edge-rail-badge" aria-hidden="true">
              {badge}
            </span>
          )}
        </ActionIcon>
      </Tooltip>
    );
  }
  const rail = (
    <nav
      ref={railRef}
      className="edge-rail"
      aria-label="Conversation actions"
      data-side={geometry.side}
      style={
        {
          width: geometry.width,
          top: `calc(var(--mobile-viewport-top, 0px) + ${geometry.top}px)`,
          height: geometry.height,
          [geometry.side]: 0,
          [geometry.side === "right" ? "paddingRight" : "paddingLeft"]:
            outerPadding,
          alignItems: geometry.side === "right" ? "flex-end" : "flex-start",
          "--edge-rail-target": `${geometry.targetSize}px`,
        } as CSSProperties
      }
    >
      {button(
        sidebar.label || "Toggle conversations",
        <PanelLeft size={20} />,
        sidebar,
      )}
      {messages &&
        messages.count > 0 &&
        button(
          messages.count > 0
            ? `Messages, ${messages.count} need you`
            : "Messages",
          <MessageSquare size={20} />,
          messages,
          messages.count > 0 ? messages.count : undefined,
        )}
      {menu != null && (
        <Menu
          withInitialFocusPlaceholder={false}
          position={`${position}-start`}
          withinPortal
          offset={innerGap + layoutValues.edgeRail.menuGap}
          onOpen={measureMenu}
          middlewares={{
            flip: false,
            shift: {
              padding: {
                top: geometry.top,
                bottom: geometry.bottom,
                left: geometry.side === "left" ? geometry.width : 0,
                right: geometry.side === "right" ? geometry.width : 0,
              },
            },
          }}
        >
          <Menu.Target>
            <ActionIcon
              ref={menuTargetRef}
              className="edge-rail-button"
              aria-label="Chat actions"
              title="Chat actions"
            >
              <MoreHorizontal size={20} />
            </ActionIcon>
          </Menu.Target>
          <Menu.Dropdown
            className="edge-rail-menu"
            style={{
              maxHeight: menuHeight ?? geometry.height,
              overflowY: "auto",
            }}
          >
            {menu}
          </Menu.Dropdown>
        </Menu>
      )}
    </nav>
  );
  // The fixed viewport root creates a stacking context below portal drawers.
  return typeof document === "undefined"
    ? rail
    : createPortal(rail, document.body);
}
