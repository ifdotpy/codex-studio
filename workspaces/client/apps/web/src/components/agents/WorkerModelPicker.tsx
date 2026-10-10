import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { ApiError, get, errorText, type GetResult } from "../../api";
import { claudeModelLabel } from "../../claude-model-label";
import { watchResourceReads } from "../watchResourceReads";

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

type ModelRetryTicket = { key: string; attempt: number };
export function consumeModelRetryTicket(
  ticket: { current: ModelRetryTicket | null },
  key: string,
  attempt: number,
) {
  const pending = ticket.current;
  if (!pending) return false;
  if (pending.key !== key) {
    ticket.current = null;
    return false;
  }
  if (pending.attempt !== attempt) return false;
  ticket.current = null;
  return true;
}

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
} | null>(null);

export function useWorkerModels(
  accountKey: string,
  enabled: boolean,
  workers = false,
) {
  const reader = useContext(ModelCatalogReader);
  const catalogKey = `${reader?.scope || ""}:${accountKey}:${workers}`;
  const [result, setResult] = useState<{
    key: string;
    models: WorkerModelInfo[];
    error: string;
    pending?: boolean;
  } | null>(null);
  const [attempt, setAttempt] = useState(0);
  const retrySequence = useRef(0);
  const retryTicket = useRef<ModelRetryTicket | null>(null);
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    let explicitRetry = consumeModelRetryTicket(
      retryTicket,
      catalogKey,
      attempt,
    );
    const controller = new AbortController();
    setResult((previous) =>
      previous?.key === catalogKey ? { ...previous, error: "" } : null,
    );
    const load = async () => {
      let pending = false;
      const retry = explicitRetry;
      explicitRetry = false;
      try {
        const read =
          reader?.read ||
          ((options: {
            query?: { account_key?: string; workers?: string; retry?: "1" };
            signal?: AbortSignal;
          }) => get("/api/models", options));
        const data = await read({
          query: {
            account_key: accountKey,
            workers: workers ? "1" : undefined,
            retry: retry ? "1" : undefined,
          },
          signal: controller.signal,
        });
        if (!active) return;
        pending =
          data.catalogPending === true &&
          Array.isArray(data.unavailableAccounts) &&
          data.unavailableAccounts.some(
            (item) =>
              item?.catalogPending === true &&
              typeof item.accountKey === "string" &&
              typeof item.error === "string",
          );
        const models = Array.isArray(data.data)
          ? data.data.flatMap((row) => {
              const parsed = modelInfo(row);
              return parsed ? [parsed] : [];
            })
          : [];
        setResult((previous) => ({
          key: catalogKey,
          models:
            pending && previous?.key === catalogKey
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
        }));
      } catch (error) {
        if (!active) return;
        pending =
          error instanceof ApiError &&
          error.status === 400 &&
          isRecord(error.details) &&
          error.details.catalogPending === true;
        setResult((previous) => ({
          key: catalogKey,
          models: previous?.key === catalogKey ? previous.models : [],
          error: pending ? "" : errorText(error),
          pending,
        }));
        if (!pending) throw error;
      }
    };
    const stop = watchResourceReads({ kind: "models" }, load, (error) => {
      if (active)
        setResult((previous) => ({
          key: catalogKey,
          models: previous?.key === catalogKey ? previous.models : [],
          error: errorText(error),
        }));
    });
    // A dialog can read another server before that server has a resource stream.
    if (reader) stop.refresh();
    return () => {
      active = false;
      controller.abort();
      stop();
    };
  }, [accountKey, catalogKey, workers, enabled, attempt, reader]);
  const current = result?.key === catalogKey ? result : null;
  const models = useMemo(
    () =>
      current?.models
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
    [current?.models],
  );
  return {
    models,
    loading: !current || Boolean(current.pending && !current.models.length),
    error: current?.error || "",
    retry: () => {
      const next = ++retrySequence.current;
      retryTicket.current = { key: catalogKey, attempt: next };
      setAttempt(next);
    },
  };
}
