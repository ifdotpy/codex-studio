import { Button, TextInput } from "@mantine/core";
import {
  ChevronDown,
  ChevronUp,
  Copy,
  Plus,
  Search,
  Square,
  Terminal as TerminalIcon,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { Terminal } from "@xterm/xterm";
import { api, errorText, save, saved } from "../api";
import type { Agent, Snapshot } from "../types";
import "./terminal-dock.css";

type Shell = {
  id: string;
  agent: string;
  title: string;
  cwd: string;
  status: string;
  exitCode?: number;
  error?: string;
  created: number;
};
type Entry = Shell & { key: string };
type Output = {
  text: string;
  offset: number;
  truncated: boolean;
  status: string;
  exitCode?: number;
  error?: string;
};
const running = (status: string) =>
  ["running", "starting", "approval"].includes(status);

export default function TerminalDock({
  data,
  agent,
  notify,
}: {
  data: Snapshot | null;
  agent?: Agent;
  notify: (message: string) => void;
}) {
  const [opened, setOpened] = useState(() =>
    saved("codex.terminal.open", false),
  );
  const [height, setHeight] = useState(() =>
    saved("codex.terminal.height", 360),
  );
  const [shells, setShells] = useState<Shell[]>([]);
  const [selected, setSelected] = useState(() =>
    saved("codex.terminal.selected", ""),
  );
  const [query, setQuery] = useState("");
  const [scrollTop, setScrollTop] = useState(0);
  const [error, setError] = useState("");
  const [creating, setCreating] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [title, setTitle] = useState("");
  const [pending, setPending] = useState(false);
  const [endTarget, setEndTarget] = useState<string | null>(null);
  const list = useRef<HTMLDivElement>(null);
  const listingRevision = useRef(0);
  const notifyRef = useRef(notify);
  notifyRef.current = notify;
  const load = useCallback(async (allowed: () => boolean = () => true) => {
    const request = ++listingRevision.current;
    const current = () => request === listingRevision.current && allowed();
    try {
      const value = await api<{ items: Shell[] }>("/api/terminals");
      if (!current()) return;
      setShells(value.items || []);
      setError("");
    } catch (error) {
      if (current()) throw error;
    }
  }, []);
  useEffect(() => {
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try {
        await load(() => !stopped);
      } catch (e) {
        if (!stopped) setError(errorText(e));
      }
      if (!stopped) timer = setTimeout(poll, opened ? 1600 : 5000);
    };
    void poll();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [opened, load]);
  useEffect(() => {
    if (!opened) setEndTarget(null);
    const update = () =>
      document.documentElement.style.setProperty(
        "--terminal-dock-height",
        `${opened ? Math.min(height, window.innerHeight - 100) : 38}px`,
      );
    update();
    window.addEventListener("resize", update);
    save("codex.terminal.open", opened);
    save("codex.terminal.height", height);
    return () => window.removeEventListener("resize", update);
  }, [opened, height]);
  useEffect(() => {
    const key = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.code === "Backquote") {
        event.preventDefault();
        setOpened((value: boolean) => !value);
      }
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, []);
  useEffect(() => {
    save("codex.terminal.selected", selected);
    setRenaming(false);
    setEndTarget(null);
  }, [selected]);
  const entries: Entry[] = shells
    .filter((shell) => shell.status !== "closed")
    .map((shell) => ({ key: `shell:${shell.id}`, ...shell }));
  const current = entries.find((entry) => entry.key === selected);
  const owner = (id: string) =>
    data?.threads.find((item) => item.id === id)?.name || "Agent";
  const filtered = entries.filter((entry) =>
    `${entry.title} ${entry.cwd || ""} ${owner(entry.agent)} ${entry.status}`
      .toLowerCase()
      .includes(query.toLowerCase()),
  );
  const rowHeight = 58;
  const start = Math.max(
    0,
    Math.min(
      Math.max(0, filtered.length - 1),
      Math.floor(scrollTop / rowHeight) - 3,
    ),
  );
  const visible = filtered.slice(
    start,
    start + Math.ceil(height / rowHeight) + 6,
  );
  const create = async () => {
    if (!agent || creating) return;
    setCreating(true);
    setOpened(true);
    try {
      const shell = await api<Shell>("/api/terminals/create", {
        id: crypto.randomUUID(),
        agent: agent.id,
        cols: 100,
        rows: 24,
      });
      setShells((items) => [
        shell,
        ...items.filter((item) => item.id !== shell.id),
      ]);
      setSelected(`shell:${shell.id}`);
      setQuery("");
      setScrollTop(0);
      if (list.current) list.current.scrollTop = 0;
      await load();
    } catch (e) {
      notifyRef.current(errorText(e));
    } finally {
      setCreating(false);
    }
  };
  const action = async (path: string, body: object) => {
    setPending(true);
    try {
      await api(path, body);
      await load();
      return true;
    } catch (e) {
      notifyRef.current(errorText(e));
      return false;
    } finally {
      setPending(false);
    }
  };
  return (
    <section
      className={`terminal-dock ${opened ? "is-open" : ""}`}
      aria-label="Terminals"
      style={{ height: opened ? `min(${height}px, calc(100dvh - 100px))` : 38 }}
    >
      {opened && (
        <div
          className="terminal-dock-resizer"
          role="separator"
          aria-label="Terminal panel height"
          aria-orientation="horizontal"
          tabIndex={0}
          onKeyDown={(event) => {
            if (event.key === "ArrowUp" || event.key === "ArrowDown") {
              event.preventDefault();
              setHeight(
                Math.max(
                  220,
                  Math.min(
                    window.innerHeight - 100,
                    height + (event.key === "ArrowUp" ? 40 : -40),
                  ),
                ),
              );
            }
          }}
          onPointerDown={(event) => {
            event.currentTarget.setPointerCapture(event.pointerId);
          }}
          onPointerMove={(event) => {
            if (event.currentTarget.hasPointerCapture(event.pointerId))
              setHeight(
                Math.max(
                  220,
                  Math.min(
                    window.innerHeight - 100,
                    window.innerHeight - event.clientY,
                  ),
                ),
              );
          }}
        />
      )}
      <header className="terminal-dock-bar">
        <button
          type="button"
          className="terminal-dock-toggle"
          onClick={() => setOpened(!opened)}
          aria-expanded={opened}
          aria-label={opened ? "Hide terminals" : "Show terminals"}
        >
          <TerminalIcon size={15} />
          <strong>Terminals</strong>
          <span className="terminal-dock-count">{entries.length}</span>
          {entries.some((entry) => running(entry.status)) && (
            <span className="terminal-dock-running">
              {entries.filter((entry) => running(entry.status)).length} active
            </span>
          )}
          {opened ? <ChevronDown size={14} /> : <ChevronUp size={14} />}
        </button>
        {!opened && current && (
          <span className="terminal-dock-current">{current.title}</span>
        )}
        <span className="terminal-dock-spacer" />
        {opened && (
          <Button
            size="compact-xs"
            variant="subtle"
            onClick={() => setOpened(false)}
          >
            Hide panel
          </Button>
        )}
        <Button
          size="compact-xs"
          aria-label="New terminal"
          title="New terminal"
          leftSection={<Plus size={13} />}
          disabled={!agent}
          loading={creating}
          onClick={() => void create()}
        >
          <span className="terminal-new-label">New terminal</span>
        </Button>
      </header>
      {opened && (
        <div className="terminal-dock-body">
          <aside
            className="terminal-dock-sessions"
            aria-label="Terminal sessions"
          >
            <TextInput
              aria-label="Find terminal"
              placeholder="Find a terminal"
              leftSection={<Search size={13} />}
              size="xs"
              value={query}
              onChange={(event) => {
                setQuery(event.currentTarget.value);
                setScrollTop(0);
                if (list.current) list.current.scrollTop = 0;
              }}
            />
            <div
              className="terminal-dock-list"
              ref={list}
              onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}
            >
              <div
                style={{
                  height: filtered.length * rowHeight,
                  position: "relative",
                }}
              >
                {visible.map((entry, index) => (
                  <button
                    type="button"
                    className={`terminal-session ${current?.key === entry.key ? "is-selected" : ""}`}
                    key={entry.key}
                    onClick={() => setSelected(entry.key)}
                    aria-pressed={current?.key === entry.key}
                    style={{
                      position: "absolute",
                      top: (start + index) * rowHeight,
                      height: rowHeight,
                      width: "100%",
                    }}
                    title={`${entry.title}\n${entry.cwd || ""}`}
                  >
                    <span
                      className={`terminal-session-dot ${running(entry.status) ? "is-running" : ""}`}
                    />
                    <span className="terminal-session-description">
                      <strong>{entry.title}</strong>
                      <small>Your terminal · {owner(entry.agent)}</small>
                    </span>
                  </button>
                ))}
              </div>
              {!filtered.length && (
                <p className="terminal-dock-empty">
                  {query
                    ? "No matching sessions"
                    : "Your terminals appear here."}
                </p>
              )}
            </div>
          </aside>
          <div className="terminal-dock-detail">
            {error && (
              <p className="terminal-dock-error" role="status">
                {error}{" "}
                <button
                  onClick={() =>
                    void load().catch((e) => setError(errorText(e)))
                  }
                >
                  Retry
                </button>
              </p>
            )}
            {current ? (
              <>
                <div className="terminal-detail-heading">
                  {renaming ? (
                    <form
                      onSubmit={(event) => {
                        event.preventDefault();
                        void action("/api/terminals/rename", {
                          id: current.id,
                          title,
                        }).then((ok) => {
                          if (ok) setRenaming(false);
                        });
                      }}
                    >
                      <TextInput
                        aria-label="Terminal name"
                        value={title}
                        autoFocus
                        maxLength={120}
                        onChange={(event) =>
                          setTitle(event.currentTarget.value)
                        }
                        onKeyDown={(event) => {
                          if (event.key === "Escape") setRenaming(false);
                        }}
                      />
                      <Button
                        size="compact-xs"
                        type="submit"
                        disabled={pending || !title.trim()}
                      >
                        Save
                      </Button>
                    </form>
                  ) : (
                    <button
                      className="terminal-detail-title"
                      title="Rename terminal"
                      onClick={() => {
                        setTitle(current.title);
                        setRenaming(true);
                      }}
                    >
                      {current.title}
                    </button>
                  )}
                  <span className="terminal-detail-status">
                    {current.status}
                    {current.exitCode != null
                      ? ` · Exit ${current.exitCode}`
                      : ""}
                  </span>
                  <span className="terminal-dock-spacer" />
                  <Button
                    size="compact-xs"
                    color="red"
                    variant="subtle"
                    disabled={pending}
                    onClick={() => {
                      if (running(current.status)) setEndTarget(current.id);
                      else
                        void action("/api/terminals/close", { id: current.id });
                    }}
                  >
                    End session
                  </Button>
                </div>
                {endTarget === current.id && (
                  <div
                    className="terminal-end-confirm"
                    role="alertdialog"
                    aria-label="End terminal session"
                  >
                    <p>
                      End this session? This stops the shell and its active
                      processes.
                    </p>
                    <Button
                      size="compact-xs"
                      disabled={pending}
                      onClick={() => setEndTarget(null)}
                    >
                      Keep session
                    </Button>
                    <Button
                      size="compact-xs"
                      color="red"
                      loading={pending}
                      onClick={() => {
                        const id = current.id;
                        setEndTarget(null);
                        void action("/api/terminals/close", { id });
                      }}
                    >
                      End session and stop processes
                    </Button>
                  </div>
                )}
                <div className="terminal-detail-path" title={current.cwd}>
                  {current.cwd || owner(current.agent)}
                </div>
                <ShellView key={current.key} shell={current} notify={notify} />
              </>
            ) : (
              <div className="terminal-dock-welcome">
                <TerminalIcon size={26} />
                <strong>Your terminals, always here</strong>
                <p>
                  Start a shell in {agent?.cwd || "the selected agent’s folder"}
                  .
                </p>
                <Button
                  size="xs"
                  leftSection={<Plus size={14} />}
                  disabled={!agent}
                  loading={creating}
                  onClick={() => void create()}
                >
                  Open terminal
                </Button>
                <small>⌘ / Ctrl + ` to show or hide</small>
              </div>
            )}
          </div>
        </div>
      )}
    </section>
  );
}

function ShellView({
  shell,
  notify,
}: {
  shell: Shell;
  notify: (message: string) => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal | null>(null);
  const [failure, setFailure] = useState("");
  const [inputFailure, setInputFailure] = useState("");
  const inputBlocked = useRef(false);
  const [truncated, setTruncated] = useState(false);
  const [status, setStatus] = useState(shell.status);
  const [retry, setRetry] = useState(0);
  const inputChain = useRef(Promise.resolve());
  const send = (text: string) => {
    if (inputBlocked.current) return;
    const request_id = crypto.randomUUID();
    inputChain.current = inputChain.current
      .then(async () => {
        if (inputBlocked.current) return;
        const result = await api("/api/terminals/input", {
          id: shell.id,
          text,
          request_id,
        });
        if (result.ok === false)
          throw new Error(
            result.error || "Terminal input delivery is uncertain.",
          );
      })
      .catch((e) => {
        inputBlocked.current = true;
        if (terminal.current) terminal.current.options.disableStdin = true;
        setInputFailure(
          errorText(e) + " Input paused. Reconnect before you continue.",
        );
      });
  };
  const sendRef = useRef(send);
  sendRef.current = send;
  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    let observer: ResizeObserver | undefined;
    let resizeTimer: ReturnType<typeof setTimeout>;
    let offset = 0;
    inputBlocked.current = false;
    setInputFailure("");
    let instance: Terminal | undefined;
    void Promise.all([
      import("@xterm/xterm"),
      import("@xterm/addon-fit"),
      import("@xterm/xterm/css/xterm.css"),
    ])
      .then(([{ Terminal }, { FitAddon }]) => {
        if (disposed || !host.current) return;
        instance = new Terminal({
          cursorBlink: true,
          screenReaderMode: true,
          fontSize: 12,
          fontFamily: "SFMono-Regular, Menlo, Consolas, monospace",
          scrollback: 5000,
          allowProposedApi: false,
          disableStdin: !running(shell.status),
          theme: {
            background: "#141519",
            foreground: "#dadde5",
            cursor: "#a7a6ff",
            selectionBackground: "#55547d",
          },
        });
        terminal.current = instance;
        const fit = new FitAddon();
        instance.loadAddon(fit);
        instance.open(host.current);
        instance.onData((text) => sendRef.current(text));
        const resize = () => {
          if (disposed || !host.current?.clientWidth) return;
          fit.fit();
          clearTimeout(resizeTimer);
          resizeTimer = setTimeout(() => {
            if (instance && !disposed && running(shell.status))
              void api("/api/terminals/resize", {
                id: shell.id,
                cols: instance.cols,
                rows: instance.rows,
              }).catch((e) => {
                if (!disposed) setFailure(errorText(e));
              });
          }, 150);
        };
        observer = new ResizeObserver(resize);
        observer.observe(host.current);
        resize();
        const poll = async () => {
          try {
            const output = await api<Output>(
              `/api/terminals/output?id=${encodeURIComponent(shell.id)}&offset=${offset}`,
            );
            if (disposed) return;
            if (output.truncated) {
              instance!.reset();
              setTruncated(true);
            }
            if (output.text) instance!.write(output.text);
            offset = output.offset;
            setStatus(output.status);
            instance!.options.disableStdin =
              inputBlocked.current || !running(output.status);
            setFailure(output.error || "");
          } catch (e) {
            if (!disposed) setFailure(errorText(e));
          }
          if (!disposed) timer = setTimeout(poll, 350);
        };
        void poll();
      })
      .catch((e) => {
        if (!disposed) setFailure(errorText(e));
      });
    return () => {
      disposed = true;
      clearTimeout(timer);
      clearTimeout(resizeTimer);
      observer?.disconnect();
      instance?.dispose();
      terminal.current = null;
    };
  }, [shell.id, retry]);
  return (
    <div className="terminal-shell-view">
      {truncated && (
        <div className="terminal-output-notice">
          Earlier output exceeded the retained buffer. The latest output is
          shown.
        </div>
      )}
      {(failure || inputFailure) && (
        <div className="terminal-dock-error" role="status">
          {inputFailure || failure}{" "}
          <button onClick={() => setRetry((value) => value + 1)}>
            Reconnect
          </button>
        </div>
      )}
      <div
        ref={host}
        className="terminal-xterm"
        aria-label="Interactive terminal"
      />
      <footer className="terminal-output-actions">
        <span>
          {running(status)
            ? "Direct shell input · No model call"
            : "Session ended · Output retained"}
        </span>
        <span className="terminal-dock-spacer" />
        <Button
          size="compact-xs"
          variant="subtle"
          leftSection={<Copy size={12} />}
          onClick={() => {
            const text = terminal.current?.getSelection();
            if (!text) {
              notify("Select terminal text first.");
              return;
            }
            void navigator.clipboard
              .writeText(text)
              .catch((e) => notify(errorText(e)));
          }}
        >
          Copy selection
        </Button>
        <Button
          size="compact-xs"
          variant="subtle"
          disabled={!running(status) || !!inputFailure}
          leftSection={<Square size={11} />}
          onClick={() => send("\u0003")}
        >
          Ctrl+C
        </Button>
      </footer>
    </div>
  );
}
