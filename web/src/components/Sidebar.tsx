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
  ArrowDownToLine,
  BookOpen,
  Check,
  FolderOpen,
  MessageSquare,
  MoreHorizontal,
  Pencil,
  Search,
  SquarePen,
  Terminal,
  Trash2,
  Users,
} from "lucide-react";
import { save, saved } from "../api";
import {
  busy,
  statusLabel,
  type Agent,
  type Room,
  type Snapshot,
} from "../types";
type Props = {
  data: Snapshot;
  opened: string | null;
  view: string;
  open: (id: string) => void;
  newChat: () => void;
  creating: boolean;
  complaints: () => void;
  rename: (id: string, name: string) => Promise<void>;
  remove: (id: string, room: boolean) => void;
  importChat: () => void;
  other: () => void;
  mobile: boolean;
  close: () => void;
};
export default function Sidebar(p: Props) {
  const [tab, setTab] = useState("leads"),
    [query, setQuery] = useState(""),
    [limit, setLimit] = useState(60),
    [renaming, setRenaming] = useState<string | null>(null),
    [name, setName] = useState("");
  const key = `codex-chat-seen:${p.data.stateDir}`;
  const [seen, setSeen] = useState<Record<string, number>>(() =>
    saved(key, {}),
  );
  const agents = p.data.threads.filter(
      (a) => a.source === "managed" && a.isLead,
    ),
    rooms = p.data.runtime.rooms || [];
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
    }
  }, [p.opened]);
  const rows: (Agent | Room)[] = tab === "leads" ? agents : rooms;
  const filtered = rows
    .filter((a) =>
      `${a.name} ${"lastMessage" in a ? a.lastMessage?.text : (a as Agent).tail}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    )
    .sort(
      (a, b) =>
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
  const compact = useMediaQuery("(max-width: 760px)");
  const content = (
    <aside id="sidebar" aria-label="Conversations">
      <a className="brand" href="/">
        <span className="brand-mark">
          <Terminal size={18} />
        </span>
        Codex <span>Agents</span>
      </a>
      <Button
        id="new-chat"
        className="new-chat"
        variant="default"
        leftSection={<SquarePen size={16} />}
        onClick={p.newChat}
        loading={p.creating}
      >
        New chat
      </Button>
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
            Leads
          </Tabs.Tab>
          <Tabs.Tab value="agents" leftSection={<Users size={14} />}>
            Agents{" "}
            {rooms.length > 0 && (
              <span className="tab-count">{rooms.length}</span>
            )}
          </Tabs.Tab>
        </Tabs.List>
      </Tabs>
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
        {filtered.slice(0, limit).map((row) => {
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
                  <strong>{row.name}</strong>
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
                  {unread && (
                    <span className="unread" aria-label="Unread messages" />
                  )}
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
                <Menu
                  position="bottom-end"
                  withinPortal
                  shadow="lg"
                  width={170}
                >
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
        })}
        {!filtered.length && (
          <p className="notice">
            {query ? "No matching chats." : "No chats yet."}
          </p>
        )}
        {limit < filtered.length && (
          <Button className="load-more" onClick={() => setLimit((n) => n + 40)}>
            More chats
          </Button>
        )}
      </nav>
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
              {p.data.runtime.complaints?.filter((c) => c.needsResponse)
                .length || ""}
            </span>
          }
        >
          Complaint book
        </Button>
        <div className="sidebar-bottom">
          <Button
            id="import-chat"
            leftSection={<ArrowDownToLine size={15} />}
            onClick={p.importChat}
          >
            Import from Codex
          </Button>
          <Button
            id="other-sessions"
            leftSection={<FolderOpen size={15} />}
            onClick={p.other}
          >
            Other sessions
          </Button>
        </div>
      </div>
    </aside>
  );
  return compact ? (
    <Drawer
      opened={p.mobile}
      onClose={p.close}
      size={300}
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
