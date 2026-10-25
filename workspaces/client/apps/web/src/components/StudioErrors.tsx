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
import { Button, Checkbox, Group, Stack, Text } from "@mantine/core";
import { errorText, type GetResult } from "../api";
import { STUDIO_ERRORS_DISMISS_MAX_IDS } from "../generated/sharedValues";

export type StudioErrorItem = GetResult<"/api/studio-errors">["items"][number];
const EMPTY_ERRORS: StudioErrorItem[] = [];

export function actionLabel(item: StudioErrorItem) {
  return item.action === "resume" ? "Resume" : "Open";
}
function when(at: number) {
  return at > 0 ? new Date(at * 1000).toLocaleString() : "";
}

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
  onSelectAll: () => void;
  onDelete: () => void;
  onAction: (item: StudioErrorItem) => void;
}) {
  return (
    <Stack gap="sm">
      <Group gap="xs">
        <Button
          size="xs"
          variant="default"
          disabled={busy !== null || items.length === 0}
          onClick={onSelectAll}
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
      {items.length === 0 ? (
        <Text size="sm">No agent work ended because of a Studio error.</Text>
      ) : (
        <Stack gap="sm" aria-label="Studio errors">
          {items.map((item) => (
            <article
              className="studio-error-row"
              key={item.id}
              data-kind={item.kind}
            >
              <Checkbox
                label={item.agentName}
                aria-label={`Select error ${item.id} for ${item.agentName}`}
                checked={selected.has(item.id)}
                disabled={
                  busy !== null ||
                  (!selected.has(item.id) &&
                    selected.size >= STUDIO_ERRORS_DISMISS_MAX_IDS)
                }
                onChange={(event) =>
                  onSelect(item.id, event.currentTarget.checked)
                }
              />
              <Text size="sm">{item.reason}</Text>
              {item.subject && (
                <Text size="xs" c="dimmed" lineClamp={2}>
                  {item.subject}
                </Text>
              )}
              {item.detail && (
                <Text size="xs" c="dimmed" lineClamp={2}>
                  {item.detail}
                </Text>
              )}
              <Group gap="xs" mt="xs">
                {when(item.at) && (
                  <Text size="xs" c="dimmed">
                    {when(item.at)}
                  </Text>
                )}
                <Button
                  size="xs"
                  variant="light"
                  loading={busy === item.id}
                  disabled={busy !== null}
                  onClick={() => onAction(item)}
                >
                  {actionLabel(item)}
                </Button>
              </Group>
            </article>
          ))}
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
  const selectAll = () => {
    if (!pending.current)
      setSelected(
        new Set(
          items.slice(0, STUDIO_ERRORS_DISMISS_MAX_IDS).map((item) => item.id),
        ),
      );
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
