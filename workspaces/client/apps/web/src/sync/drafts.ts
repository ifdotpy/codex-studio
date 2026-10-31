import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type SetStateAction,
} from "react";
import { syncPost } from "../api.ts";
import { randomUUID } from "../uuid.ts";
import type { DraftVersion } from "../generated/rustOwned.ts";
import type { components } from "../generated/api.ts";
import { watchResourceChanges } from "./subscriptions/events.ts";
import { replaceStream } from "./resourceEvents.ts";
import { createDirectDrafts } from "./directDrafts.ts";
import { decodeDraftVersion } from "./draftDecoder.ts";
import { encodeDraftPayload } from "./draftPayload.ts";
import { draftVersionId } from "./draftIdentity.ts";
import { activeDraftVersions } from "./draftVersions.ts";
import { DRAFT_SYNC_TIMING_MS } from "./draftSyncTiming.ts";

export type { DraftVersion } from "../generated/rustOwned.ts";
type Drafts = Record<string, string>;
const incompleteDrafts = "Some saved drafts could not be loaded.";

export function createBrowserDrafts(options: {
  api: {
    syncPost: (
      path: "/api/sync/drafts",
      body: components["schemas"]["DraftPushRequest"],
    ) => Promise<components["schemas"]["SyncDocument"][]>;
  };
  resources: Pick<
    typeof import("./resourceEvents.ts"),
    "watchResourceChanges" | "replaceStream"
  >;
}) {
  const device = randomUUID();
  return function useBrowserDrafts() {
    const [writer] = useState(() => randomUUID());
    const current = useRef<Drafts>({});
    const listeners = useRef(new Map<string, Set<() => void>>());
    const versions = useRef<DraftVersion[]>([]);
    const dismissed = useRef(new Set<string>());
    const [conflicts, setConflicts] = useState<DraftVersion[]>([]);
    const [error, setError] = useState("");
    const quiet = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
    const maximum = useRef<ReturnType<typeof setTimeout> | undefined>(
      undefined,
    );
    const store = useRef<ReturnType<typeof createDirectDrafts> | undefined>(
      undefined,
    );
    const key = (version: DraftVersion) =>
      JSON.stringify([version.id, version.text]);
    const publish = (previous: Drafts, next: Drafts) => {
      for (const [session, callbacks] of listeners.current)
        if (previous[session] !== next[session])
          callbacks.forEach((callback) => callback());
    };
    const reconcile = () => {
      if (!store.current) return;
      const records = store.current
        .records()
        .map((row) => decodeDraftVersion(JSON.parse(row.payload)));
      versions.current = records;
      const groups = new Map<string, DraftVersion[]>();
      for (const record of records) {
        const group = groups.get(record.session) ?? [];
        group.push(record);
        groups.set(record.session, group);
      }
      const next: Drafts = {};
      const alternatives: DraftVersion[] = [];
      for (const [session, group] of groups) {
        const active = activeDraftVersions(group);
        const own = draftVersionId(device, writer, session);
        const chosen =
          active.find((record) => record.id === own) ??
          active.find((record) => !dismissed.current.has(key(record)));
        if (chosen && next[session] !== chosen.text) {
          next[session] = chosen.text;
        }
        for (const record of active) {
          if (record.text && record.text !== next[session])
            alternatives.push(record);
          for (const text of record.alternatives ?? [])
            if (text && text !== next[session])
              alternatives.push({
                ...record,
                id: `${record.id}:conflict`,
                text,
              });
        }
      }
      const previous = current.current;
      current.current = next;
      publish(previous, next);
      const unique = new Set<string>();
      const nextConflicts = alternatives.filter((version) => {
        const id = key(version);
        if (dismissed.current.has(id) || unique.has(id)) return false;
        unique.add(id);
        return true;
      });
      setConflicts((previous) =>
        previous.length === nextConflicts.length &&
        previous.every(
          (item, index) => key(item) === key(nextConflicts[index]!),
        )
          ? previous
          : nextConflicts,
      );
    };
    if (!store.current)
      store.current = createDirectDrafts({
        post: (rows) => options.api.syncPost("/api/sync/drafts", { rows }),
        changed: reconcile,
      });
    const flush = () => {
      clearTimeout(quiet.current);
      clearTimeout(maximum.current);
      quiet.current = maximum.current = undefined;
      void store.current
        ?.flush()
        .then(() =>
          setError((previous) =>
            previous === incompleteDrafts ? previous : "",
          ),
        )
        .catch(() =>
          setError("Draft changes could not be saved. Keep this chat open."),
        );
    };
    const schedule = () => {
      clearTimeout(quiet.current);
      quiet.current = setTimeout(flush, DRAFT_SYNC_TIMING_MS.pushQuietWait);
      maximum.current ??= setTimeout(flush, DRAFT_SYNC_TIMING_MS.pushMaxWait);
    };
    const setDrafts = useCallback(
      (value: SetStateAction<Drafts>) => {
        const previous = current.current;
        const next = typeof value === "function" ? value(previous) : value;
        current.current = next;
        publish(previous, next);
        const updated = Math.max(
          Date.now(),
          ...versions.current.map((version) => version.updated + 1),
        );
        for (const session of new Set([
          ...Object.keys(previous),
          ...Object.keys(next),
        ])) {
          if (previous[session] === next[session]) continue;
          const id = draftVersionId(device, writer, session);
          const seen = Object.fromEntries(
            versions.current
              .filter((version) => version.session === session)
              .flatMap((version) => [
                [version.id, version.updated],
                ...Object.entries(version.seen ?? {}),
              ])
              .sort((a, b) => Number(a[1]) - Number(b[1])),
          );
          const version: DraftVersion = {
            id,
            session,
            device,
            text: next[session] || "",
            updated,
            seen,
          };
          store.current?.edit({
            id,
            payload: encodeDraftPayload(version),
            seq: 0,
            _deleted: false,
          });
          versions.current = [
            ...versions.current.filter((item) => item.id !== id),
            version,
          ];
        }
        schedule();
      },
      [writer],
    );
    const getDraft = useCallback(
      (session: string) => current.current[session] || "",
      [],
    );
    const subscribeDraft = useCallback(
      (session: string, listener: () => void) => {
        const callbacks =
          listeners.current.get(session) ?? new Set<() => void>();
        callbacks.add(listener);
        listeners.current.set(session, callbacks);
        return () => {
          callbacks.delete(listener);
          if (!callbacks.size) listeners.current.delete(session);
        };
      },
      [],
    );
    const dismissDraft = useCallback(
      (version: DraftVersion) => {
        dismissed.current.add(key(version));
        reconcile();
      },
      [writer],
    );
    useEffect(() => {
      const stop = options.resources.watchResourceChanges(
        { kind: "drafts" },
        (version) => {
          if (!version || !store.current) return;
          try {
            if (version.draftSnapshot) {
              store.current.snapshot(
                version.draftSnapshot.documents,
                version.draftSnapshot.complete,
                version.revision,
              );
              setError(version.draftSnapshot.complete ? "" : incompleteDrafts);
              schedule();
            } else if (
              version.reason === "change" &&
              version.revision > store.current.sequence()
            ) {
              const batch = version.draftChanges;
              if (
                !batch ||
                store.current.sequence() < 0 ||
                batch.after > store.current.sequence() ||
                batch.through !== version.revision ||
                batch.through <= batch.after ||
                batch.documents.at(-1)?.seq !== batch.through
              ) {
                store.current.waitForBaseline();
                options.resources.replaceStream();
                return;
              }
              store.current.changes(batch.documents, batch.through);
            }
          } catch {
            setError(
              "Some saved drafts could not be read. Keep this chat open.",
            );
          }
        },
        { waitForBaseline: true },
      );
      return () => {
        stop();
        clearTimeout(quiet.current);
        clearTimeout(maximum.current);
      };
    }, [writer]);
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
      localPersistenceFailed: !!error && error !== incompleteDrafts,
    };
  };
}
export const useSyncedDrafts = createBrowserDrafts({
  api: { syncPost },
  resources: { watchResourceChanges, replaceStream },
});
