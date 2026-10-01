import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type SetStateAction,
} from "react";
import { saved, save } from "../api";
import { startDraftReplication, syncDatabase } from "./client";

import { encodeDraftPayload } from "./draftPayload";
import {
  readDraftJournal,
  writeDraftJournal,
  forgetDraftJournal,
  journalPrefix,
  type PendingDraft,
} from "./draftJournal";
import { onResume } from "./resume";
import { draftVersionId } from "./draftIdentity";
import { activeDraftVersions } from "./draftVersions";
import {
  copyLocalDraftScope,
  readLocalDraftRecord,
  readDraftsWithLegacyFallback,
  migrateLegacyDrafts,
  legacyBaselineHash,
  writeLocalDraftRecord,
} from "./draftStorage";

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
    `codex-drafts:${saved("codex-sync-workspace", "unassigned")}`,
  );
  const localRecovery = useState(() => {
    let error = "";
    let imported: Record<string, string | null> = {};
    try {
      imported = migrateLegacyDrafts(storageKey.current, () => {
        error = "Some saved drafts could not be read. Keep this chat open.";
      });
    } catch {
      error = "Saved drafts could not be migrated. Keep this chat open.";
    }
    const drafts = readDraftsWithLegacyFallback(storageKey.current, () => {
      error = "Some saved drafts could not be read. Keep this chat open.";
    });
    return { drafts, error, imported };
  })[0];
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
    const next: Drafts = { ...localRecovery.drafts };
    for (const entry of recovery.entries) {
      if (entry.version.id === `${device}:${entry.version.session}`) {
        try {
          if (readLocalDraftRecord(storageKey.current, entry.version.session))
            continue;
        } catch {
          // The pending journal remains a recovery source if this record is corrupt.
        }
      }
      next[entry.version.session] = entry.version.text;
    }
    return next;
  });
  const importUnassigned = useRef(storageKey.current.endsWith(":unassigned"));
  const current = useRef(drafts);
  const draftListeners = useRef(new Map<string, Set<() => void>>());
  const getDraft = useCallback(
    (session: string) => current.current[session] || "",
    [],
  );
  const subscribeDraft = useCallback(
    (session: string, listener: () => void) => {
      let listeners = draftListeners.current.get(session);
      if (!listeners)
        draftListeners.current.set(session, (listeners = new Set()));
      listeners.add(listener);
      return () => {
        listeners?.delete(listener);
        if (!listeners?.size) draftListeners.current.delete(session);
      };
    },
    [],
  );
  const publishDraftChanges = (previous: Drafts, next: Drafts) => {
    for (const [session, listeners] of draftListeners.current)
      if (previous[session] !== next[session])
        listeners.forEach((listener) => listener());
  };
  const [conflicts, setConflicts] = useState<DraftVersion[]>([]);
  const conflictValues = useRef<DraftVersion[]>([]);
  const initialLocalError = recovery.error || localRecovery.error;
  const [localError, setLocalError] = useState(initialLocalError);
  const [syncFailed, setSyncFailed] = useState(false);
  const localErrorValue = useRef(initialLocalError);
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
  const reconcile = useCallback(
    (records: DraftVersion[]) => {
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
        const active = activeDraftVersions(branches);
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
              alternatives.push({
                ...branch,
                id: `${branch.id}:conflict`,
                text,
              });
        }
      }
      if (next !== current.current) {
        const previous = current.current;
        current.current = next;
        update(next);
        publishDraftChanges(previous, next);
        for (const [session, text] of Object.entries(next)) {
          if (previous[session] === text) continue;
          try {
            writeLocalDraftRecord(
              storageKey.current,
              session,
              text === "" ? null : text,
              "remote",
              { updated: Date.now() },
            );
          } catch {
            reportLocalError(
              "Draft changes could not be saved on this device. Keep this chat open.",
            );
          }
        }
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
    },
    [reportLocalError],
  );
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
  const queueLegacyUpdates = useCallback(
    (imported: Record<string, string | null>, scope = storageKey.current) => {
      for (const [session, text] of Object.entries(imported)) {
        try {
          const id = `${device}:${session}`;
          const key = `${journalPrefix(scope)}legacy:${encodeURIComponent(session)}`;
          const existing = readLocalDraftRecord(scope, session);
          if (!existing) {
            reportLocalError(
              "A saved draft could not be imported from another tab. Keep this chat open.",
            );
            continue;
          }
          const prior = journal.current.get(key)?.version;
          const value = text ?? "";
          const localVersions = [
            ...versions.current,
            ...entries().map((entry) => entry.version),
          ].filter((item) => item.session === session && item.id !== id);
          const lastLegacy = Math.max(
            existing.legacyUpdated || 0,
            prior?.updated || 0,
          );
          const baselineHash =
            existing.legacyBaselineHash ??
            (existing.legacyBaseline !== undefined
              ? legacyBaselineHash(session, existing.legacyBaseline)
              : undefined);
          const localIsNewer =
            localVersions.some((item) => item.updated > lastLegacy) ||
            (existing.source !== "legacy" &&
              baselineHash !== undefined &&
              (existing.deleted
                ? baselineHash !== legacyBaselineHash(session, null)
                : legacyBaselineHash(session, existing.text) !== baselineHash));
          const localUpdated = Math.max(
            existing.source === "local" ? existing.updated || 0 : 0,
            ...localVersions.map((item) => item.updated),
          );
          let version: DraftVersion;
          if (prior?.text === value) {
            version = prior;
          } else {
            let updated = Math.max(Date.now(), lastLegacy + 1);
            if (localIsNewer && localUpdated > lastLegacy + 1)
              updated = Math.min(updated, localUpdated - 1);
            if (updated <= lastLegacy) updated = lastLegacy + 1;
            version = {
              id,
              session,
              device,
              text: value,
              updated,
              ...(prior?.text && prior.text !== value
                ? {
                    alternatives: [
                      ...new Set([...(prior.alternatives || []), prior.text]),
                    ],
                  }
                : {}),
            };
          }
          const entry = { key, version };
          if (!localIsNewer) {
            const previous = current.current;
            const next = { ...previous };
            if (text === null) delete next[session];
            else next[session] = text;
            current.current = next;
            update(next);
            publishDraftChanges(previous, next);
          }
          // The journal is the retry boundary: persist it before advancing the
          // legacy checkpoint, so failed storage writes remain discoverable.
          writeDraftJournal(entry);
          journal.current.set(key, entry);
          pendingEdits.current.set(session, version.updated);
          if (!localIsNewer) {
            writeLocalDraftRecord(scope, session, text, "local", {
              legacyBaselineHash: legacyBaselineHash(session, text),
              legacyPending: false,
              legacyUpdated: version.updated,
              updated: version.updated,
            });
          } else {
            writeLocalDraftRecord(
              scope,
              session,
              existing.deleted ? null : existing.text,
              existing.source,
              {
                legacyBaselineHash: legacyBaselineHash(session, text),
                legacyPending: false,
                legacyUpdated: version.updated,
                updated: existing.updated,
              },
            );
          }
        } catch {
          reportLocalError(
            "A saved draft could not be imported from another tab. Keep this chat open.",
          );
        }
      }
    },
    [reportLocalError],
  );
  const adoptScope = useCallback(
    (workspaceId: string) => {
      const targetKey = `codex-drafts:${workspaceId}`;
      if (storageKey.current === targetKey) return;
      const previousKey = storageKey.current;
      const unassigned = previousKey.endsWith(":unassigned");
      if (unassigned) {
        try {
          copyLocalDraftScope(previousKey, targetKey, () =>
            reportLocalError(
              "Some saved drafts could not be moved to this workspace. Keep this chat open.",
            ),
          );
        } catch (error) {
          reportLocalError(
            "Draft changes could not be moved to this workspace. Keep this chat open.",
          );
          throw error;
        }
      }
      try {
        queueLegacyUpdates(
          migrateLegacyDrafts(targetKey, () =>
            reportLocalError(
              "Some saved drafts could not be read. Keep this chat open.",
            ),
          ),
          targetKey,
        );
      } catch {
        reportLocalError(
          "Saved drafts could not be migrated. Keep this chat open.",
        );
      }
      let next: Drafts = readDraftsWithLegacyFallback(targetKey, () =>
        reportLocalError(
          "Some saved drafts could not be read. Keep this chat open.",
        ),
      );
      if (unassigned) next = { ...current.current, ...next };
      if (unassigned) {
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
      const previous = current.current;
      storageKey.current = targetKey;
      for (const entry of readDraftJournal(targetKey))
        journal.current.set(entry.key, entry);
      markPending();
      for (const entry of entries()) {
        if (
          entry.version.id === `${device}:${entry.version.session}` &&
          readLocalDraftRecord(targetKey, entry.version.session)
        )
          continue;
        next[entry.version.session] = entry.version.text;
      }
      dismissed.current = saved(`${targetKey}:dismissed`, []);
      current.current = next;
      update(next);
      publishDraftChanges(previous, next);
    },
    [reportLocalError, queueLegacyUpdates],
  );
  const flushDrafts = useCallback(() => {
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
      } catch {
        if (connecting) reportSyncFailure(true);
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
      publishDraftChanges(previous, next);
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
        const id = draftVersionId(device, writer, session);
        const version: DraftVersion = {
          id,
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
              [id, localHeads.current.get(session) || 0] as const,
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
      for (const session of new Set([
        ...Object.keys(previous),
        ...Object.keys(next),
      ])) {
        if (previous[session] === next[session]) continue;
        try {
          writeLocalDraftRecord(
            storageKey.current,
            session,
            next[session] === undefined ? null : next[session],
            "local",
            { updated },
          );
        } catch {
          reportLocalError(
            "Draft changes are not saved yet. Keep this chat open.",
          );
        }
      }
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
      if (stopped || starting || started) return;
      clearTimeout(retry);
      starting = true;
      void syncDatabase()
        .then(async ({ db, workspaceId }) => {
          if (stopped) return;
          adoptScope(workspaceId);
          await flushDrafts();
          cancel = await startDraftReplication((e) => {
            if (!stopped) reportSyncFailure(e !== null);
          });
          if (stopped) {
            cancel();
            return;
          }
          // Adopt drafts made before this device learns its workspace identity.
          for (const [session, text] of Object.entries(
            importUnassigned.current ? current.current : {},
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
          importUnassigned.current = false;
        })
        .catch(() => {
          if (!stopped) {
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
    queueLegacyUpdates(localRecovery.imported);
    start();
    const scanLegacyStorage = () => {
      try {
        const imported = migrateLegacyDrafts(
          storageKey.current,
          () =>
            reportLocalError(
              "Some saved drafts could not be read. Keep this chat open.",
            ),
          { includeNewLegacyChats: true },
        );
        queueLegacyUpdates(imported);
        void flushDrafts();
      } catch {
        reportLocalError(
          "A saved draft could not be imported from another tab. Keep this chat open.",
        );
      }
    };
    const stopResume = onResume(() => {
      start();
      scanLegacyStorage();
      void flushDrafts();
    });
    const onStorage = (event: StorageEvent) => {
      if (event.key !== storageKey.current) return;
      scanLegacyStorage();
    };
    window.addEventListener("storage", onStorage);
    const writeTimer = setInterval(() => {
      scanLegacyStorage();
      void flushDrafts();
    }, 3000);
    return () => {
      stopped = true;
      clearTimeout(retry);
      clearInterval(writeTimer);
      stopResume();
      window.removeEventListener("storage", onStorage);
      unsubscribe();
      cancel();
    };
  }, [
    reconcile,
    adoptScope,
    flushDrafts,
    decodeDrafts,
    reportSyncFailure,
    reportLocalError,
    setDrafts,
    queueLegacyUpdates,
  ]);
  return {
    get drafts() {
      return current.current;
    },
    setDrafts,
    getDraft,
    subscribeDraft,
    conflicts,
    dismissDraft,
    error: localError || syncNotice,
  };
}
