import { useEffect, useState } from "react";
import { ApiError, get, errorText, type GetResult } from "../../api";
import { claudeModelLabel } from "../../claude-model-label";

type ModelCatalog = GetResult<"/api/models">;
type ModelInfo = NonNullable<ModelCatalog["data"]>[number];

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

export const isDaybreakAlias = (model: string) =>
  /^gpt-daybreak-(blue|red)-latest$/.test(model);

const cyberAccessPrograms = (info?: ModelInfo): string[] | undefined => {
  const access = info?.availableAccessPrograms;
  if (!isRecord(access)) return undefined;
  const cyber = access.cyber;
  return Array.isArray(cyber)
    ? cyber.filter((program): program is string => typeof program === "string")
    : undefined;
};

export function daybreakProgram(info?: ModelInfo): string | null {
  const programs = cyberAccessPrograms(info);
  if (!programs) return null;
  if (programs.includes("daybreakBlue")) return "daybreakBlue";
  if (programs.includes("daybreakRed")) return "daybreakRed";
  return null;
}

export function supportsDaybreakMode(
  info: ModelInfo | undefined,
  enabled: boolean,
) {
  if (!info || isDaybreakAlias(info.model)) return false;
  const programs = cyberAccessPrograms(info);
  if (enabled) return !!daybreakProgram(info);
  return programs === undefined || programs.includes("standard");
}

export function useWorkerModels(
  accountKey: string,
  enabled: boolean,
  workers = false,
) {
  const catalogKey = `${accountKey}:${workers}`;
  const [result, setResult] = useState<{
    key: string;
    models: ModelInfo[];
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
        const models = Array.isArray(data.data) ? data.data : [];
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
        .map((model): ModelInfo => ({
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
