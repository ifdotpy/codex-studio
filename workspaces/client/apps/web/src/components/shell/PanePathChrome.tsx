import { ActionIcon, Button } from "@mantine/core";
import { Maximize2, Minimize2, X } from "lucide-react";
import type { CSSProperties, ReactNode } from "react";
import type { LayoutRect } from "../../layout/deriveWorkspaceLayout";
import "./duo-pane-path.css";

export function panePathStyle(rect: LayoutRect | null): CSSProperties {
  return {
    position: "absolute",
    left: 0,
    top: rect?.y ?? 0,
    transform: `translateX(${rect?.x ?? 0}px)`,
    width: rect?.width ?? 0,
    height: rect?.height ?? 0,
  };
}
export function PanePathHeader(p: {
  title: ReactNode;
  fullScreen: boolean;
  canFullScreen?: boolean | undefined;
  onToggleFullScreen: () => void;
  onClose: () => void;
  actions?: ReactNode;
}) {
  return (
    <header className="duo-pane-header">
      <ActionIcon aria-label="Close side panel" onClick={p.onClose}>
        <X size={18} />
      </ActionIcon>
      <span>{p.title}</span>
      {p.canFullScreen !== false && (
        <Button
          variant="subtle"
          size="compact-sm"
          leftSection={
            p.fullScreen ? <Minimize2 size={16} /> : <Maximize2 size={16} />
          }
          onClick={p.onToggleFullScreen}
          aria-label={p.fullScreen ? "Restore split view" : "Full screen"}
        >
          {p.fullScreen ? "Split view" : "Full screen"}
        </Button>
      )}
      {p.actions}
    </header>
  );
}
