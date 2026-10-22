import { importLegacyDrafts } from "./legacyDrafts";
import { nativeDraftJournal } from "./draftJournal";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type SetStateAction,
} from "react";
import type { DraftVersion } from "../sync/drafts";
import { onResume } from "../sync/resume";
import {
  watchResourceChanges,
  watchResourceConnection,
} from "../sync/resourceEvents";
import { nativeRequest } from "./client";
import { guardNativeClose } from "./closeState";
import { NativeDraftState, type DraftView } from "./draftState";

type Drafts = Record<string, string>;
export function useNativeDrafts() {
  const controller = useRef<NativeDraftState | null>(null);
  const current = useRef<Drafts>({});
  const early = useRef<Drafts>({});
  const refreshDrafts = useRef(() => {});
  const listeners = useRef(new Map<string, Set<() => void>>());
  const [conflicts, setConflicts] = useState<DraftVersion[]>([]);
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const publish = useCallback((next: Drafts) => {
    const previous = current.current;
    current.current = next;
    for (const [key, group] of listeners.current)
      if ((previous[key] ?? "") !== (next[key] ?? ""))
        for (const listener of group) listener();
  }, []);
  const getDraft = useCallback(
    (session: string) => current.current[session] || "",
    [],
  );
  const subscribeDraft = useCallback(
    (session: string, listener: () => void) => {
      let group = listeners.current.get(session);
      if (!group) listeners.current.set(session, (group = new Set()));
      group.add(listener);
      return () => {
        group?.delete(listener);
        if (!group?.size) listeners.current.delete(session);
      };
    },
    [],
  );
  useEffect(() => {
    let stopped = false;
    let wasPending = false;
    let running = false;
    let dirty = false;
    let unsubscribe = () => {};
    const unguard = guardNativeClose(async () => {
      if (Object.keys(early.current).length)
        throw new Error(
          "Wait for the workspace. Draft changes are not saved yet.",
        );
      await controller.current?.waitCommitted();
    });
    const update = () => {
      if (stopped || !controller.current) return;
      const view = controller.current.getSnapshot();
      publish(view.drafts);
      setConflicts(view.conflicts);
      setPending(view.pending);
      const committed = wasPending && !view.pending;
      wasPending = view.pending;
      if (committed) refreshDrafts.current();
      setError(
        view.localError
          ? "Draft changes are not saved yet. Keep this chat open. " +
              view.localError
          : view.error || "",
      );
    };
    const refresh = async () => {
      if (stopped) return;
      dirty = true;
      if (running) return;
      running = true;
      // Coalesce notifications and reconnect baselines delivered together.
      await Promise.resolve();
      dirty = false;
      try {
        if (stopped) return;
        if (!controller.current) {
          // Recover local drafts before contacting the server. A server outage
          // must not hide already committed text or prevent another local edit.
          const cached = await nativeRequest<{ workspaceId: string } | null>(
            "identity.get",
            {},
          );
          const identity =
            cached ??
            (await nativeRequest<{ workspaceId: string }>("sync.open", {}));
          if (stopped) return;
          await importLegacyDrafts(identity.workspaceId, nativeRequest);
          if (stopped) return;
          const state = new NativeDraftState(
            identity.workspaceId,
            nativeRequest,
            nativeDraftJournal(identity.workspaceId),
          );
          const view = await nativeRequest<DraftView>("drafts.view", {
            workspace: identity.workspaceId,
          });
          if (stopped) return;
          state.accept(view);
          controller.current = state;
          unsubscribe = state.subscribe(update);
          if (Object.keys(early.current).length)
            state.edit({ ...state.getSnapshot().drafts, ...early.current });
          early.current = {};
          update();
          // Recover exact requests before a network pull can replace their text.
          await state.retry();
        }
        const state = controller.current;
        const view = await nativeRequest<DraftView>("drafts.refresh", {
          workspace: state.workspace,
        });
        if (!stopped) state.accept(view);
      } catch (failure) {
        if (!stopped)
          setError(
            failure instanceof Error ? failure.message : String(failure),
          );
      } finally {
        running = false;
        if (dirty && !stopped) void refresh();
      }
    };
    refreshDrafts.current = () => {
      void refresh();
    };
    const stopChanges = watchResourceChanges(
      { kind: "drafts" },
      refreshDrafts.current,
    );
    void refresh();
    let connected = false;
    const stopConnection = watchResourceConnection((status) => {
      const live = status === "live";
      if (live && !connected) void refresh();
      connected = live;
    });
    const resume = onResume(() => {
      void controller.current?.retry();
      void refresh();
    });
    return () => {
      stopped = true;
      unguard();
      stopChanges();
      stopConnection();
      refreshDrafts.current = () => {};
      resume();
      unsubscribe();
      controller.current = null;
    };
  }, [publish]);
  const setDrafts = useCallback(
    (action: SetStateAction<Drafts>) => {
      const next =
        typeof action === "function" ? action(current.current) : action;
      if (controller.current) {
        controller.current.edit(next);
      } else {
        for (const key of new Set([
          ...Object.keys(current.current),
          ...Object.keys(next),
        ]))
          if ((current.current[key] ?? "") !== (next[key] ?? ""))
            early.current[key] = next[key] ?? "";
        publish(next);
        setPending(Object.keys(early.current).length > 0);
        setError("Wait for the workspace. Draft changes are not saved yet.");
      }
    },
    [publish],
  );
  const dismissDraft = useCallback((version: DraftVersion) => {
    const state = controller.current;
    if (!state) return;
    // TODO(attar-sync): Return to synchronous core.commit for this local write.
    void nativeRequest<DraftView>("drafts.dismiss", {
      workspace: state.workspace,
      version,
    })
      .then((view) => {
        state.accept(view);
        refreshDrafts.current();
      })
      .catch((failure) =>
        setError(failure instanceof Error ? failure.message : String(failure)),
      );
  }, []);
  return {
    get drafts() {
      return current.current;
    },
    setDrafts,
    getDraft,
    subscribeDraft,
    conflicts,
    dismissDraft,
    error,
    localPersistenceFailed: pending,
  };
}
