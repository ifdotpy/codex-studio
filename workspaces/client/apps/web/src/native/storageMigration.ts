import {
  MONITOR_INPUT_DATABASE_NAME,
  validMonitorInput,
} from "../servers/monitorInputStore";
import { scopedStorage, serverStoragePrefix } from "../servers/storage";
import { databaseSuffixForServer } from "../servers/environment";
import { readLegacyDrafts } from "./legacyDrafts";
import { DraftJournal } from "./draftJournal";
import type { NativeJournalContext } from "./mutationJournal";
import {
  PREFERENCE_JOURNAL,
  readPendingLegacyPreferences,
} from "./preferenceJournal";
export { readPendingLegacyPreferences } from "./preferenceJournal";

const PREFERENCE_READY = "studio-native-preferences-ready-v1";
// This bounds a one-time browser snapshot, not the native store's capacity.
const MAX_LEGACY_ROWS = 10_000;
type Context = NativeJournalContext & {
  readonly serverId: string;
  readonly workspaceId: string;
};
type Receipt = { manifestId: string; sourceSha256: string; imported: true };

/** Read-only access to the managed WebView source, used only after host cutover. */
export function captureLegacyNativeStorage() {
  const original = window.localStorage;
  const storage: Storage = {
    get length() {
      return original.length;
    },
    key: (index) => original.key(index),
    getItem: (key) => original.getItem(key),
    setItem() {
      throw new Error("The legacy migration source is read-only.");
    },
    removeItem() {
      throw new Error("The legacy migration source is read-only.");
    },
    clear() {
      throw new Error("The legacy migration source is read-only.");
    },
  };
  return { storage, indexedDB: window.indexedDB };
}

function snapshot(storage: Storage) {
  if (storage.length > MAX_LEGACY_ROWS)
    throw new Error("The legacy storage snapshot is too large.");
  return Object.fromEntries(
    Array.from({ length: storage.length }, (_, i) => storage.key(i)!)
      .sort()
      .map((key) => [key, storage.getItem(key)]),
  );
}
async function rows(factory: IDBFactory, name: string, store: string) {
  const databases = await factory.databases();
  const existing = databases.find((db) => db.name === name);
  if (!existing) return [];
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    let blocked = false;
    const request = factory.open(name, existing.version);
    request.onupgradeneeded = () => request.transaction?.abort();
    request.onblocked = () => {
      blocked = true;
      reject(
        new Error("Close older Studio views before importing local data."),
      );
    };
    request.onerror = () =>
      reject(request.error || new Error("The legacy database cannot be read."));
    request.onsuccess = () => {
      if (blocked) request.result.close();
      else resolve(request.result);
    };
  });
  try {
    return await new Promise<{ key: IDBValidKey; value: unknown }[]>(
      (resolve, reject) => {
        const saved: { key: IDBValidKey; value: unknown }[] = [];
        const tx = db.transaction(store, "readonly");
        let failure: unknown;
        const cursor = tx.objectStore(store).openCursor();
        cursor.onsuccess = () => {
          const row = cursor.result;
          if (!row) return;
          if (saved.length >= MAX_LEGACY_ROWS) {
            failure = new Error(
              "The legacy database contains too many records.",
            );
            tx.abort();
            return;
          }
          saved.push({ key: row.key, value: row.value });
          row.continue();
        };
        tx.oncomplete = () => resolve(saved);
        tx.onabort = tx.onerror = () =>
          reject(
            failure ||
              tx.error ||
              new Error("The legacy read was interrupted."),
          );
      },
    );
  } finally {
    db.close();
  }
}

/** Only bootstrap can call this adapter. It reads browser data without starting
 * replication or changing the source. The caller excludes all legacy writers. */
export async function migrateNativeStorage(
  context: Context,
  options: {
    storage: Storage;
    indexedDB: IDBFactory;
    assertExclusive(): Promise<void>;
    preferencesReplayed?: boolean;
    nativeDraftOwner?: string;
    sourceIdentity?: string;
    expectedByteImports?: readonly string[];
    archiveManifest?: unknown;
  },
) {
  if (
    options.sourceIdentity !== undefined &&
    !/^[a-f0-9]{64}$/.test(options.sourceIdentity)
  )
    throw new Error("The native migration source identity is invalid.");
  const manifestId = `legacy-browser-storage:v1:${options.sourceIdentity ? options.sourceIdentity + ":" : ""}${context.serverId}`;
  const existing = await context.request<Receipt | null>(
    "client_storage.status",
    { manifestId },
  );
  if (existing) return existing;
  await options.assertExclusive();
  const before = snapshot(options.storage);
  const logical = scopedStorage(
    () => options.storage,
    serverStoragePrefix(context.serverId),
  );
  const pending = readPendingLegacyPreferences(options.storage);
  const pendingPreferences = options.preferencesReplayed
    ? []
    : pending.map((write) => {
        const prefix = 'browser:"root":';
        if (write.method === "preferences.import")
          return {
            ...write,
            params: {
              version: 1,
              entries: Object.fromEntries(
                Object.entries(
                  write.params.entries as Record<string, string>,
                ).map(([key, value]) => [prefix + key, value]),
              ),
            },
          };
        return {
          ...write,
          params: { ...write.params, key: prefix + write.params.key },
        };
      });
  const preferences: Record<string, string> = {};
  if (options.storage.getItem(PREFERENCE_READY) !== "1") {
    for (let i = 0; i < logical.length; i++) {
      const key = logical.key(i)!;
      if (
        key === PREFERENCE_JOURNAL ||
        key === PREFERENCE_READY ||
        key === "studio-browser-profile-import-v1" ||
        key.startsWith("studio-native-draft-journal-v1:") ||
        (context.serverId === "local" && key.startsWith(":server:"))
      )
        continue;
      preferences[key] = logical.getItem(key)!;
    }
  }
  const suffix = databaseSuffixForServer(context.serverId);
  const versions = await readLegacyDrafts(context.workspaceId, {
    storage: logical,
    indexedDB: options.indexedDB,
    databaseSuffix: suffix,
  });
  const draftDocuments = (
    await rows(
      options.indexedDB,
      `rxdb-dexie-studio${context.workspaceId}${suffix}--0--drafts`,
      "docs",
    )
  ).map((row) => {
    const document = row.value as {
      id: string;
      payload: string;
      seq: number;
      _deleted: boolean | number;
    };
    if (
      !document ||
      typeof document.id !== "string" ||
      document.id !== row.key ||
      typeof document.payload !== "string" ||
      !Number.isSafeInteger(document.seq) ||
      document.seq < 0 ||
      ![true, false, 0, 1].includes(document._deleted)
    )
      throw new Error("Invalid saved draft document.");
    return {
      id: document.id,
      payload: document.payload,
      seq: document.seq,
      _deleted: !!document._deleted,
    };
  });
  const pendingVersions = versions.filter(
    (version) =>
      !draftDocuments.some(
        (document) =>
          document.id === version.id &&
          JSON.stringify(JSON.parse(document.payload)) ===
            JSON.stringify(version),
      ),
  );
  const pendingDrafts = (
    context.serverId === (options.nativeDraftOwner ?? "local")
      ? new DraftJournal(options.storage, context.workspaceId).load()
      : []
  ).map((edit) => ({ workspace: context.workspaceId, ...edit }));
  const monitorInputs = (
    await rows(options.indexedDB, MONITOR_INPUT_DATABASE_NAME, "inputs")
  ).flatMap((row) => {
    if (!Array.isArray(row.key) || row.key[0] !== context.serverId) return [];
    if (row.key.length !== 2 || typeof row.key[1] !== "string" || !row.key[1])
      throw new Error("Invalid saved monitor owner.");
    const entry = validMonitorInput(row.value);
    if (!entry) throw new Error("Invalid saved monitor input.");
    return [{ monitorId: row.key[1], ...entry }];
  });
  const outbox = (
    await rows(
      options.indexedDB,
      `rxdb-dexie-studio${context.workspaceId}${suffix}--0--outbox`,
      "docs",
    )
  ).flatMap((row) => {
    const document = row.value as {
      id: string;
      payload: string;
      _deleted?: boolean | number;
    };
    if (document._deleted)
      throw new Error(
        "A deleted browser outbox row needs exact native settlement before migration. Its source was preserved.",
      );
    const saved = JSON.parse(document.payload);
    if (!saved || typeof saved !== "object" || saved.body?.id !== document.id)
      throw new Error("The saved message identity does not match its row.");
    return [
      {
        ...saved,
        id: document.id,
        workspace: context.workspaceId,
        displayPending: saved.displayPending ?? true,
        attachments: saved.attachments ?? [],
      },
    ];
  });
  await options.assertExclusive();
  if (JSON.stringify(before) !== JSON.stringify(snapshot(options.storage)))
    throw new Error(
      "Legacy data changed during migration. No source data was removed.",
    );
  const serializedSource = JSON.stringify({
    version: 1,
    ...(options.sourceIdentity
      ? {
          sourceIdentity: options.sourceIdentity,
          expectedByteImports: options.expectedByteImports ?? [],
        }
      : {}),
    ...(options.archiveManifest
      ? { archiveManifest: options.archiveManifest }
      : {}),
    preferences,
    legacyPreferenceScope: "root",
    pendingPreferences,
    pendingDrafts,
    draftDocuments: draftDocuments.length
      ? [{ workspace: context.workspaceId, documents: draftDocuments }]
      : [],
    drafts: pendingVersions.length
      ? [
          {
            workspace: context.workspaceId,
            versions: pendingVersions,
            serializedParams: JSON.stringify({
              workspace: context.workspaceId,
              versions: pendingVersions,
            }),
          },
        ]
      : [],
    monitorInputs,
    outbox,
  });
  const receipt = await context.request<Receipt>("client_storage.import", {
    manifestId,
    serializedSource,
  });
  const verified = await context.request<Receipt | null>(
    "client_storage.status",
    { manifestId },
  );
  if (
    !receipt ||
    receipt.manifestId !== manifestId ||
    receipt.imported !== true ||
    !/^[a-f0-9]{64}$/.test(receipt.sourceSha256) ||
    JSON.stringify(verified) !== JSON.stringify(receipt)
  )
    throw new Error(
      "The native import receipt could not be verified. The browser source was preserved.",
    );
  return receipt;
}
