import {
  useServerApi,
  useServerFileActions,
} from "../servers/ServerServicesContext";
import { Loader } from "@mantine/core";
import { lazy, Suspense, useEffect, useState } from "react";
import "../api";
import { isolatedDocument } from "./RichPreview";
import { fileFormat, parseDelimited, safeSvg } from "./filePreviewFormats";
import FilePreviewView, {
  type FilePresentation,
  type PreviewTarget,
} from "./FilePreviewView";
export type { PreviewTarget } from "./FilePreviewView";
const Markdown = lazy(() => import("./conversation/transcript/StreamingText"));
function formattedJson(text: string) {
  try {
    return { text: JSON.stringify(JSON.parse(text), null, 2), error: "" };
  } catch {
    return { text, error: "Invalid JSON. The original source is shown." };
  }
}
type LoadedFile = FilePresentation & { key: string; bytes: Uint8Array };
export default function FilePreview({
  target,
  onClose,
}: {
  target: PreviewTarget | null;
  onClose(): void;
}) {
  const actions = useServerFileActions();
  const { get } = useServerApi();
  const key = JSON.stringify([target?.agent, target?.path, target?.asset]);
  const [loaded, setLoaded] = useState<LoadedFile | null>(null);
  const file = loaded?.key === key ? loaded : null;
  const [failure, setFailure] = useState<{
    key: string;
    error: unknown;
  } | null>(null);
  const error = failure?.key === key ? failure.error : null;
  useEffect(() => {
    setLoaded(null);
    setFailure(null);
    if (!target) return;
    let active = true,
      url = "";
    const query = {
      ...(target.agent ? { agent: target.agent } : {}),
      ...(target.path ? { path: target.path } : {}),
      ...(target.asset ? { asset: target.asset } : {}),
    };
    get("/api/file", { query })
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
          table: ["csv", "tsv"].includes(format)
            ? parseDelimited(text, format === "tsv" ? "\t" : ",")
            : null,
          json: format === "json" ? formattedJson(text) : null,
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
  return (
    <FilePreviewView
      target={target}
      file={file}
      error={error}
      onClose={onClose}
      onError={(error) => setFailure({ key, error })}
      actions={actions}
      save={() =>
        file
          ? actions.saveFile({
              name: file.name,
              mime: file.mime,
              data: file.bytes,
            })
          : Promise.resolve(false)
      }
      markdown={(file, target) => (
        <Suspense fallback={<Loader size="sm" />}>
          <Markdown
            text={file.text}
            agentId={target.agent}
            basePath={target.asset ? undefined : target.path}
          />
        </Suspense>
      )}
      html={(file) => (
        <iframe
          title={file.name}
          className="workspace-preview-frame"
          sandbox=""
          referrerPolicy="no-referrer"
          srcDoc={isolatedDocument(file.text)}
        />
      )}
      pdf={(file) => (
        <iframe
          title={file.name}
          className="workspace-preview-frame"
          src={file.url}
        />
      )}
      media={(file) =>
        file.format === "audio" ? (
          <audio
            className="file-preview-media"
            controls
            preload="metadata"
            src={file.url}
            onError={() =>
              setFailure({ key, error: "Cannot play this audio format." })
            }
          />
        ) : (
          <video
            className="file-preview-media"
            controls
            playsInline
            preload="metadata"
            src={file.url}
            onError={() =>
              setFailure({ key, error: "Cannot play this video format." })
            }
          />
        )
      }
    />
  );
}
