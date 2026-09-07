import { useEffect, useState } from "react";
import {
  ActionIcon,
  Avatar,
  Button,
  Drawer,
  Menu,
  Tabs,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import {
  ArrowLeft,
  Archive,
  Pin,
  PinOff,
  ArchiveRestore,
  BookOpen,
  Check,
  FolderOpen,
  MessageSquare,
  MoreHorizontal,
  Pencil,
  Search,
  Folder,
  ChevronDown,
  Plus,
  Trash2,
  Users,
  X,
} from "lucide-react";
import { api, errorText, save, saved } from "../api";
import "./sidebar-projects.css";
import { roomLeadIds } from "../chatScope";
import {
  busy,
  complaintNeedsUserResponse,
  statusLabel,
  type Agent,
  type Room,
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
  const [tab, setTab] = useState("leads"),
    [query, setQuery] = useState(""),
    [limit, setLimit] = useState(60),
    [renaming, setRenaming] = useState<string | null>(null),
    [name, setName] = useState(""),
    [archive, setArchive] = useState(false),
    [projectsOpen, setProjectsOpen] = useState(true);
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
    try {
      await api("/api/organization", { id, ...data });
      await p.refresh?.();
    } catch (error) {
      p.notify?.(errorText(error));
    }
  };
  const key = `codex-chat-seen:${p.data.stateDir}`;
  const [seen, setSeen] = useState<Record<string, number>>(() =>
    saved(key, {}),
  );
  const agents = p.data.threads.filter(
      (a) => a.source === "managed" && a.isLead,
    ),
    rooms = (p.data.runtime.rooms || []).filter(
      (room) =>
        !!p.lead && roomLeadIds(room, p.data.threads).includes(p.lead.id),
    );
  useEffect(() => {
    const room = rooms.find((r) => r.id === p.opened);
    if (
      p.view === "chat" &&
      room?.lastMessage &&
      (seen[room.id] || 0) < room.lastMessage.seq
    ) {
      const next = { ...seen, [room.id]: room.lastMessage.seq };
      setSeen(next);
      save(key, next);
    }
  }, [p.opened, p.view, rooms, key, seen]);
  useEffect(() => {
    if (agents.some((a) => a.id === p.opened)) {
      setTab("leads");
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
  useEffect(() => {
    if (compact) setTab("leads");
  }, [compact]);
  const rows: (Agent | Room)[] = tab === "leads" ? agents : rooms;
  const filtered = rows
    .filter((row) => tab !== "leads" || !!(row as Agent).archived === archive)
    .filter((a) =>
      `${a.name} ${(a as Agent).cwd || ""} ${(a as Agent).project || ""} ${"lastMessage" in a ? a.lastMessage?.text : (a as Agent).tail}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    )
    .sort(
      (a, b) =>
        Number(!!(b as Agent).pinned) - Number(!!(a as Agent).pinned) ||
        (("updated" in b ? b.updated : b.created) || 0) -
          (("updated" in a ? a.updated : a.created) || 0),
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
  for (const a of filtered as Agent[]) {
    if (tab !== "leads") break;
    const path = a.cwd || "";
    if (!groupMap.has(path))
      groupMap.set(path, {
        path,
        name: path.split("/").filter(Boolean).at(-1) || "Other chats",
        chats: [],
        registered: false,
        hasChats: true,
      });
    groupMap.get(path)!.chats.push(a);
  }
  const projectGroups = [...groupMap.values()]
    .filter(
      (group) =>
        !query ||
        group.chats.length ||
        `${group.path} ${group.name}`
          .toLowerCase()
          .includes(query.toLowerCase()),
    )
    .sort((a, b) => a.name.localeCompare(b.name));
  const renderRow = (row: Agent | Room) => {
    const room =
      "kind" in row && ["broadcast", "private"].includes(row.kind)
        ? (row as Room)
        : null;
    const a = room ? null : (row as Agent);
    const preview =
      room?.lastMessage?.text ||
      a?.tail ||
      (a ? statusLabel(a.status) : "No messages yet");
    const unread =
      room?.lastMessage && (seen[row.id] || 0) < room.lastMessage.seq;
    const time = room?.lastMessage?.created || a?.created;
    return (
      <div
        className={`sidebar-row ${room ? "room-row" : "lead-row"} ${p.opened === row.id && p.view === "chat" ? "selected" : ""}`}
        key={row.id}
      >
        <UnstyledButton
          className="chat-row"
          data-chat={a ? row.id : undefined}
          data-room={room ? row.id : undefined}
          onClick={() => p.open(row.id)}
          aria-current={p.opened === row.id}
        >
          {room && (
            <Avatar
              size={38}
              radius="xl"
              color={room.kind === "broadcast" ? "indigo" : "cyan"}
            >
              {room.kind === "broadcast" ? (
                <Users size={18} />
              ) : (
                row.name.slice(0, 2).toUpperCase()
              )}
            </Avatar>
          )}
          <span className="row-copy">
            <strong>
              {a?.pinned && <Pin size={11} className="chat-pin" />}
              {row.name}
            </strong>
            {room && <small>{preview}</small>}
          </span>
          <span className="row-meta">
            {room && time && (
              <time>
                {new Date(time * 1000).toLocaleTimeString([], {
                  hour: "2-digit",
                  minute: "2-digit",
                })}
              </time>
            )}
            {unread && <span className="unread" aria-label="Unread messages" />}
            {a && busy.has(a.status) && <span className="dot running" />}
          </span>
        </UnstyledButton>
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
                onClick={() => p.remove(row.id, !!room)}
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
          setLimit(60);
        }}
      />
      {!compact && (
        <Tabs
          className="chat-tabs"
          value={tab}
          onChange={(v) => {
            setTab(v || "leads");
            setQuery("");
            setLimit(60);
          }}
        >
          <Tabs.List grow>
            <Tabs.Tab value="leads" leftSection={<MessageSquare size={14} />}>
              Chats
            </Tabs.Tab>
            <Tabs.Tab value="agents" leftSection={<Users size={14} />}>
              Agent chats{" "}
              {rooms.length > 0 && (
                <span className="tab-count">{rooms.length}</span>
              )}
            </Tabs.Tab>
          </Tabs.List>
        </Tabs>
      )}
      {tab === "leads" && (
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
      )}
      {tab === "agents" && p.lead && (
        <UnstyledButton
          className="agent-chats-context"
          onClick={() => p.open(p.lead!.id)}
          aria-label={`Back to ${p.lead.name}`}
          title={p.lead.name}
        >
          <ArrowLeft size={15} />
          <span>{p.lead.name}</span>
        </UnstyledButton>
      )}
      <nav
        id={tab === "leads" ? "chat-list" : "agent-chat-list"}
        className="chat-scroll"
        aria-label={tab === "leads" ? "Lead conversations" : "Agent chats"}
        onScroll={(e) => {
          const t = e.currentTarget;
          if (t.scrollHeight - t.scrollTop - t.clientHeight < 160)
            setLimit((n) => Math.min(n + 40, filtered.length));
        }}
      >
        {tab === "agents"
          ? filtered.slice(0, limit).map(renderRow)
          : projectsOpen &&
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
                      {!chats.length && (
                        <p className="project-empty">No chats</p>
                      )}
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
        {!filtered.length && (tab === "agents" || !projectGroups.length) && (
          <p className="notice">
            {query
              ? "No matching chats."
              : tab === "agents"
                ? p.lead
                  ? "No agent chats in this conversation."
                  : "Select a chat to see its agent chats."
                : "No chats yet."}
          </p>
        )}
        {tab === "agents" && limit < filtered.length && (
          <Button className="load-more" onClick={() => setLimit((n) => n + 40)}>
            More chats
          </Button>
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
