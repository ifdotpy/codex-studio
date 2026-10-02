import { useEffect, useState } from "react";
import { ApiError, api, errorText } from "../../api";
import { type Json } from "../../types";
import { claudeModelLabel } from "../../claude-model-label";

export const isDaybreakAlias = (model: string) =>
  /^gpt-daybreak-(blue|red)-latest$/.test(model);

export function daybreakProgram(info?: Json): string | null {
  const programs = info?.availableAccessPrograms?.cyber;
  if (!Array.isArray(programs)) return null;
  if (programs.includes("daybreakBlue")) return "daybreakBlue";
  if (programs.includes("daybreakRed")) return "daybreakRed";
  return null;
}

export function supportsDaybreakMode(info: Json | undefined, enabled: boolean) {
  if (!info || isDaybreakAlias(info.model)) return false;
  if (enabled) return !!daybreakProgram(info);
  const programs = info.availableAccessPrograms?.cyber;
  return (
    programs === undefined ||
    (Array.isArray(programs) && programs.includes("standard"))
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
    models: Json[];
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
        const data = await api(
          "/api/models?account_key=" +
            encodeURIComponent(accountKey) +
            (workers ? "&workers=1" : ""),
          undefined,
          { signal: controller.signal },
        );
        if (!active) return;
        pending =
          data.catalogPending === true &&
          Array.isArray(data.unavailableAccounts) &&
          data.unavailableAccounts.some(
            (item: Json) =>
              item?.catalogPending === true &&
              typeof item.accountKey === "string" &&
              typeof item.error === "string",
          );
        const models: Json[] = Array.isArray(data.data) ? data.data : [];
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
          error.details !== null &&
          typeof error.details === "object" &&
          !Array.isArray(error.details) &&
          (error.details as Json).catalogPending === true;
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
        .map((model): Json => ({
          ...model,
          displayName: model.displayName
            ? claudeModelLabel(model.displayName, model.description || "")
            : model.displayName,
        })) || [],
    loading: !current || Boolean(current.pending && !current.models.length),
    error: current?.error || "",
    retry: () => setAttempt((value) => value + 1),
  };
}
