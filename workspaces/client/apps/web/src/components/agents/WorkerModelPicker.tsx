import { useServerServices } from "../../servers/ServerServicesContext";
import {
  useCallback,
  createContext,
  useContext,
  useMemo,
  useSyncExternalStore,
  type ContextType,
} from "react";
import { ApiError, get, errorText, type GetResult } from "../../api";
import { serverResourceScope } from "../../servers/registry";
import type { Snapshot } from "../../types";
import { subscribeStateProjection } from "../../sync/client";

export type WorkerModelInfo = {
  model: string;
  provider?: string;
  resolvedModel?: string;
  displayName?: string;
  description?: string;
  hidden?: boolean;
  isDefault?: boolean;
  defaultReasoningEffort?: string;
  availableAccessPrograms?: unknown;
  serviceTiers: { id: string; description?: string }[];
  supportedReasoningEfforts: { reasoningEffort: string }[];
};

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const modelInfo = (value: unknown): WorkerModelInfo | null => {
  if (!isRecord(value) || typeof value.model !== "string") return null;
  const serviceTiers = Array.isArray(value.serviceTiers)
    ? value.serviceTiers.flatMap((tier) =>
        isRecord(tier) && typeof tier.id === "string"
          ? [
              {
                id: tier.id,
                ...(typeof tier.description === "string"
                  ? { description: tier.description }
                  : {}),
              },
            ]
          : [],
      )
    : [];
  const supportedReasoningEfforts = Array.isArray(
    value.supportedReasoningEfforts,
  )
    ? value.supportedReasoningEfforts.flatMap((effort) =>
        isRecord(effort) && typeof effort.reasoningEffort === "string"
          ? [{ reasoningEffort: effort.reasoningEffort }]
          : [],
      )
    : [];
  return {
    model: value.model,
    ...(typeof value.provider === "string" ? { provider: value.provider } : {}),
    ...(typeof value.resolvedModel === "string"
      ? { resolvedModel: value.resolvedModel }
      : {}),
    ...(typeof value.displayName === "string"
      ? { displayName: value.displayName }
      : {}),
    ...(typeof value.description === "string"
      ? { description: value.description }
      : {}),
    ...(typeof value.hidden === "boolean" ? { hidden: value.hidden } : {}),
    ...(typeof value.isDefault === "boolean"
      ? { isDefault: value.isDefault }
      : {}),
    ...(typeof value.defaultReasoningEffort === "string"
      ? { defaultReasoningEffort: value.defaultReasoningEffort }
      : {}),
    ...(Object.hasOwn(value, "availableAccessPrograms")
      ? { availableAccessPrograms: value.availableAccessPrograms }
      : {}),
    serviceTiers,
    supportedReasoningEfforts,
  };
};

export const isDaybreakAlias = (model: string) =>
  /^gpt-daybreak-(blue|red)-latest$/.test(model);

const cyberAccessPrograms = (
  info?: WorkerModelInfo,
): string[] | null | undefined => {
  if (!info || !Object.hasOwn(info, "availableAccessPrograms"))
    return undefined;
  const access = info.availableAccessPrograms;
  if (!isRecord(access)) return null;
  if (!("cyber" in access)) return undefined;
  const cyber = access.cyber;
  if (!Array.isArray(cyber)) return null;
  if (!cyber.every((program): program is string => typeof program === "string"))
    return null;
  return cyber;
};

export function daybreakProgram(info?: WorkerModelInfo): string | null {
  const programs = cyberAccessPrograms(info);
  if (!programs) return null;
  if (programs.includes("daybreakBlue")) return "daybreakBlue";
  if (programs.includes("daybreakRed")) return "daybreakRed";
  return null;
}

export function supportsDaybreakMode(
  info: WorkerModelInfo | undefined,
  enabled: boolean,
) {
  if (!info || isDaybreakAlias(info.model)) return false;
  const programs = cyberAccessPrograms(info);
  if (enabled) return !!daybreakProgram(info);
  return (
    programs === undefined ||
    (programs !== null && programs.includes("standard"))
  );
}

type ModelCatalogSource = {
  scope: string;
  cacheOwner?: object;
  read: (options: {
    query?: { account_key?: string; workers?: "1"; retry?: "1" };
    signal?: AbortSignal;
  }) => Promise<GetResult<"/api/models">>;
  subscribeState: (
    accept: (snapshot: ModelCatalogProjection) => void,
    fail: (error: unknown | null) => void,
  ) => () => void;
};
type ModelCatalogProjection = Pick<Snapshot, "entitiesComplete"> & {
  runtime: Pick<Snapshot["runtime"], "modelsByAccount">;
};
const defaultModelCatalogReader: ModelCatalogSource = {
  scope: serverResourceScope(),
  read: (options) => get("/api/models", options),
  subscribeState: (accept, fail) =>
    subscribeStateProjection((snapshot) => {
      if (snapshot) accept(snapshot);
    }, fail),
};
export const ModelCatalogReader = createContext(defaultModelCatalogReader);

export function useModelCatalogReader() {
  const configured = useContext(ModelCatalogReader);
  const services = useServerServices();
  const owned = useMemo<ModelCatalogSource | undefined>(
    () =>
      services
        ? {
            cacheOwner: services,
            scope: JSON.stringify([
              services.owner.id,
              services.owner.origin,
              services.owner.credentialId,
            ]),
            read: (options) => services.api.get("/api/models", options),
            subscribeState: (accept, fail) =>
              services.sync.subscribeStateProjection((snapshot) => {
                if (snapshot) accept(snapshot);
              }, fail),
          }
        : undefined,
    [services],
  );
  return configured === defaultModelCatalogReader
    ? (owned ?? configured)
    : configured;
}

type ModelCatalogRead = NonNullable<
  ContextType<typeof ModelCatalogReader>
>["read"];

type ModelCatalogSnapshot = {
  models: WorkerModelInfo[];
  error: string;
  pending: boolean;
  loaded: boolean;
};
type ModelCatalogEntry = {
  snapshot: ModelCatalogSnapshot;
  inFlight?: Promise<void>;
  listeners: Set<() => void>;
  stopWatching?: () => void;
};

const emptyModelCatalog: ModelCatalogSnapshot = {
  models: [],
  error: "",
  pending: false,
  loaded: false,
};
const catalogsByOwner = new WeakMap<object, Map<string, ModelCatalogEntry>>();
function ownerCatalogs(owner: object) {
  let catalogs = catalogsByOwner.get(owner);
  if (!catalogs) {
    catalogs = new Map();
    catalogsByOwner.set(owner, catalogs);
  }
  return catalogs;
}

function catalogEntry(
  modelCatalogs: Map<string, ModelCatalogEntry>,
  key: string,
): ModelCatalogEntry {
  let entry = modelCatalogs.get(key);
  if (!entry) {
    entry = {
      snapshot: emptyModelCatalog,
      listeners: new Set(),
    };
    modelCatalogs.set(key, entry);
  }
  return entry;
}

function publishCatalog(
  entry: ModelCatalogEntry,
  snapshot: ModelCatalogSnapshot,
) {
  if (
    entry.snapshot.models === snapshot.models &&
    entry.snapshot.error === snapshot.error &&
    entry.snapshot.pending === snapshot.pending &&
    entry.snapshot.loaded === snapshot.loaded
  )
    return;
  entry.snapshot = snapshot;
  for (const listener of entry.listeners) listener();
}

function modelRows(data: GetResult<"/api/models">): WorkerModelInfo[] {
  return Array.isArray(data.data)
    ? data.data.flatMap((row) => {
        const parsed = modelInfo(row);
        return parsed ? [parsed] : [];
      })
    : [];
}

function readModelCatalog(
  entry: ModelCatalogEntry,
  read: ModelCatalogRead,
  accountKey: string,
  workers: boolean,
  retry = false,
): Promise<void> {
  if (entry.inFlight) return entry.inFlight;
  if (!retry && entry.snapshot.loaded) return Promise.resolve();

  const previous = entry.snapshot;
  const request = (async () => {
    try {
      const data = await read({
        query: {
          account_key: accountKey,
          workers: workers ? "1" : undefined,
          retry: retry ? "1" : undefined,
        },
      });
      const pending =
        data.catalogPending === true &&
        Array.isArray(data.unavailableAccounts) &&
        data.unavailableAccounts.some(
          (item) =>
            item?.catalogPending === true &&
            typeof item.accountKey === "string" &&
            typeof item.error === "string",
        );
      const models = modelRows(data);
      publishCatalog(entry, {
        models:
          pending && previous.models.length
            ? Array.from(
                new Map(
                  [...previous.models, ...models].map((row) => [
                    row.model,
                    row,
                  ]),
                ).values(),
              )
            : models,
        error: "",
        pending,
        loaded: true,
      });
    } catch (error) {
      const pending =
        error instanceof ApiError &&
        error.status === 400 &&
        error.catalogPending === true;
      publishCatalog(entry, {
        models: previous.models,
        error: pending ? "" : errorText(error),
        pending,
        loaded: pending,
      });
      if (!pending) throw error;
    }
  })();
  entry.inFlight = request;
  void request
    .finally(() => {
      if (entry.inFlight === request) entry.inFlight = undefined;
    })
    .catch(() => {});
  return request;
}

function projectedModelRows(
  snapshot: ModelCatalogProjection,
  accountKey: string,
  workers: boolean,
): NonNullable<Snapshot["runtime"]["modelsByAccount"]>[string] {
  const all = Object.values(snapshot.runtime.modelsByAccount ?? {}).flat();
  let rows = workers
    ? all
        .filter((row) => !row.hidden)
        .sort((left, right) => {
          const rank = (key: string) =>
            key === accountKey ? 0 : key === "default" ? 1 : 2;
          return (
            rank(left.accountKey) - rank(right.accountKey) ||
            left.catalogPosition - right.catalogPosition ||
            left.accountKey.localeCompare(right.accountKey)
          );
        })
    : (snapshot.runtime.modelsByAccount?.[accountKey] ?? []);
  if (workers) {
    const seen = new Set<string>();
    rows = rows.filter((row) => {
      if (seen.has(row.model)) return false;
      seen.add(row.model);
      return true;
    });
  }
  return rows;
}

function subscribeModelCatalog(
  modelCatalogs: Map<string, ModelCatalogEntry>,
  key: string,
  listener: () => void,
  accountKey: string,
  workers: boolean,
  subscribeState: ModelCatalogSource["subscribeState"],
) {
  const entry = catalogEntry(modelCatalogs, key);
  entry.listeners.add(listener);
  if (!entry.stopWatching) {
    const stop = subscribeState(
      (snapshot) => {
        const rows = projectedModelRows(snapshot, accountKey, workers);
        const models = rows.flatMap((row) => {
          const parsed = modelInfo(row);
          return parsed ? [parsed] : [];
        });
        const loaded = snapshot.entitiesComplete === true;
        publishCatalog(entry, { models, error: "", pending: !loaded, loaded });
      },
      (error) =>
        publishCatalog(entry, {
          ...entry.snapshot,
          error: error === null ? "" : errorText(error),
        }),
    );
    entry.stopWatching = stop;
  }
  return () => {
    entry.listeners.delete(listener);
    if (!entry.listeners.size && entry.stopWatching) {
      entry.stopWatching();
      entry.stopWatching = undefined;
    }
  };
}

/** One read of an account's subagent models, parsed like useWorkerModels. */
export async function readWorkerModels(
  reader: ContextType<typeof ModelCatalogReader>,
  accountKey: string,
  workers = false,
): Promise<WorkerModelInfo[]> {
  return new Promise((resolve, reject) => {
    let stop: (() => void) | undefined;
    let settled = false;
    const unsubscribe = reader.subscribeState(
      (snapshot) => {
        if (settled || !snapshot.entitiesComplete) return;
        settled = true;
        const models = projectedModelRows(
          snapshot,
          accountKey,
          workers,
        ).flatMap((row) => {
          const parsed = modelInfo(row);
          return parsed ? [parsed] : [];
        });
        resolve(models);
        stop?.();
      },
      (error) => {
        if (settled || error === null) return;
        settled = true;
        reject(error);
        stop?.();
      },
    );
    if (settled) unsubscribe();
    else stop = unsubscribe;
  });
}

export function useWorkerModels(
  accountKey: string,
  enabled: boolean,
  workers = false,
) {
  const reader = useModelCatalogReader();
  const modelCatalogs = ownerCatalogs(reader.cacheOwner ?? reader);
  const catalogKey = JSON.stringify([reader.scope, accountKey, workers]);
  const read = useMemo<ModelCatalogRead>(() => reader.read, [reader.read]);
  const subscribe = useCallback(
    (listener: () => void) =>
      enabled
        ? subscribeModelCatalog(
            modelCatalogs,
            catalogKey,
            listener,
            accountKey,
            workers,
            reader.subscribeState,
          )
        : () => {},
    [
      accountKey,
      catalogKey,
      enabled,
      read,
      workers,
      reader.subscribeState,
      modelCatalogs,
    ],
  );
  const getSnapshot = useCallback(
    () => modelCatalogs.get(catalogKey)?.snapshot || emptyModelCatalog,
    [catalogKey, modelCatalogs],
  );
  const current = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  const models = useMemo(
    () => current.models.filter((model) => model.model && !model.hidden),
    [current.models],
  );
  return {
    models,
    loading:
      !current.loaded || Boolean(current.pending && !current.models.length),
    error: current.error,
    retry: () => {
      if (!enabled) return;
      const entry = catalogEntry(modelCatalogs, catalogKey);
      void readModelCatalog(entry, read, accountKey, workers, true).catch(
        () => {},
      );
    },
  };
}
