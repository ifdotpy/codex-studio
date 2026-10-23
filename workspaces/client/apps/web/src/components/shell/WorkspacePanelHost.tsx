import { ActionIcon, Drawer } from "@mantine/core";
import { X } from "lucide-react";
import { createPortal } from "react-dom";
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { WorkspaceSurface } from "./WorkspaceSurface";
import { panelWidthAfterResize } from "./workspacePanelWidth";
import {
  resolveLayoutLength,
  type PanelLayout,
} from "../../layout/deriveWorkspaceLayout";
import "./WorkspacePanelHost.css";

export type WorkspacePanelPresentationProps = {
  inlineTarget?: HTMLElement | null;
  panelLayout?: PanelLayout;
  onPanelWidthChange?: (widthRem: number) => void;
  resizeScope?: string;
};

type Props = WorkspacePanelPresentationProps & {
  opened: boolean;
  onClose: () => void;
  title: ReactNode;
  actions?: ReactNode;
  drawerSize: string;
  className?: string;
  children: ReactNode;
};

export function WorkspacePanelHost({
  inlineTarget,
  panelLayout,
  onPanelWidthChange,
  resizeScope,
  opened,
  onClose,
  title,
  actions,
  drawerSize,
  className = "",
  children,
}: Props) {
  const [container] = useState(() => {
    const node = document.createElement("div");
    node.className = "workspace-panel-surface-host";
    node.tabIndex = -1;
    node.setAttribute("data-autofocus", "");
    return node;
  });
  const [drawerBody, setDrawerBody] = useState<HTMLDivElement | null>(null);
  const inlineBody = useRef<HTMLDivElement>(null);
  const parked = useRef<HTMLDivElement>(null);
  const inline = panelLayout?.mode === "inline" && !!inlineTarget;
  const inlineWidth = panelLayout?.width ?? 0;
  const inlineMinWidth = panelLayout?.minWidth ?? 0;
  const inlineMaxWidth = panelLayout?.maxWidth ?? 0;
  const [drag, setDrag] = useState<{
    x: number;
    width: number;
    next: number;
  } | null>(null);
  const width = drag?.next ?? inlineWidth;
  const [placement, setPlacement] = useState<"inline" | "drawer" | "parked">(
    "parked",
  );
  const lastSurfaceFocus = useRef<HTMLElement | null>(null);
  const pendingFocus = useRef<HTMLElement | null>(null);
  const modalFocus = useRef<HTMLElement | null>(null);
  const returnFocus = useRef<HTMLElement | null>(null);
  const previous = useRef({ inline, opened: false });
  useLayoutEffect(() => {
    const rememberFocus = () => {
      const active = document.activeElement;
      if (!(active instanceof HTMLElement) || active === document.body) return;
      if (container.contains(active) && active !== container)
        lastSurfaceFocus.current = active;
      else if (!active.closest(".workspace-drawer, .workspace-panel-inline")) {
        lastSurfaceFocus.current = null;
        returnFocus.current = active;
      }
    };
    rememberFocus();
    const restoreSurfaceFocus = (event: FocusEvent) => {
      if (event.target !== container) return;
      const focus = modalFocus.current || lastSurfaceFocus.current;
      if (focus?.isConnected) focus.focus({ preventScroll: true });
    };
    container.addEventListener("focus", restoreSurfaceFocus);
    document.addEventListener("focusin", rememberFocus);
    return () => {
      container.removeEventListener("focus", restoreSurfaceFocus);
      document.removeEventListener("focusin", rememberFocus);
    };
  }, [container]);

  useLayoutEffect(() => {
    setDrag(null);
  }, [
    inline,
    opened,
    resizeScope,
    inlineTarget,
    inlineMinWidth,
    inlineMaxWidth,
  ]);

  useLayoutEffect(() => {
    const target = !opened
      ? parked.current
      : inline
        ? inlineBody.current
        : drawerBody;
    if (!target || container.parentElement === target) return;
    const active = document.activeElement;
    modalFocus.current =
      active instanceof HTMLElement &&
      active.closest('[role="dialog"]') &&
      !active.closest(".workspace-drawer")
        ? active
        : null;
    const focus =
      active instanceof HTMLElement && container.contains(active)
        ? active
        : active === document.body
          ? lastSurfaceFocus.current
          : null;
    const scroll = [container, ...container.querySelectorAll<HTMLElement>("*")]
      .filter((node) => node.scrollTop || node.scrollLeft)
      .map((node) => ({ node, top: node.scrollTop, left: node.scrollLeft }));
    // Move the DOM container, not the React portal destination. Child state stays mounted.
    target.append(container);
    setPlacement(!opened ? "parked" : inline ? "inline" : "drawer");
    pendingFocus.current = opened ? focus : null;
    if (focus && opened) focus.focus({ preventScroll: true });
    for (const { node, top, left } of scroll) {
      node.scrollTop = top;
      node.scrollLeft = left;
    }
    if (previous.current.opened && !opened && returnFocus.current?.isConnected)
      returnFocus.current.focus({ preventScroll: true });
    previous.current = { inline, opened };
  }, [container, inline, inlineTarget, drawerBody, opened]);

  useLayoutEffect(() => {
    if (opened && pendingFocus.current?.isConnected) {
      pendingFocus.current.focus({ preventScroll: true });
      pendingFocus.current = null;
    }
  }, [opened, placement]);
  useLayoutEffect(() => () => container.remove(), [container]);
  const header = <span className="workspace-drawer-title">{title}</span>;
  const commit = (value: number | undefined) => {
    if (value !== undefined && panelLayout && panelLayout.rootFontSize > 0)
      onPanelWidthChange?.(value / panelLayout.rootFontSize);
    setDrag(null);
  };
  return (
    <>
      <div ref={parked} hidden />
      {inlineTarget &&
        createPortal(
          <aside
            className="workspace-panel-inline"
            aria-label="Workspace"
            hidden={placement !== "inline"}
            style={{ width, flexBasis: width }}
          >
            <div
              role="separator"
              aria-label="Workspace panel width"
              aria-orientation="vertical"
              tabIndex={0}
              aria-valuemin={inlineMinWidth}
              aria-valuemax={inlineMaxWidth}
              aria-valuenow={width}
              className="workspace-panel-resize"
              onPointerDown={(event) => {
                if (event.button !== 0) return;
                event.preventDefault();
                event.currentTarget.setPointerCapture(event.pointerId);
                setDrag({
                  x: event.clientX,
                  width: inlineWidth,
                  next: inlineWidth,
                });
              }}
              onPointerMove={(event) => {
                if (!drag) return;
                const next = panelWidthAfterResize(
                  drag.width,
                  drag.x,
                  event.clientX,
                  inlineMinWidth,
                  inlineMaxWidth,
                );
                if (next !== undefined) setDrag({ ...drag, next });
              }}
              onPointerUp={(event) => {
                if (drag)
                  commit(
                    panelWidthAfterResize(
                      drag.width,
                      drag.x,
                      event.clientX,
                      inlineMinWidth,
                      inlineMaxWidth,
                    ),
                  );
              }}
              onPointerCancel={() => setDrag(null)}
              onLostPointerCapture={() => setDrag(null)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  setDrag(null);
                  return;
                }
                const step = panelLayout
                  ? resolveLayoutLength("resize.step", panelLayout.rootFontSize)
                  : 0;
                const delta =
                  event.key === "ArrowLeft"
                    ? step
                    : event.key === "ArrowRight"
                      ? -step
                      : 0;
                if (event.key === "Home" || event.key === "End" || delta) {
                  event.preventDefault();
                  commit(
                    panelWidthAfterResize(
                      event.key === "Home"
                        ? inlineMinWidth
                        : event.key === "End"
                          ? inlineMaxWidth
                          : inlineWidth,
                      0,
                      -delta,
                      inlineMinWidth,
                      inlineMaxWidth,
                    ),
                  );
                }
              }}
            />
            <header className="workspace-panel-header">
              {header}
              {actions}
              <ActionIcon variant="subtle" aria-label="Close" onClick={onClose}>
                <X size={18} />
              </ActionIcon>
            </header>
            <div ref={inlineBody} className="workspace-panel-body" />
          </aside>,
          inlineTarget,
        )}
      <Drawer.Root
        opened={opened && !inline}
        onClose={onClose}
        position="right"
        size={drawerSize}
        keepMounted
        trapFocus={opened && !inline}
        returnFocus={false}
        className={`workspace-drawer ${className}`}
      >
        <Drawer.Overlay />
        <Drawer.Content>
          <Drawer.Header>
            <Drawer.Title>{header}</Drawer.Title>
            {actions}
            <Drawer.CloseButton aria-label="Close" />
          </Drawer.Header>
          <Drawer.Body>
            <div ref={setDrawerBody} className="workspace-panel-body" />
          </Drawer.Body>
        </Drawer.Content>
      </Drawer.Root>
      {createPortal(<WorkspaceSurface>{children}</WorkspaceSurface>, container)}
    </>
  );
}
