import { useRef, useState } from "react";
import { api, ApiError, errorText } from "../api";
import type { Json } from "../types";

// Project metadata uses exact values and revision checks for a safe retry.
export function useProjectSave(
  path: string,
  saved: () => Promise<void>,
  validate?: (result: Json) => void,
) {
  const [pending, setPending] = useState(false);
  const [frozen, setFrozen] = useState(false);
  const [retryLabel, setRetryLabel] = useState<string | null>(null);
  const [error, setError] = useState("");
  const lock = useRef(false);
  const request = useRef<Json | null>(null);
  const acknowledged = useRef(false);
  const submit = async (body: Json) => {
    if (lock.current) return;
    lock.current = true;
    request.current ||= body;
    setPending(true);
    setFrozen(true);
    setError("");
    try {
      if (!acknowledged.current) {
        const result = await api(path, request.current, { timeoutMs: 15000 });
        validate?.(result);
        acknowledged.current = true;
      }
      await saved();
    } catch (failure) {
      if (
        !acknowledged.current &&
        failure instanceof ApiError &&
        failure.status >= 400 &&
        failure.status < 500 &&
        failure.status !== 408
      ) {
        request.current = null;
        setFrozen(false);
        setRetryLabel(null);
      } else {
        setRetryLabel(
          acknowledged.current ? "Refresh project" : "Retry saved request",
        );
      }
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setPending(false);
    }
  };
  return {
    submit,
    pending,
    frozen,
    error,
    retryLabel,
  };
}
