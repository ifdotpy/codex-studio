import { Button, Loader } from "@mantine/core";
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import ErrorDescription from "./ErrorDescription";
import ImageViewer from "./ImageViewer";
import PreviewModal from "./PreviewModal";
import "./file-preview.css";

export interface PreviewTarget {
  agent?: string;
  path?: string;
  asset?: string;
  line?: number;
}
export type FilePresentation = {
  name: string;
  mime: string;
  format: string;
  text: string;
  url: string;
  table: { rows: string[][]; truncated: boolean; error: string } | null;
  json: { text: string; error: string } | null;
};
export type PreviewActions = {
  canUseNativeFiles(): boolean;
  openFile(target: PreviewTarget): Promise<unknown>;
  revealFile(target: PreviewTarget): Promise<unknown>;
  previewFile(target: PreviewTarget): Promise<unknown>;
};

/** Presentation only. The platform retains file bytes and every effect owner. */
export default function FilePreviewView({
  target,
  file,
  error,
  onClose,
  onError,
  actions,
  save,
  markdown,
  html,
  pdf,
  media,
}: {
  target: PreviewTarget | null;
  file: FilePresentation | null;
  error: unknown;
  onClose(): void;
  onError(error: unknown): void;
  actions: PreviewActions;
  save(): Promise<unknown>;
  markdown(file: FilePresentation, target: PreviewTarget): ReactNode;
  html(file: FilePresentation): ReactNode;
  pdf(file: FilePresentation): ReactNode;
  media(file: FilePresentation): ReactNode;
}) {
  const [source, setSource] = useState(!!target?.line);
  const selectedLine = useRef<HTMLElement>(null);
  const key = JSON.stringify([target?.agent, target?.path, target?.asset]);
  useEffect(() => setSource(!!target?.line), [key, target?.line]);
  useLayoutEffect(() => {
    if (file && target?.line && source)
      selectedLine.current?.scrollIntoView({ block: "center" });
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
  const act = (operation: () => Promise<unknown>) => {
    void operation().catch(onError);
  };
  const toggle =
    file && ["markdown", "csv", "tsv", "json"].includes(file.format);
  return (
    <PreviewModal
      opened={!!target}
      onClose={onClose}
      title={file?.name || target?.path || "File preview"}
      size={
        file &&
        file.text.length < 2000 &&
        ["text", "json"].includes(file.format)
          ? "min(720px, 100vw)"
          : "min(900px, 100vw)"
      }
      overlayProps={{ blur: 0 }}
      className="workspace-file-modal"
    >
      <div className="file-preview-toolbar">
        {file && (
          <>
            <span title={file.mime}>
              {["text", "markdown", "csv", "tsv", "json"].includes(file.format)
                ? "Text file"
                : file.mime}
              {target?.line ? ` · Line ${target.line}` : ""}
            </span>
            <Button size="xs" variant="subtle" onClick={() => act(save)}>
              Save a copy
            </Button>
          </>
        )}
        {target && actions.canUseNativeFiles() && (
          <>
            <Button
              size="xs"
              variant="subtle"
              onClick={() => act(() => actions.openFile(target))}
            >
              Open
            </Button>
            <Button
              size="xs"
              variant="subtle"
              onClick={() => act(() => actions.revealFile(target))}
            >
              Show in folder
            </Button>
            <Button
              size="xs"
              variant="subtle"
              onClick={() => act(() => actions.previewFile(target))}
            >
              System preview
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
            onError={() => onError("Cannot load this image.")}
          />
        ) : file.format === "markdown" && target ? (
          <div className="file-preview-body">{markdown(file, target)}</div>
        ) : file.table ? (
          <>
            {file.table.error && <p role="alert">{file.table.error}</p>}
            {file.table.truncated && (
              <p role="status">Shows up to 200 rows and 100 columns.</p>
            )}
            <div className="file-preview-table">
              <table aria-label="File table">
                <tbody>
                  {file.table.rows.map((row, i) => (
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
        ) : file.json ? (
          <>
            {file.json.error && <p role="alert">{file.json.error}</p>}
            <pre className="file-preview-body file-preview-source">
              {file.json.text}
            </pre>
          </>
        ) : file.format === "html" ? (
          html(file)
        ) : file.format === "pdf" ? (
          pdf(file)
        ) : file.format === "audio" || file.format === "video" ? (
          media(file)
        ) : (
          <p>No preview for this file type.</p>
        ))}
    </PreviewModal>
  );
}
