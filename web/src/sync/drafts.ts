import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type SetStateAction,
} from "react";
import { saved, save } from "../api";
import {
  startDraftReplication,
  syncDatabase,
  UnsupportedSyncError,
} from "./client";

import { encodeDraftPayload } from "./draftPayload";
import {
  readDraftJournal,
  writeDraftJournal,
  forgetDraftJournal,
  journalPrefix,
  type PendingDraft,
} from "./draftJournal";
import { onResume } from "./resume";

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
  const [writer] = useState(() => crypto.randomUUID());
  const [recovery] = useState(() => {
    try {
      return { entries: readDraftJournal(storageKey.current), error: "" };
    } catch {
      return {
        entries: [] as PendingDraft[],
        error: "Saved drafts could not be read. Keep this chat open.",
      };
    }
  });
  const journal = useRef(
    new Map(recovery.entries.map((entry) => [entry.key, entry])),
  );
  const [drafts, update] = useState<Drafts>(() => {
    const next: Drafts = saved(
      storageKey.current,
      storageKey.current.endsWith(":legacy")
        ? saved("codex-agent-drafts", {})
        : {},
    );
    for (const entry of recovery.entries)
      next[entry.version.session] = entry.version.text;
    return next;
  });
  const importLegacy = useRef(storageKey.current.endsWith(":legacy"));
  const current = useRef(drafts);
  const [conflicts, setConflicts] = useState<DraftVersion[]>([]);
  const conflictValues = useRef<DraftVersion[]>([]);
  const [localError, setLocalError] = useState(recovery.error);
  const [syncFailed, setSyncFailed] = useState(false);
  const localErrorValue = useRef(recovery.error);
  const syncFailureValue = useRef(false);
  const reportLocalError = useCallback((message: string) => {
    if (localErrorValue.current === message) return;
    localErrorValue.current = message;
    setLocalError(message);
  }, []);
  const reportSyncFailure = useCallback((failed: boolean) => {
    if (syncFailureValue.current === failed) return;
    syncFailureValue.current = failed;
    setSyncFailed(failed);
  }, []);
  const [syncNotice, setSyncNotice] = useState("");
  useEffect(() => {
    if (!syncFailed) {
      setSyncNotice("");
      return;
    }
    // Brief network interruptions recover without moving the conversation.
    const timer = setTimeout(
      () => setSyncNotice("Draft sync paused. Retrying automatically."),
      8000,
    );
    return () => clearTimeout(timer);
  }, [syncFailed]);
  const versions = useRef<DraftVersion[]>([]);
  const decoded = useRef(new WeakMap<object, DraftVersion>());
  const decodeDrafts = useCallback(
    (docs: any[]): DraftVersion[] =>
      docs.map((doc) => {
        const latest = doc.getLatest();
        let version = decoded.current.get(latest);
        if (!version) {
          version = JSON.parse(latest.payload) as DraftVersion;
          decoded.current.set(latest, version);
        }
        return version;
      }),
    [],
  );
  const dismissed = useRef<string[]>(
    saved(`${storageKey.current}:dismissed`, []),
  );
  const versionKey = (version: DraftVersion) =>
    JSON.stringify([version.id, version.text]);
  const reconcile = useCallback((records: DraftVersion[]) => {
    versions.current = records;
    let next = current.current;
    const alternatives: DraftVersion[] = [];
    const sessions = new Map<string, DraftVersion[]>();
    for (const version of records) {
      const branches = sessions.get(version.session);
      if (branches) branches.push(version);
      else sessions.set(version.session, [version]);
    }
    const dismissedKeys = new Set(dismissed.current);
    for (const [session, branches] of sessions) {
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
        (version) =>
          !dismissedKeys.size || !dismissedKeys.has(versionKey(version)),
      );
      if (
        chosen &&
        !pendingEdits.current.has(session) &&
        next[session] !== chosen.text
      ) {
        if (next === current.current) next = { ...next };
        next[session] = chosen.text;
      }
      if (pendingEdits.current.has(session)) continue;
      for (const branch of active) {
        if (branch.text && branch.text !== next[session])
          alternatives.push(branch);
        for (const text of branch.alternatives || [])
          if (text && text !== next[session])
            alternatives.push({ ...branch, id: `${branch.id}:conflict`, text });
      }
    }
    if (next !== current.current) {
      current.current = next;
      update(next);
      save(storageKey.current, next);
    }
    const unique = new Set<string>();
    const nextConflicts = alternatives.filter((version) => {
      const key = versionKey(version);
      if (dismissedKeys.has(key) || unique.has(key)) return false;
      unique.add(key);
      return true;
    });
    const previousConflicts = conflictValues.current;
    if (
      previousConflicts.length !== nextConflicts.length ||
      previousConflicts.some(
        (version, index) => version !== nextConflicts[index],
      )
    ) {
      conflictValues.current = nextConflicts;
      setConflicts(nextConflicts);
    }
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
  const flushing = useRef<Promise<void> | null>(null);
  const unsupported = useRef(false);
  const pendingEdits = useRef(
    new Map(
      recovery.entries.map(({ version }) => [version.session, version.updated]),
    ),
  );
  const editSequence = useRef(0);
  const localHeads = useRef(new Map<string, number>());
  const entries = () =>
    [...journal.current.values()]
      .filter((entry) =>
        entry.key.startsWith(journalPrefix(storageKey.current)),
      )
      .sort(
        (a, b) =>
          a.version.updated - b.version.updated || a.key.localeCompare(b.key),
      );
  const markPending = () => {
    pendingEdits.current = new Map(
      entries().map(({ version }) => [version.session, version.updated]),
    );
  };
  const adoptScope = useCallback((workspaceId: string) => {
    const targetKey = `codex-drafts:${workspaceId}`;
    if (storageKey.current === targetKey) return;
    const previousKey = storageKey.current;
    const legacy = previousKey.endsWith(":legacy");
    const next: Drafts = legacy ? { ...current.current } : saved(targetKey, {});
    if (legacy) {
      for (const entry of entries()) {
        const moved = {
          ...entry,
          key: targetKey + entry.key.slice(previousKey.length),
        };
        writeDraftJournal(moved);
        forgetDraftJournal(entry);
        journal.current.delete(entry.key);
        journal.current.set(moved.key, moved);
      }
    }
    storageKey.current = targetKey;
    for (const entry of readDraftJournal(targetKey))
      journal.current.set(entry.key, entry);
    markPending();
    for (const entry of entries())
      next[entry.version.session] = entry.version.text;
    dismissed.current = saved(`${targetKey}:dismissed`, []);
    current.current = next;
    update(next);
    save(targetKey, next);
  }, []);
  const flushDrafts = useCallback(() => {
    if (unsupported.current) return Promise.resolve();
    if (flushing.current) return flushing.current;
    flushing.current = (async () => {
      let connecting = false;
      try {
        for (const entry of readDraftJournal(storageKey.current)) {
          const old = journal.current.get(entry.key);
          if (!old || entry.version.updated > old.version.updated)
            journal.current.set(entry.key, entry);
        }
        markPending();
        if (!entries().length) return;
        connecting = true;
        const { db, workspaceId } = await syncDatabase();
        connecting = false;
        adoptScope(workspaceId);
        for (;;) {
          const batch = entries();
          if (!batch.length) break;
          for (const entry of batch) {
            const item = entry.version;
            const old = await db.drafts.findOne(item.id).exec();
            const previous: DraftVersion | undefined =
              old && JSON.parse(old.payload);
            // A recovered older edit must not replace a newer branch from another tab.
            const newer = previous && previous.updated > item.updated;
            const chosen = newer ? previous : item;
            const other = newer ? item : previous;
            const alternatives = [
              ...new Set([
                ...(previous?.alternatives || []),
                ...(item.alternatives || []),
                ...(other &&
                other.text !== chosen.text &&
                other.text &&
                (chosen.seen?.[other.id] ?? -1) < other.updated
                  ? [other.text]
                  : []),
              ]),
            ].filter((text) => text !== chosen.text);
            const payload = encodeDraftPayload({ ...chosen, alternatives });
            if (!old || old.payload !== payload)
              await db.drafts.incrementalUpsert({
                id: item.id,
                payload,
                seq: 0,
              });
            forgetDraftJournal(entry);
            if (
              journal.current.get(entry.key)?.version.updated === item.updated
            )
              journal.current.delete(entry.key);
            markPending();
          }
        }
        reportLocalError("");
        reconcile(decodeDrafts(await db.drafts.find().exec()));
      } catch (error) {
        if (error instanceof UnsupportedSyncError) unsupported.current = true;
        else if (connecting) reportSyncFailure(true);
        else
          reportLocalError(
            "Draft changes could not be saved for synchronization. Keep this chat open.",
          );
      }
    })().finally(() => {
      flushing.current = null;
    });
    return flushing.current;
  }, [
    adoptScope,
    reconcile,
    decodeDrafts,
    reportLocalError,
    reportSyncFailure,
  ]);
  const setDrafts = useCallback(
    (value: SetStateAction<Drafts>) => {
      const previous = current.current;
      const next = typeof value === "function" ? value(previous) : value;
      current.current = next;
      update(next);
      const updated = Math.max(
        Date.now(),
        ...versions.current.map((version) => version.updated + 1),
        ...entries().map((entry) => entry.version.updated + 1),
        editSequence.current + 1,
      );
      editSequence.current = updated;
      for (const session of new Set([
        ...Object.keys(previous),
        ...Object.keys(next),
      ])) {
        if (previous[session] === next[session]) continue;
        const version: DraftVersion = {
          id: `${device}:${session}`,
          session,
          device,
          text: next[session] || "",
          updated,
          seen: Object.fromEntries(
            [
              ...versions.current
                .filter((version) => version.session === session)
                .flatMap((version) => [
                  [version.id, version.updated] as const,
                  ...Object.entries(version.seen || {}),
                ]),
              ...entries()
                .filter((entry) => entry.version.session === session)
                .map(
                  (entry) => [entry.version.id, entry.version.updated] as const,
                ),
              [
                `${device}:${session}`,
                localHeads.current.get(session) || 0,
              ] as const,
            ].sort((a, b) => a[1] - b[1]),
          ),
        };
        localHeads.current.set(session, updated);
        const entry = {
          key: `${journalPrefix(storageKey.current)}${writer}:${encodeURIComponent(session)}`,
          version,
        };
        journal.current.set(entry.key, entry);
        pendingEdits.current.set(session, updated);
        try {
          writeDraftJournal(entry);
        } catch {
          reportLocalError(
            "Draft changes are not saved yet. Keep this chat open.",
          );
        }
      }
      save(storageKey.current, next);
      void flushDrafts();
    },
    [flushDrafts, writer, reportLocalError],
  );
  useEffect(() => {
    let stopped = false,
      unsubscribe = () => {},
      cancel = () => {};
    let retry: ReturnType<typeof setTimeout> | undefined;
    let starting = false,
      started = false;
    const start = () => {
      if (stopped || starting || started || unsupported.current) return;
      clearTimeout(retry);
      starting = true;
      void syncDatabase()
        .then(async ({ db, workspaceId }) => {
          if (stopped) return;
          adoptScope(workspaceId);
          await flushDrafts();
          cancel = await startDraftReplication((e) => {
            if (!stopped && !(e instanceof UnsupportedSyncError))
              reportSyncFailure(e !== null);
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
            reconcile(decodeDrafts(docs));
          });
          unsubscribe = () => sub.unsubscribe();
          started = true;
          importLegacy.current = false;
        })
        .catch((e) => {
          if (e instanceof UnsupportedSyncError) unsupported.current = true;
          if (!stopped && !(e instanceof UnsupportedSyncError)) {
            reportSyncFailure(true);
            unsubscribe();
            cancel();
            retry = setTimeout(start, 3000);
          }
        })
        .finally(() => {
          starting = false;
        });
    };
    start();
    const stopResume = onResume(() => {
      start();
      void flushDrafts();
    });
    const writeTimer = setInterval(() => void flushDrafts(), 3000);
    return () => {
      stopped = true;
      clearTimeout(retry);
      clearInterval(writeTimer);
      stopResume();
      unsubscribe();
      cancel();
    };
  }, [reconcile, adoptScope, flushDrafts, decodeDrafts, reportSyncFailure]);
  return {
    drafts,
    setDrafts,
    conflicts,
    dismissDraft,
    error: localError || syncNotice,
  };
}
