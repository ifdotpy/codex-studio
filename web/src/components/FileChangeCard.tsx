import { memo, useMemo, useState } from "react";
import {
  ChevronRight,
  CircleX,
  FileDiff as FileDiffIcon,
  LoaderCircle,
} from "lucide-react";
import type { Json, Message } from "../types";
import FileDiff, { DiffCounts } from "./FileDiff";
import { changeLabel, fileChanges, unifiedDiff } from "./fileChangeModel";

export default memo(function FileChangeCard({
  item,
  payload,
  status,
  cwd,
}: {
  item: Message;
  payload: Json;
  status: string;
  cwd?: string;
}) {
  const [open, setOpen] = useState(true);
  const [sourceOpen, setSourceOpen] = useState(false);
  const files = useMemo(() => {
    const prefix = cwd ? cwd.replace(/\/$/, "") + "/" : "";
    const displayPath = (path: string) =>
      prefix && path.startsWith(prefix) ? path.slice(prefix.length) : path;
    return (
      payload.type === "fileChange"
        ? fileChanges(payload.changes)
        : unifiedDiff(payload.diff || "")
    ).map((file) => ({
      ...file,
      path: displayPath(file.path),
      movePath: file.movePath ? displayPath(file.movePath) : undefined,
    }));
  }, [payload, cwd]);
  const label = changeLabel(files, status);
  const error = payload.error;
  return (
    <details
      className="file-change-card"
      data-message={item.id}
      data-tool-status={status}
      open={open}
      onToggle={(event) => {
        if (event.target === event.currentTarget)
          setOpen(event.currentTarget.open);
      }}
    >
      <summary>
        {status === "running" ? (
          <LoaderCircle size={15} className="spin" />
        ) : ["failed", "declined", "cancelled", "interrupted"].includes(
            status,
          ) ? (
          <CircleX size={15} />
        ) : (
          <FileDiffIcon size={15} />
        )}
        <span className="file-change-title">{label}</span>
        {!item.truncated && <DiffCounts files={files} />}
        <ChevronRight size={13} className="file-change-chevron" />
      </summary>
      {open && (
        <div className="file-change-body">
          <FileDiff
            files={files}
            showCounts={!item.truncated}
            showPaths={files.length !== 1 || status !== "completed"}
          />
          {!files.length && (
            <p className="file-diff-notice">
              File changes are not available in this event.
            </p>
          )}
          {item.truncated && (
            <p className="file-diff-notice">The saved patch is clipped.</p>
          )}
          {error != null && (
            <pre className="file-change-error">
              {typeof error === "string"
                ? error
                : JSON.stringify(error, null, 2)}
            </pre>
          )}
          {payload.aggregatedOutput && (
            <pre className="file-diff-raw">{payload.aggregatedOutput}</pre>
          )}
          <details
            className="file-change-source"
            open={sourceOpen}
            onToggle={(event) => {
              if (event.target === event.currentTarget)
                setSourceOpen(event.currentTarget.open);
            }}
          >
            <summary>Raw event</summary>
            {sourceOpen && (
              <pre className="file-diff-raw">
                {JSON.stringify(payload, null, 2)}
              </pre>
            )}
          </details>
        </div>
      )}
    </details>
  );
});
