import { serverLocalStorage as localStorage } from "../servers/storage";
import { useRef, useState } from "react";
import {
  post,
  ApiError,
  errorText,
  saved as readSaved,
  type ApiPostPath,
  type PostBody,
  type PostResult,
} from "../api";

export function persistedProjectRequest<Path extends ApiPostPath>(
  persisted: PostBody<Path> | null,
  next: PostBody<Path>,
): PostBody<Path> {
  return persisted ?? next;
}

// Project metadata uses exact values and revision checks for a safe retry.
export function useProjectSave<Path extends ApiPostPath>(
  path: Path,
  onSaved: () => Promise<void>,
  validate?: (result: PostResult<Path>) => void,
  storageKey?: string,
) {
  type StoredRequest = {
    body: PostBody<Path>;
    acknowledged: boolean;
  };
  const [stored] = useState<StoredRequest | null>(() =>
    storageKey ? readSaved<StoredRequest | null>(storageKey, null) : null,
  );
  const [pending, setPending] = useState(false);
  const [frozen, setFrozen] = useState(!!stored);
  const [retryLabel, setRetryLabel] = useState<string | null>(
    stored
      ? stored.acknowledged
        ? "Refresh project"
        : "Retry saved request"
      : null,
  );
  const [error, setError] = useState("");
  const lock = useRef(false);
  const request = useRef<PostBody<Path> | null>(stored?.body ?? null);
  const acknowledged = useRef(!!stored?.acknowledged);
  const persist = () => {
    if (storageKey)
      localStorage.setItem(
        storageKey,
        JSON.stringify({
          body: request.current,
          acknowledged: acknowledged.current,
        }),
      );
  };
  const submit = async (body: PostBody<Path>) => {
    if (lock.current) return;
    lock.current = true;
    const requestBody = persistedProjectRequest(request.current, body);
    request.current = requestBody;
    setPending(true);
    setFrozen(true);
    setError("");
    try {
      persist();
      if (!acknowledged.current) {
        const result = await post(path, requestBody, { timeoutMs: 15000 });
        validate?.(result);
        acknowledged.current = true;
        persist();
      }
      await onSaved();
      if (storageKey) localStorage.removeItem(storageKey);
    } catch (failure) {
      if (
        !acknowledged.current &&
        failure instanceof ApiError &&
        failure.status >= 400 &&
        failure.status < 500 &&
        failure.status !== 408
      ) {
        request.current = null;
        if (storageKey) localStorage.removeItem(storageKey);
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
