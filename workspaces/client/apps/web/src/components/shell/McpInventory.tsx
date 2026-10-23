import { Button } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import {
  errorText,
  type GetResult,
  type PostBody,
  type PostResult,
} from "../../api";
import { useServerServices } from "../../servers/ServerServicesContext";
import { serverViewId } from "../../servers/environment";

type Inventory = GetResult<"/api/capabilities">;
type Reconnect = PostBody<"/api/mcp/reconnect">;
type Result = PostResult<"/api/mcp/reconnect">;
type Props = {
  agent: string;
  initial: Inventory;
  query: string;
  loadPage: (cursor?: string) => Promise<Inventory>;
  reconnect: (body: Reconnect) => Promise<Result>;
};
type Server = { name: string; status: string; error: string };

export function mcpServers(entries: Inventory["mcp"]): Server[] {
  return entries.flatMap((entry) => {
    if (!entry || typeof entry !== "object" || Array.isArray(entry)) return [];
    const name = typeof entry.name === "string" ? entry.name : entry.serverName;
    if (typeof name !== "string" || !name.trim()) return [];
    const error =
      typeof entry.error === "string"
        ? entry.error
        : entry.error &&
            typeof entry.error === "object" &&
            !Array.isArray(entry.error) &&
            typeof entry.error.message === "string"
          ? entry.error.message
          : "";
    return [
      {
        name,
        status: typeof entry.status === "string" ? entry.status : "Unknown",
        error,
      },
    ];
  });
}

export function McpInventory({
  agent,
  initial,
  query,
  loadPage,
  reconnect,
}: Props) {
  const services = useServerServices();
  const owner = services?.owner.id ?? serverViewId ?? "local";
  const [inventory, setInventory] = useState(initial);
  const [rows, setRows] = useState(() => mcpServers(initial.mcp));
  const [pageBusy, setPageBusy] = useState(false);
  const [pageError, setPageError] = useState("");
  const [feedback, setFeedback] = useState<
    Record<string, { busy: boolean; message: string }>
  >({});
  const requests = useRef(new Map<string, Reconnect>());
  const inFlight = useRef(new Set<string>());
  const revision = useRef(0);
  const seen = useRef(new Set<string>());
  const active = useRef(true);
  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
    };
  }, []);
  useEffect(() => {
    revision.current += 1;
    setInventory(initial);
    setRows(mcpServers(initial.mcp));
    seen.current.clear();
    setPageError("");
    setPageBusy(false);
  }, [initial]);
  const requestKey = (name: string) =>
    `studio-mcp-reconnect:${JSON.stringify([owner, agent, inventory.mcpSession, name])}`;
  useEffect(() => {
    if (!inventory.mcpSession) return;
    // Restore the visible receipt action after a reload without sending it.
    for (const row of rows) {
      const key = requestKey(row.name);
      try {
        const requestId = sessionStorage.getItem(key);
        if (requestId && !requests.current.has(key)) {
          requests.current.set(key, {
            id: agent,
            name: row.name,
            session: inventory.mcpSession,
            request_id: requestId,
          });
          setFeedback((old) => ({
            ...old,
            [key]: {
              busy: false,
              message:
                "Reconnect outcome is unconfirmed. Check the reconnect result.",
            },
          }));
        }
      } catch {
        /* The click handler reports unavailable storage before any write. */
      }
    }
  }, [owner, agent, inventory.mcpSession, rows]);
  async function refreshServers() {
    if (pageBusy) return;
    const current = ++revision.current;
    setPageBusy(true);
    try {
      const page = await loadPage();
      if (active.current && revision.current === current) {
        setInventory(page);
        setRows(mcpServers(page.mcp));
        seen.current.clear();
        setPageError(page.errors.join(" "));
      }
    } catch (error) {
      if (active.current && revision.current === current)
        setPageError(errorText(error));
    } finally {
      if (active.current && revision.current === current) setPageBusy(false);
    }
  }
  async function loadMore() {
    const cursor = inventory.serversCursor;
    if (!cursor || pageBusy) return;
    const current = revision.current;
    setPageBusy(true);
    setPageError("");
    try {
      if (seen.current.has(cursor))
        throw new Error(
          "The server list cursor did not advance. Refresh the tools list.",
        );
      const page = await loadPage(cursor);
      if (!active.current || revision.current !== current) return;
      if (
        JSON.stringify(page.mcpSession) !== JSON.stringify(inventory.mcpSession)
      )
        throw new Error("The agent session changed. Refresh the tools list.");
      if (page.errors.length) throw new Error(page.errors.join(" "));
      seen.current.add(cursor);
      setRows((previous) => [
        ...new Map(
          [...previous, ...mcpServers(page.mcp)].map((row) => [row.name, row]),
        ).values(),
      ]);
      setInventory(page);
    } catch (error) {
      if (active.current && revision.current === current)
        setPageError(errorText(error));
    } finally {
      if (active.current && revision.current === current) setPageBusy(false);
    }
  }
  async function reconnectServer(name: string) {
    const storageKey = requestKey(name);
    if (!inventory.mcpSession || inFlight.current.has(storageKey)) return;
    let body = requests.current.get(storageKey);
    if (!body) {
      // Keep an uncertain request across a page reload. Failure to persist must
      // stop the mutation before it reaches the server.
      try {
        const requestId =
          sessionStorage.getItem(storageKey) || crypto.randomUUID();
        body = {
          id: agent,
          name,
          session: inventory.mcpSession,
          request_id: requestId,
        };
        sessionStorage.setItem(storageKey, requestId);
        requests.current.set(storageKey, body);
      } catch (error) {
        setFeedback((old) => ({
          ...old,
          [storageKey]: {
            busy: false,
            message: `Cannot save the reconnect request. ${errorText(error)}`,
          },
        }));
        return;
      }
    }
    inFlight.current.add(storageKey);
    setFeedback((old) => ({
      ...old,
      [storageKey]: { busy: true, message: "Reconnect in progress." },
    }));
    try {
      const result = await reconnect(body);
      if (!active.current) return;
      setFeedback((old) => ({
        ...old,
        [storageKey]: { busy: false, message: result.message },
      }));
      if (result.status !== "unknown") {
        try {
          sessionStorage.removeItem(storageKey);
          requests.current.delete(storageKey);
        } catch {
          /* A saved settled identity can still be replayed safely. */
        }
      }
      if (result.status === "completed") await refreshServers();
    } catch (error) {
      if (active.current)
        setFeedback((old) => ({
          ...old,
          [storageKey]: { busy: false, message: errorText(error) },
        }));
    } finally {
      inFlight.current.delete(storageKey);
    }
  }
  const filtered = rows.filter((row) =>
    `${row.name} ${row.status} ${row.error}`
      .toLowerCase()
      .includes(query.toLowerCase()),
  );
  return (
    <div className="workspace-mcp">
      <Button
        variant="subtle"
        loading={pageBusy}
        onClick={() => void refreshServers()}
      >
        Refresh servers
      </Button>
      {!rows.length && (
        <p className="workspace-muted">No MCP servers returned.</p>
      )}
      {!!rows.length && !filtered.length && (
        <p className="workspace-muted">No matching MCP servers.</p>
      )}
      {!inventory.mcpSession && !!rows.length && (
        <p className="workspace-error">
          No active session. Refresh the tools list before reconnect.
        </p>
      )}
      {filtered.map((row) => (
        <section
          className="workspace-mcp-server"
          key={row.name}
          aria-label={`MCP server ${row.name}`}
        >
          <strong>{row.name}</strong>
          <span>Status: {row.status}</span>
          {row.error && <p className="workspace-error">Error: {row.error}</p>}
          <Button
            size="xs"
            loading={feedback[requestKey(row.name)]?.busy}
            disabled={!inventory.mcpSession}
            onClick={() => void reconnectServer(row.name)}
          >
            {requests.current.has(requestKey(row.name))
              ? "Check reconnect result"
              : "Reconnect"}
          </Button>
          {feedback[requestKey(row.name)] && (
            <p role="status">{feedback[requestKey(row.name)].message}</p>
          )}
        </section>
      ))}
      {pageError && (
        <p role="alert" className="workspace-error">
          {pageError}
        </p>
      )}
      {inventory.serversCursor && (
        <Button
          variant="light"
          loading={pageBusy}
          onClick={() => void loadMore()}
        >
          Load more servers
        </Button>
      )}
    </div>
  );
}
