import { useRef, useState } from "react";
import { post, ApiError, errorText, saved as readSaved } from "../api";
import type { paths } from "../generated/api";

type PostPath = Extract<
  {
    [Path in keyof paths]: paths[Path] extends { post: unknown } ? Path : never;
  }[keyof paths],
  string
>;
type PostBody<Path extends PostPath> = paths[Path] extends {
  post: {
    requestBody: { content: { "application/json": infer Body } };
  };
}
  ? Body
  : never;
type PostResult<Path extends PostPath> = Awaited<ReturnType<typeof post<Path>>>;

// Project metadata uses exact values and revision checks for a safe retry.
export function useProjectSave<Path extends PostPath>(
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
    request.current ??= body;
    const requestBody = request.current;
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
