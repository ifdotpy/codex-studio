import { useCallback, useRef, useState, type SetStateAction } from "react";
import type { DraftVersion } from "../generated/rustOwned";

type Drafts = Record<string, string>;

const conflicts: DraftVersion[] = [];
const dismissDraft = () => {};

/**
 * Drafts for a view whose core is not connected. Text stays in memory only, and
 * the view reports that nothing is saved. The browser has no draft store.
 */
export function useUnavailableDrafts() {
  const current = useRef<Drafts>({});
  const listeners = useRef(new Map<string, Set<() => void>>());
  const [, rerender] = useState(0);
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
  const setDrafts = useCallback((action: SetStateAction<Drafts>) => {
    const previous = current.current;
    const next = typeof action === "function" ? action(previous) : action;
    current.current = next;
    for (const [session, group] of listeners.current)
      if ((previous[session] ?? "") !== (next[session] ?? ""))
        for (const listener of group) listener();
    rerender((count) => count + 1);
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
    error: "Drafts are not available in this view yet. Changes are not saved.",
    localPersistenceFailed: true,
  };
}
