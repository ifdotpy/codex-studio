import { useEffect, useState } from "react";
import {
  ActionIcon,
  Button,
  Drawer,
  Menu,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import {
  Archive,
  Pin,
  PinOff,
  ArchiveRestore,
  BookOpen,
  Check,
  FolderOpen,
  MoreHorizontal,
  Pencil,
  Search,
  Folder,
  ChevronDown,
  Plus,
  Trash2,
  X,
} from "lucide-react";
import { api, errorText, save, saved } from "../api";
import "./sidebar-projects.css";
import { useSidebarOrder } from "./useSidebarOrder";
import {
  busy,
  complaintNeedsUserResponse,
  type Agent,
  type Snapshot,
} from "../types";
type Props = {
  data: Snapshot;
  opened: string | null;
  lead?: Agent;
  view: string;
  open: (id: string) => void;
  newChat: (path?: string) => void;
  addProject: () => void;
  changeProject: (agent: Agent) => void;
  creating: boolean;
  complaints: () => void;
  rename: (id: string, name: string) => Promise<void>;
  remove: (id: string, room: boolean) => void;
  mobile: boolean;
  close: () => void;
  refresh?: () => Promise<void>;
  notify?: (message: string) => void;
};
export default function Sidebar(p: Props) {
  const compact = useMediaQuery("(max-width: 760px)");
  const [query, setQuery] = useState(""),
    [renaming, setRenaming] = useState<string | null>(null),
    [name, setName] = useState(""),
    [archive, setArchive] = useState(false),
    [projectsOpen, setProjectsOpen] = useState(true);
  const sorting = useSidebarOrder(
    `codex-sidebar-order:${p.data.stateDir}`,
    p.notify,
  );
  const [organizing, setOrganizing] = useState<string | null>(null);
  const projectKey = `codex-project-tree:${p.data.stateDir}`;
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>(() =>
    saved(projectKey, {}),
  );
  const [projectLimits, setProjectLimits] = useState<Record<string, number>>(
    {},
  );
  const toggleProject = (path: string) => {
    const next = { ...collapsed, [path]: !collapsed[path] };
    setCollapsed(next);
    save(projectKey, next);
  };
  const organize = async (id: string, data: Record<string, unknown>) => {
    if (organizing) return;
    setOrganizing(id);
    try {
      await api("/api/organization", { id, ...data });
      await p.refresh?.();
    } catch (error) {
      p.notify?.(errorText(error));
    } finally {
      setOrganizing(null);
    }
  };
  const agents = p.data.threads.filter(
    (a) => a.source === "managed" && a.isLead,
  );
  useEffect(() => {
    if (agents.some((a) => a.id === p.opened)) {
      setQuery("");
      setProjectsOpen(true);
      const path = agents.find((a) => a.id === p.opened)?.cwd;
      if (path && collapsed[path]) {
        const next = { ...collapsed, [path]: false };
        setCollapsed(next);
        save(projectKey, next);
      }
    }
  }, [p.opened]);
  const chatGroup = (a: Agent) =>
    JSON.stringify(["chats", a.cwd, !!a.pinned, !!a.archived]);
  const orderedAgents = [...agents].sort(
    (a, b) =>
      Number(!!b.pinned) - Number(!!a.pinned) ||
      sorting.rank(chatGroup(a), a.id) - sorting.rank(chatGroup(b), b.id) ||
      (("updated" in b ? b.updated : b.created) || 0) -
        (("updated" in a ? a.updated : a.created) || 0),
  );
  const filtered = orderedAgents
    .filter((row) => !!row.archived === archive)
    .filter((a) =>
      `${a.name} ${a.cwd || ""} ${a.project || ""} ${a.tail || ""}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    );
  const rename = async (e: React.FormEvent, id: string) => {
    e.preventDefault();
    try {
      await p.rename(id, name);
      setRenaming(null);
    } catch {
      /* Keep the draft when the server rejects the name. */
    }
  };
  const groupMap = new Map<
    string,
    {
      path: string;
      name: string;
      chats: Agent[];
      registered: boolean;
      hasChats: boolean;
    }
  >();
  for (const project of p.data.runtime.projects || [])
    groupMap.set(project.path, {
      path: project.path,
      name: project.name,
      chats: [],
      registered: true,
      hasChats: agents.some((a) => a.cwd === project.path),
    });
  for (const a of agents) {
    const path = a.cwd || "";
    if (!groupMap.has(path))
      groupMap.set(path, {
        path,
        name: path.split("/").filter(Boolean).at(-1) || "Other chats",
        chats: [],
        registered: false,
        hasChats: true,
      });
  }
  for (const a of filtered) groupMap.get(a.cwd || "")!.chats.push(a);
  const allProjectGroups = [...groupMap.values()].sort(
    (a, b) =>
      sorting.rank("projects", a.path) - sorting.rank("projects", b.path) ||
      a.name.localeCompare(b.name),
  );
  const projectGroups = allProjectGroups.filter(
    (group) =>
      !query ||
      group.chats.length ||
      `${group.path} ${group.name}`.toLowerCase().includes(query.toLowerCase()),
  );
  const renderRow = (row: Agent) => {
    const a = row;
    return (
      <div
        className={`sidebar-row lead-row ${p.opened === row.id && p.view === "chat" ? "selected" : ""}`}
        key={row.id}
      >
        <UnstyledButton
          {...sorting.bindings(
            chatGroup(row),
            row.id,
            orderedAgents
              .filter((a) => chatGroup(a) === chatGroup(row))
              .map((a) => a.id),
          )}
          title={row.name}
          aria-description="Drag to reorder. Alt + Up or Down moves this chat."
          className="chat-row"
          data-chat={row.id}
          onClick={() => p.open(row.id)}
          aria-current={p.opened === row.id}
        >
          <span className="row-copy">
            <strong>
              {a?.pinned && <Pin size={11} className="chat-pin" />}
              {row.name}
            </strong>
          </span>
          {busy.has(a.status) && (
            <span className="row-meta">
              <span className="dot running" />
            </span>
          )}
        </UnstyledButton>
        {renaming !== row.id && (
          <ActionIcon
            className="row-pin-action"
            aria-label={`${row.pinned ? "Unpin" : "Pin"} ${row.name}`}
            title={row.pinned ? "Unpin chat" : "Pin chat"}
            disabled={organizing === row.id}
            onClick={() => void organize(row.id, { pinned: !row.pinned })}
          >
            {row.pinned ? <PinOff size={14} /> : <Pin size={14} />}
          </ActionIcon>
        )}
        {renaming === row.id ? (
          <form
            className="inline-rename"
            onSubmit={(e) => void rename(e, row.id)}
          >
            <TextInput
              aria-label="Chat name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              autoFocus
              maxLength={80}
              onKeyDown={(e) => {
                if (e.key === "Escape") setRenaming(null);
              }}
            />
            <ActionIcon type="submit" aria-label="Save name">
              <Check size={16} />
            </ActionIcon>
          </form>
        ) : (
          <Menu position="bottom-end" withinPortal shadow="lg" width={170}>
            <Menu.Target>
              <ActionIcon
                className="row-actions"
                aria-label={`Actions for ${row.name}`}
              >
                <MoreHorizontal size={16} />
              </ActionIcon>
            </Menu.Target>
            <Menu.Dropdown>
              {a && (
                <>
                  <Menu.Item
                    leftSection={
                      a.pinned ? <PinOff size={14} /> : <Pin size={14} />
                    }
                    onClick={() => void organize(a.id, { pinned: !a.pinned })}
                  >
                    {a.pinned ? "Unpin" : "Pin"}
                  </Menu.Item>
                  {!compact && (
                    <Menu.Item
                      leftSection={<FolderOpen size={14} />}
                      disabled={!!a.threadId || !!a.inFlight}
                      onClick={() => p.changeProject(a)}
                    >
                      Change project folder
                    </Menu.Item>
                  )}
                  <Menu.Item
                    leftSection={
                      a.archived ? (
                        <ArchiveRestore size={14} />
                      ) : (
                        <Archive size={14} />
                      )
                    }
                    disabled={!!a.inFlight}
                    onClick={() =>
                      void organize(a.id, { archived: !a.archived })
                    }
                  >
                    {a.archived ? "Restore chat" : "Archive"}
                  </Menu.Item>
                  <Menu.Divider />
                </>
              )}
              <Menu.Item
                leftSection={<Pencil size={14} />}
                onClick={() => {
                  setName(row.name);
                  setRenaming(row.id);
                }}
              >
                Rename
              </Menu.Item>
              <Menu.Item
                color="red"
                leftSection={<Trash2 size={14} />}
                onClick={() => p.remove(row.id, false)}
              >
                Delete
              </Menu.Item>
            </Menu.Dropdown>
          </Menu>
        )}
      </div>
    );
  };
  const content = (
    <aside id="sidebar" aria-label="Conversations">
      <span className="sr-only" role="status">
        {sorting.announcement}
      </span>
      <div className="sidebar-header">
        <a className="brand" href="/">
          <span className="brand-name">
            <strong>Codex</strong> <span>Studio</span>
          </span>
        </a>
        {compact && (
          <ActionIcon aria-label="Close conversations" onClick={p.close}>
            <X size={20} />
          </ActionIcon>
        )}
      </div>
      <TextInput
        id="chat-search"
        type="search"
        placeholder="Search chats"
        aria-label="Search chats"
        leftSection={<Search size={15} />}
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
        }}
      />
      <div className="projects-heading">
        <UnstyledButton
          className="projects-heading-toggle"
          onClick={() => setProjectsOpen(!projectsOpen)}
          aria-expanded={projectsOpen}
        >
          {archive ? "Archived projects" : "Projects"}
          <ChevronDown size={14} />
        </UnstyledButton>
        <Menu position="bottom-end" withinPortal>
          <Menu.Target>
            <ActionIcon aria-label="Project list options">
              <MoreHorizontal size={16} />
            </ActionIcon>
          </Menu.Target>
          <Menu.Dropdown>
            <Menu.Item onClick={() => setArchive(!archive)}>
              {archive ? "Show active chats" : "Show archived chats"}
            </Menu.Item>
            <Menu.Item
              onClick={() => {
                setCollapsed({});
                save(projectKey, {});
                setProjectsOpen(true);
              }}
            >
              Expand all projects
            </Menu.Item>
          </Menu.Dropdown>
        </Menu>
        {!compact && (
          <ActionIcon aria-label="Add project" onClick={p.addProject}>
            <Plus size={18} />
          </ActionIcon>
        )}
      </div>
      <nav
        id="chat-list"
        className="chat-scroll"
        aria-label="Lead conversations"
      >
        {projectsOpen &&
          projectGroups.map((group) => {
            const shown = projectLimits[group.path] || 5;
            const selected = group.chats.find((a) => a.id === p.opened);
            const chats = group.chats.slice(
              0,
              query ? group.chats.length : shown,
            );
            if (selected && !chats.includes(selected)) chats.push(selected);
            const isCollapsed = collapsed[group.path] && !query;
            return (
              <section
                className="sidebar-project"
                data-project-path={group.path}
                key={group.path}
              >
                <div className="project-tree-heading">
                  <UnstyledButton
                    {...sorting.bindings(
                      "projects",
                      group.path,
                      allProjectGroups.map((item) => item.path),
                    )}
                    aria-description="Drag to reorder. Alt + Up or Down moves this project."
                    className="project-tree-toggle"
                    title={group.path}
                    aria-expanded={!isCollapsed}
                    onClick={() => toggleProject(group.path)}
                  >
                    {isCollapsed ? (
                      <Folder size={18} />
                    ) : (
                      <FolderOpen size={18} />
                    )}
                    <span>{group.name}</span>
                  </UnstyledButton>
                  {!archive && (!compact || !!group.path) && (
                    <ActionIcon
                      className="project-tree-action"
                      aria-label={`New chat in ${group.name}`}
                      onClick={() => p.newChat(group.path || undefined)}
                      disabled={p.creating}
                    >
                      <Plus size={15} />
                    </ActionIcon>
                  )}
                  {!compact && group.registered && !group.hasChats && (
                    <Menu withinPortal position="bottom-end">
                      <Menu.Target>
                        <ActionIcon
                          className="project-tree-action"
                          aria-label={`Options for project ${group.name}`}
                        >
                          <MoreHorizontal size={15} />
                        </ActionIcon>
                      </Menu.Target>
                      <Menu.Dropdown>
                        <Menu.Item
                          onClick={async () => {
                            try {
                              await api("/api/projects", {
                                action: "remove",
                                path: group.path,
                              });
                              await p.refresh?.();
                            } catch (error) {
                              p.notify?.(errorText(error));
                            }
                          }}
                        >
                          Remove from sidebar
                        </Menu.Item>
                      </Menu.Dropdown>
                    </Menu>
                  )}
                </div>
                {!isCollapsed && (
                  <div className="project-chats">
                    {chats.map(renderRow)}
                    {!chats.length && <p className="project-empty">No chats</p>}
                    {!query && group.chats.length > shown && (
                      <UnstyledButton
                        className="project-show-more"
                        onClick={() =>
                          setProjectLimits({
                            ...projectLimits,
                            [group.path]: shown + 10,
                          })
                        }
                      >
                        Show more
                      </UnstyledButton>
                    )}
                  </div>
                )}
              </section>
            );
          })}
        {!filtered.length && !projectGroups.length && (
          <p className="notice">
            {query ? "No matching chats." : "No chats yet."}
          </p>
        )}
      </nav>
      {!compact && (
        <div className="sidebar-footer">
          <Button
            id="open-complaints"
            fullWidth
            justify="space-between"
            className={p.view === "complaints" ? "selected" : ""}
            leftSection={<BookOpen size={16} />}
            onClick={p.complaints}
            rightSection={
              <span id="complaint-count">
                {p.data.runtime.complaints?.filter(complaintNeedsUserResponse)
                  .length || ""}
              </span>
            }
          >
            Complaint book
          </Button>
        </div>
      )}
    </aside>
  );
  return compact ? (
    <Drawer
      opened={p.mobile}
      onClose={p.close}
      size="min(360px, 100vw)"
      padding={0}
      withCloseButton={false}
      title="Conversations"
      classNames={{ header: "sr-only" }}
    >
      {content}
    </Drawer>
  ) : (
    content
  );
}
