import { useEffect, useRef, useState } from "react";
import { errorText } from "../api";
import { onResume } from "../sync/resume";
import { retryableReadError } from "../sync/readRetry";
import {
  onUploadsChanged,
  type deliverUpload,
  type PendingUpload,
} from "../sync/uploads";
import { useServerUploads } from "../servers/ServerServicesContext";
import type { useAttachmentDrafts } from "./useAttachmentDrafts";

const UPLOAD_RETRY_BASE_DELAY_MS = 1_000;
const UPLOAD_RETRY_MAX_DELAY_MS = 30_000;
const UPLOAD_RETRY_MAX_EXPONENT = 5;

export function createUploadRecoveryRetryScheduler(
  run: (ids: Set<string> | null) => void,
) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let attempt = 0;
  const reset = () => {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
    attempt = 0;
  };
  const schedule = (ids: readonly string[] | null) => {
    if (ids && ids.length === 0) {
      reset();
      return;
    }
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
    const delay = Math.min(
      UPLOAD_RETRY_BASE_DELAY_MS *
        2 ** Math.min(attempt, UPLOAD_RETRY_MAX_EXPONENT),
      UPLOAD_RETRY_MAX_DELAY_MS,
    );
    attempt += 1;
    const selectedIds = ids ? new Set(ids) : null;
    timer = setTimeout(() => {
      timer = undefined;
      run(selectedIds);
    }, delay);
  };
  return { reset, schedule, stop: reset };
}

export function retryableUploadFailure(error: unknown) {
  // Upload retries reuse the immutable IndexedDB row ID and its stored bytes.
  return retryableReadError(error);
}

export function retryableUploadStoreFailure(error: unknown) {
  return (
    retryableUploadFailure(error) ||
    (typeof DOMException !== "undefined" &&
      error instanceof DOMException &&
      [
        "AbortError",
        "InvalidStateError",
        "TimeoutError",
        "UnknownError",
      ].includes(error.name))
  );
}

export async function attemptUploadDelivery(
  row: PendingUpload,
  deliver: (
    row: PendingUpload,
  ) => Promise<Awaited<ReturnType<typeof deliverUpload>>>,
) {
  try {
    return { ok: true as const, asset: await deliver(row) };
  } catch (error) {
    return {
      ok: false as const,
      error,
      retryable: retryableUploadStoreFailure(error),
    };
  }
}

export function subscribeUploadRecoveryWakeups(
  refresh: () => void,
  subscribe = onUploadsChanged,
) {
  const stopUploadsChanged = subscribe(refresh);
  const stopResume = onResume(refresh);
  return () => {
    stopResume();
    stopUploadsChanged();
  };
}

export function useUploadRecovery(
  stateDir: string,
  workspaceId: string,
  attach: ReturnType<typeof useAttachmentDrafts>[1],
) {
  const uploads = useServerUploads();
  const {
    deliverUpload,
    pendingUploads,
    removeUpload,
    finishUpload,
    uploadCancelled,
    onUploadsChanged,
  } = uploads;
  const [pending, setPending] = useState<PendingUpload[]>([]);
  const [error, setError] = useState("");
  const wake = useRef(() => {});
  const currentWorkspace = useRef(workspaceId);
  currentWorkspace.current = workspaceId;
  useEffect(() => {
    let stopped = false,
      busy = false,
      again = false,
      retryPermanentAgain = false;
    const failures = new Map<string, { text: string; retryable: boolean }>();
    let storageError = "";
    let scheduler: ReturnType<typeof createUploadRecoveryRetryScheduler>;
    const run = async (
      retryOnly: Set<string> | null = null,
      retryPermanent = false,
    ) => {
      if (stopped || !workspaceId) return;
      if (busy) {
        again = true;
        retryPermanentAgain ||= retryPermanent;
        return;
      }
      busy = true;
      try {
        const rows = await pendingUploads(stateDir, workspaceId);
        if (stopped) return;
        const pendingIds = new Set(rows.map((row) => row.id));
        for (const id of failures.keys())
          if (!pendingIds.has(id)) failures.delete(id);
        storageError = "";
        setPending(rows);
        if (!rows.length) {
          scheduler.schedule([]);
          setError(
            [...failures.values()].map((failure) => failure.text).join(" "),
          );
          return;
        }
        if (!navigator.onLine) {
          scheduler.reset();
          setError(
            [...failures.values()].map((failure) => failure.text).join(" "),
          );
          return;
        }
        const retryableIds = new Set<string>();
        for (const row of rows) {
          if (
            (retryOnly && !retryOnly.has(row.id)) ||
            (!retryPermanent && failures.get(row.id)?.retryable === false)
          )
            continue;
          try {
            const delivery = await attemptUploadDelivery(row, deliverUpload);
            if (!delivery.ok) {
              failures.set(row.id, {
                text: `${row.name}: ${errorText(delivery.error)}`,
                retryable: delivery.retryable,
              });
              if (delivery.retryable) retryableIds.add(row.id);
              continue;
            }
            const asset = delivery.asset;
            if (stopped) return;
            if (!asset || uploadCancelled(row.id)) {
              failures.delete(row.id);
              continue;
            }
            if (currentWorkspace.current !== row.workspace) return;
            const saved = await attach((current) => {
              if (
                stopped ||
                currentWorkspace.current !== row.workspace ||
                uploadCancelled(row.id)
              )
                return current;
              return {
                ...current,
                [row.agent]: [
                  ...(current[row.agent] || []).filter(
                    (item) => item.id !== asset.id,
                  ),
                  asset,
                ],
              };
            });
            if (
              stopped ||
              currentWorkspace.current !== row.workspace ||
              uploadCancelled(row.id)
            )
              return;
            if (!saved)
              throw new Error(
                "The uploaded file is saved. Its attachment could not be saved on this device. Keep this page open.",
              );
            await finishUpload(row.id);
            failures.delete(row.id);
          } catch (failure) {
            const retryable = retryableUploadStoreFailure(failure);
            failures.set(row.id, {
              text: `${row.name}: ${errorText(failure)}`,
              retryable,
            });
            if (retryable) retryableIds.add(row.id);
          }
        }
        if (!stopped) {
          try {
            const remaining = await pendingUploads(stateDir, workspaceId);
            const remainingIds = new Set(remaining.map((row) => row.id));
            for (const id of failures.keys())
              if (!remainingIds.has(id)) failures.delete(id);
            storageError = "";
            setPending(remaining);
            setError(
              [
                ...[...failures.values()].map((failure) => failure.text),
                storageError,
              ]
                .filter(Boolean)
                .join(" "),
            );
            scheduler.schedule([...retryableIds]);
          } catch (failure) {
            storageError = errorText(failure);
            setError(
              [
                ...[...failures.values()].map((entry) => entry.text),
                storageError,
              ]
                .filter(Boolean)
                .join(" "),
            );
            if (navigator.onLine && retryableUploadStoreFailure(failure))
              scheduler.schedule(null);
            else scheduler.reset();
          }
        }
      } catch (failure) {
        if (!stopped) {
          storageError = errorText(failure);
          setError(
            [...[...failures.values()].map((entry) => entry.text), storageError]
              .filter(Boolean)
              .join(" "),
          );
          if (navigator.onLine && retryableUploadStoreFailure(failure))
            scheduler.schedule(null);
          else scheduler.reset();
        }
      } finally {
        busy = false;
        if (again && !stopped) {
          again = false;
          const retryPermanent = retryPermanentAgain;
          retryPermanentAgain = false;
          scheduler.reset();
          void run(null, retryPermanent);
        }
      }
    };
    const refresh = () => {
      scheduler.reset();
      void run(null);
    };
    const retry = () => {
      scheduler.reset();
      void run(null, true);
    };
    scheduler = createUploadRecoveryRetryScheduler((ids) => void run(ids));
    wake.current = retry;
    const stopWakeups = subscribeUploadRecoveryWakeups(
      refresh,
      onUploadsChanged,
    );
    setPending([]);
    setError("");
    void run(null);
    return () => {
      stopped = true;
      scheduler.stop();
      stopWakeups();
    };
  }, [stateDir, workspaceId, uploads]);
  return {
    pending,
    error,
    retry: () => wake.current(),
    remove: removeUpload,
    include: (rows: PendingUpload[]) =>
      setPending((current) => [
        ...new Map([...current, ...rows].map((row) => [row.id, row])).values(),
      ]),
  };
}
