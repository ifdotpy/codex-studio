import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type SetStateAction,
} from "react";
import { errorText, saved, save } from "../api";
import {
  startDraftReplication,
  syncDatabase,
  UnsupportedSyncError,
} from "./client";

import { encodeDraftPayload } from "./draftPayload";

type Drafts = Record<string, string>;
export type DraftVersion = {
  id: string;
  session: string;
  text: string;
  device: string;
  updated: number;
  alternatives?: string[];
};
let device = saved<string>("codex-draft-device", "");
if (!device) {
  device = crypto.randomUUID();
  save("codex-draft-device", device);
}
export function useSyncedDrafts() {
  const storageKey = useRef(
    `codex-drafts:${saved("codex-sync-workspace", "legacy")}`,
  );
  const [drafts, update] = useState<Drafts>(() =>
    saved(
      storageKey.current,
      storageKey.current.endsWith(":legacy")
        ? saved("codex-agent-drafts", {})
        : {},
    ),
  );
  const current = useRef(drafts);
  const [conflicts, setConflicts] = useState<DraftVersion[]>([]);
  const [error, setError] = useState("");
  const writes = useRef(Promise.resolve());
  const pendingEdits = useRef(new Map<string, number>());
  const editSequence = useRef(0);
  const setDrafts = useCallback((value: SetStateAction<Drafts>) => {
    const previous = current.current;
    const next = typeof value === "function" ? value(previous) : value;
    current.current = next;
    update(next);
    save(storageKey.current, next);
    const changes = [
      ...new Set([...Object.keys(previous), ...Object.keys(next)]),
    ]
      .filter((session) => previous[session] !== next[session])
      .map((session) => ({
        id: `${device}:${session}`,
        session,
        device,
        text: next[session] || "",
        updated: Date.now(),
      }));
    const sequence = ++editSequence.current;
    for (const item of changes)
      pendingEdits.current.set(item.session, sequence);
    writes.current = writes.current
      .then(async () => {
        const { db } = await syncDatabase();
        for (const item of changes) {
          const old = await db.drafts.findOne(item.id).exec();
          const alternatives = old ? JSON.parse(old.payload).alternatives : [];
          await db.drafts.incrementalUpsert({
            id: item.id,
            payload: encodeDraftPayload({ ...item, alternatives }),
            seq: 0,
          });
          if (pendingEdits.current.get(item.session) === sequence)
            pendingEdits.current.delete(item.session);
        }
      })
      .catch((e) => {
        if (!(e instanceof UnsupportedSyncError)) setError(errorText(e));
      });
  }, []);
  useEffect(() => {
    let stopped = false,
      unsubscribe = () => {},
      cancel = () => {};
    void syncDatabase()
      .then(async ({ db, workspaceId }) => {
        if (stopped) return;
        const targetKey = `codex-drafts:${workspaceId}`;
        if (storageKey.current !== targetKey) {
          const next: Drafts = storageKey.current.endsWith(":legacy")
            ? current.current
            : saved(targetKey, {});
          // Preserve edits made in this mount before IndexedDB opened.
          for (const session of pendingEdits.current.keys())
            next[session] = current.current[session];
          storageKey.current = targetKey;
          current.current = next;
          update(next);
          save(targetKey, next);
        }
        cancel = await startDraftReplication((e) => {
          if (!stopped && !(e instanceof UnsupportedSyncError))
            setError(errorText(e));
        });
        if (stopped) {
          cancel();
          return;
        }
        // Import the previous browser drafts without replacing a replicated branch.
        for (const [session, text] of Object.entries(current.current)) {
          const id = `${device}:${session}`;
          if (!(await db.drafts.findOne(id).exec()))
            await db.drafts.insert({
              id,
              seq: 0,
              payload: encodeDraftPayload({
                id,
                session,
                device,
                text,
                updated: Date.now(),
              }),
            });
        }
        if (stopped) return;
        const sub = db.drafts.find().$.subscribe((docs: any[]) => {
          const versions: DraftVersion[] = docs.map((doc) =>
            JSON.parse(doc.payload),
          );
          const next = { ...current.current };
          const alternatives: DraftVersion[] = [];
          const sessions = new Set(versions.map((v) => v.session));
          for (const session of sessions) {
            const branches = versions
              .filter((v) => v.session === session)
              .sort(
                (a, b) => b.updated - a.updated || a.id.localeCompare(b.id),
              );
            const own = branches.find((v) => v.device === device);
            const chosen = own || branches[0];
            if (!pendingEdits.current.has(session)) next[session] = chosen.text;
            alternatives.push(
              ...branches.filter(
                (v) => v.id !== chosen.id && v.text && v.text !== chosen.text,
              ),
            );
          }
          for (const branch of versions)
            for (const [index, text] of (branch.alternatives || []).entries()) {
              if (text && text !== next[branch.session])
                alternatives.push({
                  ...branch,
                  id: `${branch.id}:conflict:${index}`,
                  text,
                });
            }
          current.current = next;
          update(next);
          save(storageKey.current, next);
          setConflicts(alternatives);
        });
        unsubscribe = () => sub.unsubscribe();
      })
      .catch((e) => {
        if (!stopped && !(e instanceof UnsupportedSyncError))
          setError(errorText(e));
      });
    return () => {
      stopped = true;
      unsubscribe();
      cancel();
    };
  }, []);
  return { drafts, setDrafts, conflicts, error };
}
