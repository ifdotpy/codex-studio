import type { CSSProperties, ReactNode, Ref } from "react";
import { useLayoutEffect, useRef } from "react";
import { createPortal } from "react-dom";
import { ActionIcon, Menu, Tooltip } from "@mantine/core";
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
  useLayoutEffect(() => {
    const focused = document.activeElement;
    if (focused instanceof HTMLElement && railRef.current?.contains(focused)) {
      focused.scrollIntoView({ block: "nearest", inline: "nearest" });
    }
  }, [geometry?.height, geometry?.side, geometry?.top]);
  if (!geometry) return null;
  const position = geometry.side === "right" ? "left" : "right";
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
          offset={(geometry.width - geometry.targetSize) / 2}
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
              ref={menuButtonRef}
              className="edge-rail-button"
              aria-label="Chat actions"
              title="Chat actions"
            >
              <MoreHorizontal size={20} />
            </ActionIcon>
          </Menu.Target>
          <Menu.Dropdown
            style={{ maxHeight: geometry.height, overflowY: "auto" }}
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
