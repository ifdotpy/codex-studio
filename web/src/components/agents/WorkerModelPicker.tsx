import { useEffect, useState } from "react";
import { ApiError, get, errorText } from "../../api";
import { claudeModelLabel } from "../../claude-model-label";

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
  return cyber.filter(
    (program): program is string => typeof program === "string",
  );
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

export function useWorkerModels(
  accountKey: string,
  enabled: boolean,
  workers = false,
) {
  const catalogKey = `${accountKey}:${workers}`;
  const [result, setResult] = useState<{
    key: string;
    models: WorkerModelInfo[];
    error: string;
    pending?: boolean;
  } | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const controller = new AbortController();
    setResult((previous) =>
      previous?.key === catalogKey ? { ...previous, error: "" } : null,
    );
    const load = async () => {
      let pending = false;
      try {
        const data = await get("/api/models", {
          query: {
            account_key: accountKey,
            workers: workers ? "1" : undefined,
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
      } finally {
        if (active && pending) timer = setTimeout(load, 1000);
      }
    };
    void load();
    return () => {
      active = false;
      clearTimeout(timer);
      controller.abort();
    };
  }, [accountKey, catalogKey, workers, enabled, attempt]);
  const current = result?.key === catalogKey ? result : null;
  return {
    models:
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
    loading: !current || Boolean(current.pending && !current.models.length),
    error: current?.error || "",
    retry: () => setAttempt((value) => value + 1),
  };
}
