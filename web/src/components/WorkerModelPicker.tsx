import { useEffect, useState } from "react";
import { api, errorText } from "../api";
import { type Json } from "../types";

export function useWorkerModels(accountKey: string, enabled: boolean) {
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
    api("/api/models?account_key=" + encodeURIComponent(accountKey))
      .then((data) => {
        if (active)
          setResult({ key: accountKey, models: data.data || [], error: "" });
      })
      .catch((error) => {
        if (active)
          setResult({ key: accountKey, models: [], error: errorText(error) });
      });
    return () => {
      active = false;
    };
  }, [accountKey, enabled, attempt]);
  const current = result?.key === accountKey ? result : null;
  return {
    models:
      current?.models.filter((model) => model.model && !model.hidden) || [],
    loading: !current,
    error: current?.error || "",
    retry: () => setAttempt((value) => value + 1),
  };
}
