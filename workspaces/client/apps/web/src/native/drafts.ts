import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type SetStateAction,
} from "react";
import type { NativeServerContext } from "./client";
import { NativeDraftState, type DraftView } from "./draftState";
import { guardNativeClose } from "./closeState";
import type { DraftVersion } from "../sync/drafts";

type Drafts = Record<string, string>;
/** One immutable server context owns every local draft command and event. */
export function createNativeDrafts(context: NativeServerContext) {
  return function useNativeDrafts() {
    const state = useRef<NativeDraftState | null>(null);
    const drafts = useRef<Drafts>({});
    const early = useRef<Drafts>({});
    const listeners = useRef(new Map<string, Set<() => void>>());
    const [conflicts, setConflicts] = useState<DraftVersion[]>([]);
    const [error, setError] = useState("");
    const [pending, setPending] = useState(false);
    const publish = useCallback((next: Drafts) => {
      const previous = drafts.current;
      drafts.current = next;
      for (const [key, group] of listeners.current)
        if ((previous[key] ?? "") !== (next[key] ?? ""))
          for (const listener of group) listener();
    }, []);
    useEffect(() => {
      let stopped = false;
      let wasPending = false;
      let read: Promise<void> | undefined;
      let dirty = false;
      let push: Promise<void> | undefined;
      let pushDirty = false;
      let stopStream = async () => {};
      const current = new NativeDraftState(
        context.workspaceId,
        context.request,
      );
      state.current = current;
      const refresh = (): Promise<void> => {
        dirty = true;
        if (read) return read;
        read = (async () => {
          do {
            dirty = false;
            const view = await context.request<DraftView>("drafts.view", {
              workspace: context.workspaceId,
            });
            if (!stopped) current.accept(view);
          } while (dirty && !stopped);
        })()
          .catch((failure) => {
            if (!stopped) setError(String(failure));
          })
          .finally(() => {
            read = undefined;
          });
        return read;
      };
      const pushLocal = () => {
        pushDirty = true;
        if (push) return;
        push = (async () => {
          do {
            pushDirty = false;
            const view = await context.request<DraftView>("drafts.push", {
              workspace: context.workspaceId,
            });
            if (!stopped) current.accept(view);
          } while (pushDirty && !stopped);
        })()
          .catch((failure) => {
            if (!stopped) setError(String(failure));
          })
          .finally(() => {
            push = undefined;
          });
      };
      const stopState = current.subscribe(() => {
        if (stopped) return;
        const view = current.getSnapshot();
        publish(view.drafts);
        setConflicts(view.conflicts);
        setPending(view.pending);
        setError(
          view.localError
            ? "Draft changes are not saved yet. Keep this chat open. " +
                view.localError
            : view.error || "",
        );
        const committed = wasPending && !view.pending;
        wasPending = view.pending;
        if (committed) pushLocal();
      });
      const unguard = guardNativeClose(() =>
        Object.keys(early.current).length
          ? Promise.reject(new Error("Draft changes have not finished saving."))
          : current.waitCommitted(),
      );
      void refresh().then(() => {
        if (!stopped && Object.keys(early.current).length) {
          current.edit({ ...current.getSnapshot().drafts, ...early.current });
          early.current = {};
        }
      });
      void context
        .subscribe<{
          type?: string;
          snapshot?: { resource?: { kind?: string } };
          status?: string;
        }>("subscriptions", { resources: [{ kind: "drafts" }] }, (event) => {
          if (stopped) return;
          if (event.error) {
            setError(event.error.message);
            return;
          }
          if (
            event.value?.type === "resource" &&
            event.value.snapshot?.resource?.kind === "drafts"
          )
            void refresh();
          if (
            event.value?.type === "connection" &&
            event.value.status === "live"
          )
            void current.retry();
        })
        .then((stop) => {
          if (stopped) void stop();
          else stopStream = stop;
        })
        .catch((failure) => {
          if (!stopped) setError(String(failure));
        });
      return () => {
        stopped = true;
        stopState();
        unguard();
        void stopStream();
        state.current = null;
      };
    }, [publish]);
    const getDraft = useCallback(
      (session: string) => drafts.current[session] || "",
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
    const setDrafts = useCallback(
      (action: SetStateAction<Drafts>) => {
        const next =
          typeof action === "function" ? action(drafts.current) : action;
        if (state.current) state.current.edit(next);
        else {
          const changes = { ...early.current };
          for (const key of new Set([
            ...Object.keys(drafts.current),
            ...Object.keys(next),
          ]))
            if ((drafts.current[key] ?? "") !== (next[key] ?? ""))
              changes[key] = next[key] ?? "";
          early.current = changes;
          publish(next);
          setPending(true);
          setError("Draft changes are not saved yet. Keep this chat open.");
        }
      },
      [publish],
    );
    const dismissDraft = useCallback((version: DraftVersion) => {
      void context
        .request<DraftView>("drafts.dismiss", {
          workspace: context.workspaceId,
          version,
        })
        .then((view) => state.current?.accept(view))
        .catch((failure) => setError(String(failure)));
    }, []);
    return {
      get drafts() {
        return drafts.current;
      },
      setDrafts,
      getDraft,
      subscribeDraft,
      conflicts,
      dismissDraft,
      error,
      localPersistenceFailed: pending,
    };
  };
}
