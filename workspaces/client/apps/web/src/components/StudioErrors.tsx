import { useServerApi } from "../servers/ServerServicesContext";
import {
  createContext,
  useContext,
  useCallback,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Badge, Button, Checkbox, Group, Stack, Text } from "@mantine/core";
import { errorText, type GetResult } from "../api";
import { clearAppLog, useAppLog } from "../appLog";
import type { components } from "../generated/api";
import { STUDIO_ERRORS_DISMISS_MAX_IDS } from "../generated/sharedValues";

export type StudioErrorItem = GetResult<"/api/studio-errors">["items"][number];
type Level = components["schemas"]["StudioErrorLevel"];
export const LEVELS: Level[] = ["critical", "error", "warning", "log"];
export const LEVEL_COLORS: Record<Level, string> = {
  critical: "grape",
  error: "red",
  warning: "orange",
  log: "blue",
};
const EMPTY_ERRORS: StudioErrorItem[] = [];

export function actionLabel(item: StudioErrorItem) {
  switch (item.action) {
    case "resume":
      return "Resume";
    case "check":
      return "Open";
    case "details":
      return "Details";
    default: {
      const exhaustive: never = item;
      return exhaustive;
    }
  }
}
function ownerLabel(item: StudioErrorItem) {
  return item.action === "details" ? "Server" : item.agentName;
}
function when(at: number) {
  return at > 0 ? new Date(at * 1000).toLocaleString() : "";
}

/** When the row's failures happened: one time, or the count and the range. */
export function period(item: StudioErrorItem) {
  const last = when(item.at);
  if (item.count == null || item.count <= 1) return last;
  const first = when(item.firstAt ?? item.at);
  return `${item.count} times${first && last ? `, ${first} to ${last}` : ""}`;
}

/** The rows with their reason and one safe action each. */
export function StudioErrorList({
  items,
  busy,
  selected,
  onSelect,
  onSelectAll,
  onDelete,
  onAction,
}: {
  items: StudioErrorItem[];
  busy: string | null;
  selected: ReadonlySet<string>;
  onSelect: (id: string, checked: boolean) => void;
  onSelectAll: (ids: string[]) => void;
  onDelete: () => void;
  onAction: (item: StudioErrorItem) => void;
}) {
  const logs = useAppLog();
  const [filter, setFilter] = useState<Level | null>(null);
  const rows = [
    ...items.map((item) => ({
      type: "server" as const,
      level: item.level,
      at: item.at * 1000,
      item,
    })),
    ...logs.map((item) => ({
      type: "local" as const,
      level: item.level,
      at: item.at,
      item,
    })),
  ].sort(
    (a, b) => LEVELS.indexOf(a.level) - LEVELS.indexOf(b.level) || b.at - a.at,
  );
  const visible = filter ? rows.filter((row) => row.level === filter) : rows;
  const visibleServerIds = visible
    .filter((row) => row.type === "server")
    .map((row) => row.item.id);
  return (
    <Stack gap="sm">
      <Group gap="xs" aria-label="Filter by level">
        <Button
          size="xs"
          variant={filter === null ? "filled" : "light"}
          onClick={() => setFilter(null)}
        >
          All ({rows.length})
        </Button>
        {LEVELS.map((level) => (
          <Button
            key={level}
            size="xs"
            color={LEVEL_COLORS[level]}
            variant={filter === level ? "filled" : "light"}
            onClick={() => setFilter(level)}
          >
            {level.toUpperCase()} (
            {rows.filter((row) => row.level === level).length})
          </Button>
        ))}
        {logs.length > 0 && (
          <Button size="xs" variant="default" onClick={clearAppLog}>
            Clear local logs
          </Button>
        )}
      </Group>
      <Group gap="xs">
        <Button
          size="xs"
          variant="default"
          disabled={busy !== null || visibleServerIds.length === 0}
          onClick={() => onSelectAll(visibleServerIds)}
        >
          Select all
        </Button>
        <Button
          size="xs"
          color="red"
          variant="light"
          disabled={busy !== null || selected.size === 0}
          onClick={onDelete}
        >
          Delete selected
        </Button>
        <Text size="xs" c="dimmed" role="status">
          {selected.size} selected
        </Text>
      </Group>
      <Text size="xs" c="dimmed">
        Only selected errors are removed from this list. Chats, commands, and
        history are kept.
      </Text>
      {visible.length === 0 ? (
        <Text size="sm">
          {filter
            ? "No entries at this level."
            : "No agent work ended because of a Studio error."}
        </Text>
      ) : (
        <Stack gap="sm" aria-label="Studio errors">
          {visible.map((row) =>
            row.type === "local" ? (
              <article className="studio-error-row" key={row.item.id}>
                <Badge color={LEVEL_COLORS[row.level]}>
                  {row.level.toUpperCase()}
                </Badge>
                <Text size="sm">{row.item.title}</Text>
                <Text size="sm">{row.item.reason}</Text>
                {row.item.detail && (
                  <Text size="xs" c="dimmed">
                    {row.item.detail}
                  </Text>
                )}
                <Text size="xs" c="dimmed">
                  {new Date(row.item.at).toLocaleString()}
                </Text>
              </article>
            ) : (
              <article
                className="studio-error-row"
                key={row.item.id}
                data-kind={row.item.kind}
              >
                <Badge color={LEVEL_COLORS[row.level]}>
                  {row.level.toUpperCase()}
                </Badge>
                <Checkbox
                  label={ownerLabel(row.item)}
                  aria-label={`Select error ${row.item.id} for ${ownerLabel(row.item)}`}
                  checked={selected.has(row.item.id)}
                  disabled={
                    busy !== null ||
                    (!selected.has(row.item.id) &&
                      selected.size >= STUDIO_ERRORS_DISMISS_MAX_IDS)
                  }
                  onChange={(event) =>
                    onSelect(row.item.id, event.currentTarget.checked)
                  }
                />
                <Text size="sm">{row.item.reason}</Text>
                {row.item.subject && (
                  <Text size="xs" c="dimmed" lineClamp={2}>
                    {row.item.subject}
                  </Text>
                )}
                {row.item.detail && (
                  <Text size="xs" c="dimmed" lineClamp={2}>
                    {row.item.detail}
                  </Text>
                )}
                <details>
                  <summary>Details</summary>
                  <pre className="studio-error-details">
                    {JSON.stringify(row.item, null, 2)}
                  </pre>
                </details>
                <Group gap="xs" mt="xs">
                  {period(row.item) && (
                    <Text size="xs" c="dimmed">
                      {period(row.item)}
                    </Text>
                  )}
                  {row.item.action !== "details" && (
                    <Button
                      size="xs"
                      variant="light"
                      loading={busy === row.item.id}
                      disabled={busy !== null}
                      onClick={() => onAction(row.item)}
                    >
                      {actionLabel(row.item)}
                    </Button>
                  )}
                </Group>
              </article>
            ),
          )}
        </Stack>
      )}
    </Stack>
  );
}

/** Reads and mutations stay on the active server, including when the trigger is portalled. */
export function useStudioErrorsState(
  sourceItems: StudioErrorItem[] = EMPTY_ERRORS,
) {
  const { get, post } = useServerApi();
  const [items, setItems] = useState<StudioErrorItem[]>(sourceItems);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const pending = useRef(false);
  const generation = useRef(0);
  const refresh = useCallback(async () => {
    const read = ++generation.current;
    const value = await get("/api/studio-errors");
    if (read === generation.current) setItems(value.items);
  }, [get]);
  useEffect(() => {
    // Complete entity batches own membership; HTTP readback is only for explicit actions.
    generation.current++;
    setItems(sourceItems);
  }, [sourceItems]);
  const select = (id: string, checked: boolean) => {
    if (pending.current) return;
    setSelected((previous) => {
      const next = new Set(previous);
      if (checked && next.size < STUDIO_ERRORS_DISMISS_MAX_IDS) next.add(id);
      else if (!checked) next.delete(id);
      return next;
    });
  };
  const selectAll = (ids: string[]) => {
    if (!pending.current)
      setSelected(new Set(ids.slice(0, STUDIO_ERRORS_DISMISS_MAX_IDS)));
  };
  const remove = async () => {
    if (pending.current || !selected.size) return;
    const ids = [...selected];
    pending.current = true;
    generation.current++;
    setBusy("delete");
    setError("");
    setNotice("");
    try {
      const value = await post("/api/studio-errors/dismiss", { ids });
      setItems(value.items);
      setSelected(new Set());
      if (value.unknownIds.length)
        setNotice(
          `${value.unknownIds.length} selected errors were already gone.`,
        );
    } catch (failure) {
      setError(
        `Could not confirm deletion of ${ids.join(", ")}: ${errorText(failure)}`,
      );
      // A lost response is not proof of failure. Keep selection and read back; never resend.
      try {
        await refresh();
      } catch (readFailure) {
        setError(
          (message) =>
            `${message}. Could not refresh: ${errorText(readFailure)}`,
        );
      }
    } finally {
      pending.current = false;
      setBusy(null);
    }
  };
  const resume = async (item: StudioErrorItem) => {
    if (item.action !== "resume") return;
    if (pending.current) return;
    pending.current = true;
    generation.current++;
    setBusy(item.id);
    setError("");
    try {
      await post("/api/connection-recovery", { id: item.agentId });
      await refresh();
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      pending.current = false;
      setBusy(null);
    }
  };
  return {
    items,
    selected,
    busy,
    error,
    notice,
    refresh: () => {
      if (!pending.current)
        void refresh().catch((failure) => setError(errorText(failure)));
    },
    select,
    selectAll,
    remove,
    resume,
  };
}

const StudioErrorsContext = createContext<ReturnType<
  typeof useStudioErrorsState
> | null>(null);

export const StudioErrorsScope = StudioErrorsContext.Provider;

/** One owner per server, above chat keys and sidebar portals, retains pending mutations. */
export function StudioErrorsProvider({
  children,
  items = EMPTY_ERRORS,
}: {
  children?: ReactNode;
  items?: StudioErrorItem[];
}) {
  const state = useStudioErrorsState(items);
  return (
    <StudioErrorsContext.Provider value={state}>
      {children}
    </StudioErrorsContext.Provider>
  );
}

export function useStudioErrors() {
  const state = useContext(StudioErrorsContext);
  if (!state)
    throw new Error("Studio errors require their server-scoped provider.");
  return state;
}
