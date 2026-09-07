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
  seen?: Record<string, number>;
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
  const importLegacy = useRef(storageKey.current.endsWith(":legacy"));
  const current = useRef(drafts);
  const [conflicts, setConflicts] = useState<DraftVersion[]>([]);
  const [error, setError] = useState("");
  const versions = useRef<DraftVersion[]>([]);
  const dismissed = useRef<string[]>(
    saved(`${storageKey.current}:dismissed`, []),
  );
  const versionKey = (version: DraftVersion) =>
    JSON.stringify([version.id, version.text]);
  const reconcile = useCallback((records: DraftVersion[]) => {
    versions.current = records;
    const next = { ...current.current };
    const alternatives: DraftVersion[] = [];
    for (const session of new Set(records.map((version) => version.session))) {
      const branches = records.filter((version) => version.session === session);
      const active = branches
        .filter(
          (version) =>
            !branches.some(
              (other) =>
                other.id !== version.id &&
                (other.seen?.[version.id] ?? -1) >= version.updated,
            ),
        )
        .sort((a, b) => b.updated - a.updated || a.id.localeCompare(b.id));
      const chosen = active.find(
        (version) => !dismissed.current.includes(versionKey(version)),
      );
      if (chosen && !pendingEdits.current.has(session))
        next[session] = chosen.text;
      if (pendingEdits.current.has(session)) continue;
      for (const branch of active) {
        if (branch.text && branch.text !== next[session])
          alternatives.push(branch);
        for (const text of branch.alternatives || [])
          if (text && text !== next[session])
            alternatives.push({ ...branch, id: `${branch.id}:conflict`, text });
      }
    }
    current.current = next;
    update(next);
    save(storageKey.current, next);
    const unique = new Set<string>();
    setConflicts(
      alternatives.filter((version) => {
        const key = versionKey(version);
        if (dismissed.current.includes(key) || unique.has(key)) return false;
        unique.add(key);
        return true;
      }),
    );
  }, []);
  const dismissDraft = useCallback(
    (version: DraftVersion) => {
      dismissed.current = [
        ...new Set([...dismissed.current, versionKey(version)]),
      ];
      save(`${storageKey.current}:dismissed`, dismissed.current);
      reconcile(versions.current);
    },
    [reconcile],
  );
  const writes = useRef(Promise.resolve());
  const pendingEdits = useRef(new Map<string, number>());
  const editSequence = useRef(0);
  const setDrafts = useCallback((value: SetStateAction<Drafts>) => {
    const previous = current.current;
    const next = typeof value === "function" ? value(previous) : value;
    current.current = next;
    update(next);
    save(storageKey.current, next);
    const updated = Math.max(
      Date.now(),
      ...versions.current.map((version) => version.updated + 1),
      editSequence.current + 1,
    );
    const changes = [
      ...new Set([...Object.keys(previous), ...Object.keys(next)]),
    ]
      .filter((session) => previous[session] !== next[session])
      .map((session) => ({
        id: `${device}:${session}`,
        session,
        device,
        text: next[session] || "",
        updated,
        // An edit replaces only versions that this browser has already observed.
        seen: Object.fromEntries(
          versions.current
            .filter((version) => version.session === session)
            .flatMap((version) => [
              [version.id, version.updated] as const,
              ...Object.entries(version.seen || {}),
            ])
            .sort((a, b) => a[1] - b[1]),
        ),
      }));
    const sequence = (editSequence.current = updated);
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
          dismissed.current = saved(`${targetKey}:dismissed`, []);
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
        for (const [session, text] of Object.entries(
          importLegacy.current ? current.current : {},
        )) {
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
          reconcile(docs.map((doc) => JSON.parse(doc.payload)));
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
  }, [reconcile]);
  return { drafts, setDrafts, conflicts, dismissDraft, error };
}
