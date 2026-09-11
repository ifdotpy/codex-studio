import { useEffect, useRef, useState } from "react";
import { errorText } from "../api";
import { UnsupportedSyncError } from "../sync/client";
import { onResume } from "../sync/resume";
import {
  deliverUpload,
  pendingUploads,
  removeUpload,
  finishUpload,
  uploadCancelled,
  type PendingUpload,
} from "../sync/uploads";
import type { useAttachmentDrafts } from "./useAttachmentDrafts";

export function useUploadRecovery(
  stateDir: string,
  workspaceId: string,
  attach: ReturnType<typeof useAttachmentDrafts>[1],
) {
  const [pending, setPending] = useState<PendingUpload[]>([]);
  const [error, setError] = useState("");
  const wake = useRef(() => {});
  const currentWorkspace = useRef(workspaceId);
  currentWorkspace.current = workspaceId;
  useEffect(() => {
    let stopped = false,
      busy = false,
      again = false;
    const run = async () => {
      if (stopped || !workspaceId) return;
      if (busy) {
        again = true;
        return;
      }
      busy = true;
      try {
        const rows = await pendingUploads(stateDir, workspaceId);
        if (stopped) return;
        setPending(rows);
        if (!navigator.onLine) return;
        const failures: string[] = [];
        for (const row of rows) {
          try {
            const asset = await deliverUpload(row);
            if (stopped) return;
            if (!asset || uploadCancelled(row.id)) continue;
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
          } catch (failure) {
            failures.push(`${row.name}: ${errorText(failure)}`);
          }
        }
        if (!stopped) {
          setPending(await pendingUploads(stateDir, workspaceId));
          setError(failures.join(" "));
        }
      } catch (failure) {
        if (!stopped && !(failure instanceof UnsupportedSyncError))
          setError(errorText(failure));
      } finally {
        busy = false;
        if (again && !stopped) {
          again = false;
          void run();
        }
      }
    };
    wake.current = () => {
      void run();
    };
    const resume = () => {
      void run();
    };
    window.addEventListener("studio-uploads-changed", resume);
    const stopResume = onResume(resume);
    const timer = setInterval(resume, 5000);
    void run();
    return () => {
      stopped = true;
      clearInterval(timer);
      stopResume();
      window.removeEventListener("studio-uploads-changed", resume);
    };
  }, [stateDir, workspaceId]);
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
