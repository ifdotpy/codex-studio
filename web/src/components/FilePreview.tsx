import { Button, Loader, Modal } from "@mantine/core";
import { Download } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { api, errorText } from "../api";
import { isolatedDocument } from "./RichPreview";

export interface PreviewTarget {
  agent?: string;
  path?: string;
  asset?: string;
  line?: number;
}
export default function FilePreview({
  target,
  onClose,
}: {
  target: PreviewTarget | null;
  onClose: () => void;
}) {
  const [file, setFile] = useState<{
    name: string;
    mime: string;
    url: string;
    text: string;
  } | null>(null);
  const [error, setError] = useState("");
  const excerpt = useMemo(() => {
    if (!file || !target?.line) return null;
    let start = 0;
    for (let line = 1; line < target.line; line++) {
      const end = file.text.indexOf("\n", start);
      if (end < 0) return null;
      start = end + 1;
    }
    const end = file.text.indexOf("\n", start);
    const stop = end < 0 ? file.text.length : end;
    return {
      before: file.text.slice(0, start),
      line: file.text.slice(start, stop),
      after: file.text.slice(stop),
    };
  }, [file, target?.line]);
  const selectedLine = useRef<HTMLElement>(null);
  useEffect(() => {
    if (!file || !target?.line) return;
    const frame = requestAnimationFrame(() =>
      selectedLine.current?.scrollIntoView({ block: "center" }),
    );
    return () => cancelAnimationFrame(frame);
  }, [file, target?.line]);
  useEffect(() => {
    setFile(null);
    setError("");
    if (!target) return;
    let active = true,
      url = "";
    const query = new URLSearchParams(
      Object.entries(target).filter(
        ([key, value]) => key !== "line" && value != null,
      ) as [string, string][],
    );
    api(`/api/file?${query}`)
      .then((result) => {
        if (!active) return;
        const bytes = Uint8Array.from(atob(result.base64), (c) =>
          c.charCodeAt(0),
        );
        url = URL.createObjectURL(new Blob([bytes], { type: result.mime }));
        setFile({
          ...result,
          url,
          text: /^(image\/|application\/pdf)/.test(result.mime)
            ? ""
            : new TextDecoder().decode(bytes),
        });
      })
      .catch((e) => {
        if (active) setError(errorText(e));
      });
    return () => {
      active = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, [target?.agent, target?.path, target?.asset]);
  const image = file && /^image\/(png|jpeg|gif|webp)$/.test(file.mime);
  const html =
    file && (file.mime === "text/html" || /\.html?$/i.test(file.name));
  return (
    <Modal
      opened={!!target}
      onClose={onClose}
      title={file?.name || target?.path || "File preview"}
      size="xl"
      className="workspace-file-modal"
    >
      {error && (
        <p role="alert" className="workspace-error">
          {error}
        </p>
      )}
      {!file && !error && <Loader size="sm" aria-label="Load file" />}
      {file && (
        <>
          <div className="workspace-toolbar">
            <span className="workspace-muted">
              {file.mime}
              {target?.line ? ` · Line ${target.line}` : ""}
            </span>
            <Button
              component="a"
              href={file.url}
              download={file.name}
              variant="light"
              size="xs"
              leftSection={<Download size={14} />}
            >
              Download
            </Button>
          </div>
          {image ? (
            <img className="workspace-image" src={file.url} alt={file.name} />
          ) : html ? (
            <iframe
              title={file.name}
              className="workspace-preview-frame"
              sandbox=""
              referrerPolicy="no-referrer"
              srcDoc={isolatedDocument(file.text)}
            />
          ) : file.mime === "application/pdf" ? (
            <iframe
              title={file.name}
              className="workspace-preview-frame"
              src={file.url}
            />
          ) : /^(text\/|application\/(json|xml|javascript))/.test(file.mime) ||
            /\.(md|tsx?|jsx?|py|rs|toml|yaml|yml|log|diff|csv)$/i.test(
              file.name,
            ) ? (
            <pre className="workspace-file-text">
              {excerpt ? (
                <>
                  {excerpt.before}
                  <mark
                    ref={selectedLine}
                    data-file-line={target?.line}
                    style={{
                      background: "var(--mantine-color-violet-light)",
                      color: "inherit",
                    }}
                  >
                    {excerpt.line || " "}
                  </mark>
                  {excerpt.after}
                </>
              ) : (
                file.text
              )}
            </pre>
          ) : (
            <p className="workspace-empty">
              Preview is unavailable for this file type. Download the file to
              open it.
            </p>
          )}
        </>
      )}
    </Modal>
  );
}
