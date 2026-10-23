import type { CSSProperties, ReactNode, Ref } from "react";
import { useLayoutEffect, useRef } from "react";
import { ActionIcon, Menu, Tooltip } from "@mantine/core";
import {
  Activity,
  FileDiff,
  MessageSquare,
  MoreHorizontal,
  Settings2,
  Terminal,
  Users,
} from "lucide-react";
import type { EdgeRailGeometry } from "./edgeRailPolicy";
import "./edge-rail.css";

interface RailAction {
  onClick: () => void;
  disabled?: boolean;
  expanded?: boolean;
}

export interface EdgeRailProps {
  geometry: EdgeRailGeometry | null;
  team?: RailAction & { busy: number; total: number };
  messages?: RailAction & { count: number };
  onChanges?: () => void;
  onTasks?: () => void;
  onSettings?: () => void;
  terminal?: RailAction;
  /** Existing Menu.Item children retain their handlers and permissions. */
  menu?: ReactNode;
  menuButtonRef?: Ref<HTMLButtonElement>;
}

export function EdgeRail({
  geometry,
  team,
  messages,
  onChanges,
  onTasks,
  onSettings,
  terminal,
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
  return (
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
      {team &&
        button(
          `Team, ${team.busy} busy of ${team.total}`,
          <Users size={20} />,
          team,
          `${team.busy}/${team.total}`,
        )}
      {messages &&
        button(
          messages.count > 0
            ? `Messages, ${messages.count} need you`
            : "Messages",
          <MessageSquare size={20} />,
          messages,
          messages.count > 0 ? messages.count : undefined,
        )}
      {onChanges &&
        button("Changes", <FileDiff size={20} />, { onClick: onChanges })}
      {onTasks && button("Tasks", <Activity size={20} />, { onClick: onTasks })}
      {onSettings &&
        button("Chat settings", <Settings2 size={20} />, {
          onClick: onSettings,
        })}
      {terminal &&
        button(
          terminal.expanded == null
            ? "Terminals"
            : terminal.expanded
              ? "Hide terminals"
              : "Show terminals",
          <Terminal size={20} />,
          terminal,
        )}
      {menu != null && (
        <Menu position={`${position}-start`} withinPortal>
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
          <Menu.Dropdown>{menu}</Menu.Dropdown>
        </Menu>
      )}
    </nav>
  );
}
