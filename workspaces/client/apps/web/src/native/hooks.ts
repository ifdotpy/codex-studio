import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { displayError } from "../errorPresentation";
import { ApiSchemaMismatchError } from "../apiErrors";
import {
  initialSnapshotProjectionStatus,
  snapshotProjectionError,
  snapshotProjectionStatusReducer,
  type SnapshotProjectionStatusAction,
} from "../hooks/snapshotProjectionStatus";
import { useCommittedCallback } from "../hooks/presentation";
import type { Agent } from "../types";
import type { UiPreferenceProjection } from "../sync/entityProjection";
import type { NativeSnapshot } from "./sync";
import {
  useServerApi,
  useServerSync,
  useServerServices,
} from "./servicesContext";
import { createNativeMessageView, type NativeMessageView } from "./messageView";

export {
  useCommittedCallback,
  useRetainedArray,
  useRetainedUsageAccounts,
  useChatSnapshot,
  transcriptMessages,
} from "../hooks/presentation";

/** The native owner publishes committed state. This hook owns its display only. */
export function useSnapshot(
  acceptUiPreferences?: (preferences: UiPreferenceProjection) => void,
) {
  const api = useServerApi();
  const sync = useServerSync();
  const owner = useMemo(() => ({ api, sync }), [api, sync]);
  const currentOwner = useRef(owner);
  currentOwner.current = owner;
  const activeOwner = useRef<typeof owner | null>(null);
  const generation = useRef(0);
  const [projection, setProjection] = useState<{
    owner: typeof owner;
    data: NativeSnapshot | null;
    workspaceId: string;
  }>({ owner, data: null, workspaceId: "" });
  const data = projection.owner === owner ? projection.data : null;
  const workspaceId = projection.owner === owner ? projection.workspaceId : "";
  const initialCreated = useMemo(() => {
    const saved = api.saved<{ scope: string; agents: Agent[] } | null>(
      "codex-confirmed-chats",
      null,
    );
    const retained =
      saved && typeof saved.scope === "string" && Array.isArray(saved.agents)
        ? saved
        : { scope: "", agents: [] };
    return { owner, ...retained };
  }, [owner]);
  const [created, setCreated] = useState(initialCreated);
  const [status, setStatus] = useState({
    owner,
    value: initialSnapshotProjectionStatus,
  });
  const statusRef = useRef(status);
  const dispatch = useCallback(
    (action: SnapshotProjectionStatusAction) => {
      if (currentOwner.current !== owner || activeOwner.current !== owner)
        return;
      const previous =
        statusRef.current.owner === owner
          ? statusRef.current.value
          : initialSnapshotProjectionStatus;
      const value = snapshotProjectionStatusReducer(previous, action);
      if (statusRef.current.owner === owner && value === previous) return;
      const next = { owner, value };
      statusRef.current = next;
      setStatus(next);
    },
    [owner],
  );
  const onUiPreferences = useCommittedCallback(
    acceptUiPreferences ?? (() => {}),
  );
  const scope = data?.stateDir || "";
  const currentScope = useRef({ owner, scope });
  currentScope.current = { owner, scope };
  const rememberCreated = useCallback(
    (agent: Agent, expectedScope: string) => {
      if (
        currentScope.current.owner !== owner ||
        currentScope.current.scope !== expectedScope
      )
        throw new Error("The workspace changed before the new chat opened.");
      setCreated((old) => ({
        owner,
        scope: expectedScope,
        agents: [
          ...(old.owner === owner && old.scope === expectedScope
            ? old.agents
            : []
          ).filter((row) => row.id !== agent.id),
          agent,
        ],
      }));
    },
    [owner],
  );
  const forgetCreated = useCallback(
    (ids: string[]) => {
      if (currentOwner.current !== owner) return;
      setCreated((old) =>
        old.owner !== owner
          ? old
          : {
              ...old,
              agents: old.agents.filter((agent) => !ids.includes(agent.id)),
            },
      );
    },
    [owner],
  );
  useEffect(() => {
    setCreated(initialCreated);
  }, [initialCreated]);
  useEffect(() => {
    if (!data || created.owner !== owner || !created.agents.length) return;
    setCreated((old) => {
      if (old.owner !== owner) return old;
      if (old.scope !== scope) return { owner, scope, agents: [] };
      const remaining = old.agents.filter(
        (agent) => !data.threads.some((row) => row.id === agent.id),
      );
      return remaining.length === old.agents.length
        ? old
        : { owner, scope, agents: remaining };
    });
  }, [owner, data, scope, created]);
  useEffect(() => {
    if (created.owner === owner)
      api.save("codex-confirmed-chats", {
        scope: created.scope,
        agents: created.agents,
      });
  }, [owner, created]);
  const visibleData = useMemo(() => {
    if (
      !data ||
      created.owner !== owner ||
      created.scope !== scope ||
      !created.agents.length
    )
      return data;
    const missing = created.agents.filter(
      (agent) => !data.threads.some((row) => row.id === agent.id),
    );
    if (!missing.length) return data;
    return {
      ...data,
      threads: [...data.threads, ...missing],
      runtime: {
        ...data.runtime,
        agents: [
          ...data.runtime.agents.filter(
            (agent) => !missing.some((row) => row.id === agent.id),
          ),
          ...missing,
        ],
      },
    };
  }, [owner, data, created, scope]);
  const refresh = useCallback(
    async (
      _credentialsOnly = false,
      onProjection?: (next: NativeSnapshot, workspaceId: string) => void,
      _resetRecovery = true,
    ): Promise<void> => {
      if (currentOwner.current !== owner || activeOwner.current !== owner)
        return;
      const request = ++generation.current;
      let resultWorkspaceId = "";
      try {
        const next = await sync.refreshProjection({
          onWorkspaceId: (id) => {
            resultWorkspaceId = id;
          },
        });
        if (currentOwner.current !== owner || activeOwner.current !== owner)
          return;
        // A mutation readback keeps its exact result when a newer render wins.
        onProjection?.(next, resultWorkspaceId);
        if (request !== generation.current) return;
        setProjection({ owner, data: next, workspaceId: resultWorkspaceId });
        dispatch({ type: "data-received" });
      } catch (error) {
        if (request !== generation.current) return;
        dispatch({
          type: "projection-failed",
          error:
            error instanceof ApiSchemaMismatchError ? "" : displayError(error),
        });
      }
    },
    [owner, sync, dispatch],
  );
  useEffect(() => {
    activeOwner.current = owner;
    let stopped = false;
    const current = () => !stopped && currentOwner.current === owner;
    const stop = sync.subscribeStateProjection(
      (next, identity) => {
        if (!current()) return;
        generation.current++;
        setProjection({ owner, data: next, workspaceId: identity });
        if (next) dispatch({ type: "data-received" });
      },
      (error) => {
        if (!current()) return;
        dispatch(
          error === null
            ? { type: "projection-recovered" }
            : {
                type: "projection-failed",
                error:
                  error instanceof ApiSchemaMismatchError
                    ? ""
                    : displayError(error),
              },
        );
      },
      acceptUiPreferences
        ? (preferences) => {
            if (current()) onUiPreferences(preferences);
          }
        : undefined,
    );
    void sync.workspaceId
      .then((identity) => {
        if (!current()) return;
        setProjection((previous) =>
          previous.owner === owner
            ? { ...previous, workspaceId: identity }
            : { owner, data: null, workspaceId: identity },
        );
      })
      .catch((error) => {
        if (current())
          dispatch({ type: "startup-failed", error: displayError(error) });
      });
    return () => {
      stopped = true;
      generation.current++;
      if (activeOwner.current === owner) activeOwner.current = null;
      stop();
    };
  }, [owner, sync, dispatch, onUiPreferences, !!acceptUiPreferences]);
  return {
    data: visibleData,
    error: status.owner === owner ? snapshotProjectionError(status.value) : "",
    refresh,
    workspaceId,
    rememberCreated,
    forgetCreated,
    creationScope: scope,
  };
}

/** Conversation state and navigation belong to the server-scoped Rust view. */
export function useMessages(
  id: string | null,
  kind: "agent" | "room" | "legacy",
  managed = false,
  workspace = "",
  syncWorkspaceId = "",
  enabled = true,
) {
  const { context } = useServerServices();
  const source = useMemo(
    () => ({ id, kind, managed, enabled, syncWorkspaceId }),
    [id, kind, managed, enabled, workspace, syncWorkspaceId],
  );
  const currentSource = useRef(source);
  currentSource.current = source;
  const committedSource = useRef(source);
  const [received, setReceived] = useState<{
    context: typeof context;
    source: typeof source;
    view: NativeMessageView;
  } | null>(null);
  const [failure, setFailure] = useState<{
    context: typeof context;
    source: typeof source;
    error: string;
  } | null>(null);
  const controller = useMemo(
    () =>
      createNativeMessageView(
        context,
        (view) => {
          setReceived({ context, source: committedSource.current, view });
          setFailure({ context, source: committedSource.current, error: "" });
        },
        (error) => {
          setFailure({
            context,
            source: committedSource.current,
            error: displayError(error),
          });
        },
      ),
    [context],
  );
  useEffect(() => {
    committedSource.current = source;
    void controller.select(source).catch((error) => {
      if (currentSource.current === source)
        setFailure({ context, source, error: displayError(error) });
    });
  }, [controller, source]);
  useEffect(
    () => () => {
      void controller.close().catch(() => {});
    },
    [controller],
  );
  const run = useCallback(
    async (
      command: Parameters<typeof controller.command>[0],
      params: Record<string, unknown> = {},
    ) => {
      if (currentSource.current !== source) return undefined;
      try {
        return await controller.command(command, params);
      } catch (error) {
        if (currentSource.current === source)
          setFailure({ context, source, error: displayError(error) });
        throw error;
      }
    },
    [controller, source, context],
  );
  const view =
    received?.context === context && received.source === source
      ? received.view
      : null;
  const error =
    failure?.context === context && failure.source === source
      ? failure.error
      : "";
  return {
    loaded: !id || !!view?.loaded,
    items: view?.items ?? [],
    notice: error || view?.notice || "",
    before: view?.before ?? null,
    after: view?.after ?? null,
    historyVersion: view?.historyVersion ?? "",
    historical: view?.historical ?? false,
    pageLoading: view?.pageLoading ?? false,
    liveAgent: view?.liveAgent ?? null,
    connection: view?.connection ?? "",
    older: async () => {
      await run("older");
    },
    newer: async () => {
      await run("newer");
    },
    showLatest: () => {
      void run("latest").catch(() => {});
    },
    setPageAnchor: (anchor: string | null) => {
      void run("anchor", { anchor }).catch(() => {});
    },
    ensureMessage: async (messageId: string) =>
      (await run("ensure", { messageId }))?.found === true,
    reload: async () => {
      await run("refresh");
    },
  };
}
