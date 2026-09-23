import { useEffect, useState } from "react";
import { api, errorText } from "../api";
import { type Json } from "../types";
import { claudeModelLabel } from "../claude-model-label";

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
  } | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    setResult(null);
    api(
      "/api/models?account_key=" +
        encodeURIComponent(accountKey) +
        (workers ? "&workers=1" : ""),
    )
      .then((data) => {
        if (active)
          setResult({ key: catalogKey, models: data.data || [], error: "" });
      })
      .catch((error) => {
        if (active)
          setResult({ key: catalogKey, models: [], error: errorText(error) });
      });
    return () => {
      active = false;
    };
  }, [accountKey, catalogKey, workers, enabled, attempt]);
  const current = result?.key === catalogKey ? result : null;
  return {
    models:
      current?.models
        .filter((model) => model.model && !model.hidden)
        .map(
          (model): Json => ({
            ...model,
            displayName: model.displayName
              ? claudeModelLabel(model.displayName, model.description || "")
              : model.displayName,
          }),
        ) || [],
    loading: !current,
    error: current?.error || "",
    retry: () => setAttempt((value) => value + 1),
  };
}
