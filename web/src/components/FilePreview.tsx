import { Button, Loader } from "@mantine/core";
import { lazy, Suspense, useEffect, useMemo, useRef, useState } from "react";
import { api } from "../api";
import {
  canUseNativeFiles,
  openFile,
  previewFile,
  revealFile,
  saveFile,
} from "../fileActions";
import ErrorDescription from "./ErrorDescription";
import { isolatedDocument } from "./RichPreview";
import { fileFormat, parseDelimited, safeSvg } from "./filePreviewFormats";
import ImageViewer from "./ImageViewer";
import PreviewModal from "./PreviewModal";
import "./file-preview.css";

// StreamingText opens FilePreview for links. Load it only for Markdown files.
const Markdown = lazy(() => import("./StreamingText"));
export interface PreviewTarget {
  agent?: string;
  path?: string;
  asset?: string;
  line?: number;
}
type LoadedFile = {
  key: string;
  name: string;
  mime: string;
  url: string;
  text: string;
  bytes: Uint8Array;
  format: ReturnType<typeof fileFormat>;
};
export default function FilePreview({
  target,
  onClose,
}: {
  target: PreviewTarget | null;
  onClose: () => void;
}) {
  const key = JSON.stringify([target?.agent, target?.path, target?.asset]);
  const [loaded, setLoaded] = useState<LoadedFile | null>(null);
  const file = loaded?.key === key ? loaded : null;
  const [failure, setFailure] = useState<{
    key: string;
    error: unknown;
  } | null>(null);
  const [source, setSource] = useState(false);
  const selectedLine = useRef<HTMLElement>(null);
  const error = failure?.key === key ? failure.error : null;
  useEffect(() => {
    setLoaded(null);
    setFailure(null);
    setSource(!!target?.line);
    if (!target) return;
    let active = true,
      url = "";
    const query = new URLSearchParams(
      Object.entries(target).filter(
        ([name, value]) => name !== "line" && value != null,
      ) as [string, string][],
    );
    api(`/api/file?${query}`)
      .then((result) => {
        if (!active) return;
        const bytes = Uint8Array.from(atob(result.base64), (c) =>
          c.charCodeAt(0),
        );
        const format = fileFormat(result.name, result.mime);
        const text = [
          "markdown",
          "csv",
          "tsv",
          "json",
          "text",
          "html",
          "svg",
        ].includes(format)
          ? new TextDecoder().decode(bytes)
          : "";
        const body = format === "svg" ? safeSvg(text) : bytes;
        url = URL.createObjectURL(
          new Blob([body], {
            type: format === "svg" ? "image/svg+xml" : result.mime,
          }),
        );
        setLoaded({
          key,
          name: result.name,
          mime: result.mime,
          bytes,
          format,
          url,
          text,
        });
      })
      .catch((error) => {
        if (active) setFailure({ key, error });
      });
    return () => {
      active = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, [key]);
  useEffect(() => {
    if (target?.line) setSource(true);
  }, [target?.line]);
  useEffect(() => {
    if (!file || !target?.line || !source) return;
    const frame = requestAnimationFrame(() =>
      selectedLine.current?.scrollIntoView({ block: "center" }),
    );
    return () => cancelAnimationFrame(frame);
  }, [file, target?.line, source]);
  const excerpt = useMemo(() => {
    if (!file || !target?.line) return null;
    let start = 0;
    for (let line = 1; line < target.line; line++) {
      const end = file.text.indexOf("\n", start);
      if (end < 0) return null;
      start = end + 1;
    }
    const end = file.text.indexOf("\n", start),
      stop = end < 0 ? file.text.length : end;
    return {
      before: file.text.slice(0, start),
      line: file.text.slice(start, stop),
      after: file.text.slice(stop),
    };
  }, [file, target?.line]);
  const table = useMemo(
    () =>
      file && ["csv", "tsv"].includes(file.format)
        ? parseDelimited(file.text, file.format === "tsv" ? "\t" : ",")
        : null,
    [file],
  );
  const json = useMemo(() => {
    if (file?.format !== "json") return null;
    try {
      return {
        text: JSON.stringify(JSON.parse(file.text), null, 2),
        error: "",
      };
    } catch {
      return {
        text: file.text,
        error: "Invalid JSON. The original source is shown.",
      };
    }
  }, [file]);
  const act = (operation: () => Promise<unknown>) => {
    operation().catch((error) => setFailure({ key, error }));
  };
  const toggle =
    file && ["markdown", "csv", "tsv", "json"].includes(file.format);
  return (
    <PreviewModal
      opened={!!target}
      onClose={onClose}
      title={file?.name || target?.path || "File preview"}
      size="xl"
      className="workspace-file-modal"
    >
      <div className="file-preview-toolbar">
        {file && (
          <>
            <span>
              {file.mime}
              {target?.line ? ` · Line ${target.line}` : ""}
            </span>
            <Button
              size="xs"
              variant="light"
              onClick={() =>
                act(() =>
                  saveFile({
                    name: file.name,
                    mime: file.mime,
                    data: file.bytes,
                  }),
                )
              }
            >
              Save a copy
            </Button>
          </>
        )}
        {target && canUseNativeFiles() && (
          <>
            <Button
              size="xs"
              variant="light"
              onClick={() => act(() => openFile(target))}
            >
              Open
            </Button>
            <Button
              size="xs"
              variant="light"
              onClick={() => act(() => revealFile(target))}
            >
              Show in folder
            </Button>
            <Button
              size="xs"
              variant="light"
              onClick={() => act(() => previewFile(target))}
            >
              Quick Look
            </Button>
          </>
        )}
      </div>
      {error != null && (
        <p>
          <ErrorDescription value={error} />
        </p>
      )}
      {!file && error == null && target && (
        <Loader size="sm" aria-label="Load file" />
      )}
      {toggle && (
        <div className="file-preview-toolbar" aria-label="File view">
          <button
            type="button"
            aria-pressed={!source}
            onClick={() => setSource(false)}
          >
            Preview
          </button>
          <button
            type="button"
            aria-pressed={source}
            onClick={() => setSource(true)}
          >
            Source
          </button>
        </div>
      )}
      {file &&
        (source || file.format === "text" ? (
          <pre className="file-preview-body file-preview-source">
            {excerpt ? (
              <>
                {excerpt.before}
                <mark ref={selectedLine} data-file-line={target?.line}>
                  {excerpt.line || " "}
                </mark>
                {excerpt.after}
              </>
            ) : (
              file.text
            )}
          </pre>
        ) : file.format === "image" || file.format === "svg" ? (
          <ImageViewer
            src={file.url}
            alt={file.name}
            onError={() =>
              setFailure({ key, error: "Cannot load this image." })
            }
          />
        ) : file.format === "markdown" ? (
          <div className="file-preview-body">
            <Suspense fallback={<Loader size="sm" />}>
              <Markdown
                text={file.text}
                agentId={target?.agent}
                basePath={target?.asset ? undefined : target?.path}
              />
            </Suspense>
          </div>
        ) : table ? (
          <>
            {table.error && <p role="alert">{table.error}</p>}
            {table.truncated && (
              <p role="status">
                Table preview is limited to 200 rows and 100 columns. Large
                tables are truncated. Source and Save a copy retain the complete
                file.
              </p>
            )}
            <div className="file-preview-table">
              <table aria-label="File table">
                <tbody>
                  {table.rows.map((row, i) => (
                    <tr key={i}>
                      <th scope="row">{i + 1}</th>
                      {row.map((cell, j) => (
                        <td key={j}>{cell}</td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </>
        ) : json ? (
          <>
            {json.error && <p role="alert">{json.error}</p>}
            <pre className="file-preview-body file-preview-source">
              {json.text}
            </pre>
          </>
        ) : file.format === "html" ? (
          <iframe
            title={file.name}
            className="workspace-preview-frame"
            sandbox=""
            referrerPolicy="no-referrer"
            srcDoc={isolatedDocument(file.text)}
          />
        ) : file.format === "pdf" ? (
          <iframe
            title={file.name}
            className="workspace-preview-frame"
            src={file.url}
          />
        ) : file.format === "audio" ? (
          <audio
            onError={() =>
              setFailure({ key, error: "Cannot play this audio format." })
            }
            className="file-preview-media"
            controls
            preload="metadata"
            src={file.url}
          />
        ) : file.format === "video" ? (
          <video
            onError={() =>
              setFailure({ key, error: "Cannot play this video format." })
            }
            className="file-preview-media"
            controls
            playsInline
            preload="metadata"
            src={file.url}
          />
        ) : (
          <p>
            Preview is unavailable for this file type. Use Quick Look, Open or
            Save a copy.
          </p>
        ))}
    </PreviewModal>
  );
}
