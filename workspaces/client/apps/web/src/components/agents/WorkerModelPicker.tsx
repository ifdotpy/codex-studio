import {
  useCallback,
  createContext,
  useContext,
  useMemo,
  useSyncExternalStore,
  type ContextType,
} from "react";
import { ApiError, get, errorText, type GetResult } from "../../api";
import { claudeModelLabel } from "../../claude-model-label";
import { watchResourceReads } from "../watchResourceReads";
import type { ResourceVersion } from "../../sync/resourceEvents";
import { serverResourceScope } from "../../servers/registry";

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

export const ModelCatalogReader = createContext<{
  scope: string;
  read: (options: {
    query?: { account_key?: string; workers?: string; retry?: "1" };
    signal?: AbortSignal;
  }) => Promise<GetResult<"/api/models">>;
}>({
  scope: serverResourceScope(),
  read: (options) => get("/api/models", options),
});

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
  version?: ResourceVersion;
  valueVersion?: ResourceVersion;
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
const modelCatalogs = new Map<string, ModelCatalogEntry>();

function catalogEntry(key: string): ModelCatalogEntry {
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

function sameResourceVersion(
  left: ResourceVersion | undefined,
  right: ResourceVersion | undefined,
) {
  return (
    !!left &&
    !!right &&
    left.epoch === right.epoch &&
    left.revision === right.revision
  );
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
  version?: ResourceVersion,
  retry = false,
): Promise<void> {
  if (version) entry.version = version;
  const readVersion = version ?? entry.version;
  const changed =
    readVersion !== undefined &&
    !sameResourceVersion(entry.valueVersion, readVersion);
  if (entry.inFlight) return entry.inFlight;
  if (!retry && !changed && entry.snapshot.loaded) return Promise.resolve();

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
      entry.valueVersion = readVersion;
    } catch (error) {
      const pending =
        error instanceof ApiError &&
        error.status === 400 &&
        isRecord(error.details) &&
        error.details.catalogPending === true;
      publishCatalog(entry, {
        models: previous.models,
        error: pending ? "" : errorText(error),
        pending,
        loaded: pending,
      });
      if (!pending) throw error;
      if (pending) entry.valueVersion = readVersion;
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

function subscribeModelCatalog(
  key: string,
  listener: () => void,
  read: ModelCatalogRead,
  accountKey: string,
  workers: boolean,
) {
  const entry = catalogEntry(key);
  entry.listeners.add(listener);
  if (!entry.stopWatching) {
    const stop = watchResourceReads(
      { kind: "models" },
      (version) => readModelCatalog(entry, read, accountKey, workers, version),
      (error) =>
        publishCatalog(entry, {
          ...entry.snapshot,
          error: errorText(error),
        }),
      { waitForBaseline: true },
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

export function useWorkerModels(
  accountKey: string,
  enabled: boolean,
  workers = false,
) {
  const reader = useContext(ModelCatalogReader);
  const catalogKey = JSON.stringify([reader.scope, accountKey, workers]);
  const read = useMemo<ModelCatalogRead>(() => reader.read, [reader.read]);
  const subscribe = useCallback(
    (listener: () => void) =>
      enabled
        ? subscribeModelCatalog(catalogKey, listener, read, accountKey, workers)
        : () => {},
    [accountKey, catalogKey, enabled, read, workers],
  );
  const getSnapshot = useCallback(
    () => modelCatalogs.get(catalogKey)?.snapshot || emptyModelCatalog,
    [catalogKey],
  );
  const current = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  const models = useMemo(
    () =>
      current.models
        .filter((model) => model.model && !model.hidden)
        .map((model): WorkerModelInfo => ({
          ...model,
          displayName:
            typeof model.displayName === "string" && model.displayName
              ? claudeModelLabel(
                  model.displayName,
                  typeof model.description === "string"
                    ? model.description
                    : "",
                )
              : model.displayName,
        })) || [],
    [current.models],
  );
  return {
    models,
    loading:
      !current.loaded || Boolean(current.pending && !current.models.length),
    error: current.error,
    retry: () => {
      if (!enabled) return;
      const entry = catalogEntry(catalogKey);
      void readModelCatalog(
        entry,
        read,
        accountKey,
        workers,
        undefined,
        true,
      ).catch(() => {});
    },
  };
}
