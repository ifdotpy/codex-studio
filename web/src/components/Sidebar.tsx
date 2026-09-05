import { useEffect, useState } from "react";
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
  return (
    <aside
      id="sidebar"
      className={p.mobile ? "visible" : ""}
      aria-label="Conversations"
    >
      <a className="brand" href="/">
        codex<span>agents</span>
      </a>
      <button
        id="new-chat"
        className="new-chat"
        onClick={p.newChat}
        disabled={p.creating}
      >
        ＋ New chat
      </button>
      <input
        id="chat-search"
        type="search"
        placeholder="Search chats"
        aria-label="Search chats"
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          setLimit(60);
        }}
      />
      <div className="chat-tabs" role="tablist">
        <button
          role="tab"
          aria-selected={tab === "leads"}
          onClick={() => {
            setTab("leads");
            setQuery("");
            setLimit(60);
          }}
        >
          Leads
        </button>
        <button
          role="tab"
          aria-selected={tab === "agents"}
          onClick={() => {
            setTab("agents");
            setQuery("");
            setLimit(60);
          }}
        >
          Agents <span>{rooms.length || ""}</span>
        </button>
      </div>
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
              className={`sidebar-row ${p.opened === row.id ? "selected" : ""}`}
              key={row.id}
            >
              <button
                className="chat-row"
                data-chat={a ? row.id : undefined}
                data-room={room ? row.id : undefined}
                onClick={() => p.open(row.id)}
                aria-current={p.opened === row.id}
              >
                <span
                  className={`avatar ${room?.kind === "broadcast" ? "broadcast" : ""}`}
                >
                  {room?.kind === "broadcast"
                    ? "#"
                    : row.name.slice(0, 2).toUpperCase()}
                </span>
                <span className="row-copy">
                  <strong>{row.name}</strong>
                  <small>{preview}</small>
                </span>
                <span className="row-meta">
                  {time && (
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
              </button>
              {renaming === row.id ? (
                <form
                  className="inline-rename"
                  onSubmit={(e) => void rename(e, row.id)}
                >
                  <input
                    aria-label="Chat name"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    autoFocus
                    maxLength={80}
                    onKeyDown={(e) => {
                      if (e.key === "Escape") setRenaming(null);
                    }}
                  />
                  <button aria-label="Save name">✓</button>
                </form>
              ) : (
                <details className="row-actions">
                  <summary aria-label={`Actions for ${row.name}`}>⋯</summary>
                  <div className="menu">
                    <button
                      onClick={(e) => {
                        e.currentTarget.closest("details")!.open = false;
                        setName(row.name);
                        setRenaming(row.id);
                      }}
                    >
                      Rename
                    </button>
                    <button
                      className="danger"
                      onClick={(e) => {
                        e.currentTarget.closest("details")!.open = false;
                        p.remove(row.id, !!room);
                      }}
                    >
                      Delete
                    </button>
                  </div>
                </details>
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
          <button className="load-more" onClick={() => setLimit((n) => n + 40)}>
            More chats
          </button>
        )}
      </nav>
      <button
        id="open-complaints"
        className={p.view === "complaints" ? "selected" : ""}
        onClick={p.complaints}
      >
        Complaint book{" "}
        <span id="complaint-count">
          {p.data.runtime.complaints?.filter((c) => c.needsResponse).length ||
            ""}
        </span>
      </button>
      <div className="sidebar-bottom">
        <button id="import-chat" onClick={p.importChat}>
          Import from Codex
        </button>
        <button id="other-sessions" onClick={p.other}>
          Other sessions
        </button>
      </div>
    </aside>
  );
}
