import {
  memo,
  useLayoutEffect,
  useMemo,
  useRef,
  type FormEvent,
  type HTMLAttributes,
} from "react";
import {
  ActionIcon,
  Button,
  Menu,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import {
  Archive,
  ArchiveRestore,
  Folder,
  FolderOpen,
  Mail,
  MoreHorizontal,
  Pencil,
  Pin,
  PinOff,
  Trash2,
} from "lucide-react";
import type { Agent } from "../types";
import {
  hasCompletedResult,
  type ChatIndicator,
} from "./chat-status/chatStatusModel";
import ChatStatus from "./agents/ChatStatus";
import { reportPromptComposerRender } from "./prompt-composer/renderProbe";

type Actions = {
  prepare: () => void;
  open: () => void;
  pin: () => void;
  rename: (event: FormEvent) => void;
  name: (value: string) => void;
  cancelRename: () => void;
  beginRename: () => void;
  unread: () => void;
  move: () => void;
  changeProject: () => void;
  archive: () => void;
  remove: () => void;
};
type Bindings = HTMLAttributes<HTMLElement> & {
  "data-folder-drop"?: string;
  "data-sidebar-group"?: string;
  "data-sidebar-id"?: string;
  "data-drop-edge"?: string;
};
type Props = {
  row: Agent;
  selected: boolean;
  indicator?: ChatIndicator;
  renaming: boolean;
  name: string;
  organizing: boolean;
  compact: boolean;
  markingRead: boolean;
  bindings: Bindings;
  actions: Actions;
};

// Commit the current handlers even when the unchanged row view skips its render.
export default function SidebarRow(props: Props) {
  const current = useRef(props);
  useLayoutEffect(() => {
    current.current = props;
  });
  const actions = useMemo<Actions>(
    () => ({
      prepare: () => current.current.actions.prepare(),
      open: () => current.current.actions.open(),
      pin: () => current.current.actions.pin(),
      rename: (event) => current.current.actions.rename(event),
      name: (value) => current.current.actions.name(value),
      cancelRename: () => current.current.actions.cancelRename(),
      beginRename: () => current.current.actions.beginRename(),
      unread: () => current.current.actions.unread(),
      move: () => current.current.actions.move(),
      changeProject: () => current.current.actions.changeProject(),
      archive: () => current.current.actions.archive(),
      remove: () => current.current.actions.remove(),
    }),
    [],
  );
  const events = useMemo<Bindings>(
    () => ({
      onDragStart: (event) => current.current.bindings.onDragStart?.(event),
      onDragOver: (event) => current.current.bindings.onDragOver?.(event),
      onDragLeave: (event) => current.current.bindings.onDragLeave?.(event),
      onDrop: (event) => current.current.bindings.onDrop?.(event),
      onDragEnd: (event) => current.current.bindings.onDragEnd?.(event),
      onKeyDown: (event) => current.current.bindings.onKeyDown?.(event),
    }),
    [],
  );
  const { draggable } = props.bindings;
  const folderDrop = props.bindings["data-folder-drop"];
  const sidebarGroup = props.bindings["data-sidebar-group"];
  const sidebarId = props.bindings["data-sidebar-id"];
  const dropEdge = props.bindings["data-drop-edge"];
  const bindingValues = useMemo<Bindings>(
    () => ({
      draggable,
      "data-folder-drop": folderDrop,
      "data-sidebar-group": sidebarGroup,
      "data-sidebar-id": sidebarId,
      "data-drop-edge": dropEdge,
    }),
    [draggable, folderDrop, sidebarGroup, sidebarId, dropEdge],
  );
  const indicator = useMemo(
    () => props.indicator,
    [props.indicator?.kind, props.indicator?.label],
  );
  return (
    <SidebarRowView
      row={props.row}
      selected={props.selected}
      indicator={indicator}
      renaming={props.renaming}
      name={props.name}
      organizing={props.organizing}
      compact={props.compact}
      markingRead={props.markingRead}
      bindingValues={bindingValues}
      events={events}
      actions={actions}
    />
  );
}

const SidebarRowView = memo(function SidebarRowView({
  row,
  selected,
  indicator,
  renaming,
  name,
  organizing,
  compact,
  markingRead,
  bindingValues,
  events,
  actions,
}: Omit<Props, "bindings"> & { bindingValues: Bindings; events: Bindings }) {
  reportPromptComposerRender("sidebar-row", row.id);
  const a = row;
  return (
    <div
      className={`sidebar-row lead-row ${selected ? "selected" : ""}`}
      data-sidebar-item={row.id}
    >
      <UnstyledButton
        {...bindingValues}
        {...events}
        title={row.name ?? undefined}
        aria-description="Drag onto a team to join it, or onto the project name to leave. Drop a team chat in the center of another lead chat to make it a subagent. Drag to an edge to reorder. Alt + Up or Down also works."
        className="chat-row"
        data-chat={row.id}
        onPointerEnter={() => actions.prepare()}
        onFocus={() => actions.prepare()}
        onPointerDown={() => actions.prepare()}
        onClick={() => actions.open()}
        aria-current={selected}
      >
        <span className="row-copy">
          <strong>
            {a?.pinned && <Pin size={11} className="chat-pin" />}
            {row.name ?? ""}
          </strong>
        </span>
        <ChatStatus
          status={indicator}
          provider={a.provider ?? undefined}
          model={a.model ?? undefined}
        />
      </UnstyledButton>
      {!renaming && (
        <ActionIcon
          className="row-pin-action"
          aria-label={`${row.pinned ? "Unpin" : "Pin"} ${row.name}`}
          title={row.pinned ? "Unpin chat" : "Pin chat"}
          disabled={organizing}
          onClick={() => actions.pin()}
        >
          {row.pinned ? <PinOff size={14} /> : <Pin size={14} />}
        </ActionIcon>
      )}
      {renaming ? (
        <form className="inline-rename" onSubmit={(e) => actions.rename(e)}>
          <TextInput
            aria-label="Chat name"
            value={name}
            onChange={(e) => actions.name(e.target.value)}
            autoFocus
            maxLength={80}
            onKeyDown={(e) => {
              if (e.key === "Escape") actions.cancelRename();
            }}
          />
          <Button type="submit" aria-label="Save name">
            Save
          </Button>
          <Button onClick={() => actions.cancelRename()}>Cancel</Button>
        </form>
      ) : (
        <Menu position="bottom-end" withinPortal shadow="lg" width={220}>
          <Menu.Target>
            <ActionIcon
              className="row-actions"
              aria-label={`Actions for ${row.name}`}
            >
              <MoreHorizontal size={16} />
            </ActionIcon>
          </Menu.Target>
          <Menu.Dropdown>
            <Menu.Item
              leftSection={<Pencil size={14} />}
              onClick={actions.beginRename}
            >
              Rename
            </Menu.Item>
            <Menu.Divider />
            <Menu.Item
              leftSection={<Mail size={14} />}
              aria-label="Mark unread"
              disabled={
                !a.readStateSupported || !hasCompletedResult(a) || markingRead
              }
              onClick={() => actions.unread()}
            >
              Mark unread
              {(!a.readStateSupported ||
                !hasCompletedResult(a) ||
                markingRead) && (
                <small className="menu-action-help">
                  {!a.readStateSupported
                    ? "Read status is unavailable for this chat."
                    : !hasCompletedResult(a)
                      ? "Available after an answer."
                      : "The read status is changing."}
                </small>
              )}
            </Menu.Item>
            {a && (
              <>
                <Menu.Item
                  leftSection={
                    a.pinned ? <PinOff size={14} /> : <Pin size={14} />
                  }
                  onClick={() => actions.pin()}
                >
                  {a.pinned ? "Unpin" : "Pin"}
                </Menu.Item>
                <Menu.Divider />
                {a.cwd && (
                  <Menu.Item
                    leftSection={<Folder size={14} />}
                    onClick={actions.move}
                  >
                    Move to folder
                  </Menu.Item>
                )}
                {!compact && (
                  <Menu.Item
                    leftSection={<FolderOpen size={14} />}
                    disabled={!!a.threadId || !!a.inFlight}
                    onClick={() => actions.changeProject()}
                  >
                    Change project directory
                  </Menu.Item>
                )}
                <Menu.Divider />
                <Menu.Item
                  leftSection={
                    a.archived ? (
                      <ArchiveRestore size={14} />
                    ) : (
                      <Archive size={14} />
                    )
                  }
                  disabled={!!a.inFlight}
                  onClick={() => actions.archive()}
                >
                  {a.archived ? "Restore chat" : "Archive"}
                </Menu.Item>
                <Menu.Divider />
              </>
            )}
            <Menu.Item
              color="red"
              leftSection={<Trash2 size={14} />}
              onClick={() => actions.remove()}
            >
              Delete
            </Menu.Item>
          </Menu.Dropdown>
        </Menu>
      )}
    </div>
  );
});
