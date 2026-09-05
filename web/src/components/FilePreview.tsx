import { Button, Loader, Modal } from "@mantine/core";
import { Download } from "lucide-react";
import { useEffect, useState } from "react";
import { api, errorText } from "../api";

export interface PreviewTarget {
  agent?: string;
  path?: string;
  asset?: string;
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
  useEffect(() => {
    setFile(null);
    setError("");
    if (!target) return;
    let active = true,
      url = "";
    const query = new URLSearchParams(
      Object.entries(target).filter(([, value]) => value != null) as [
        string,
        string,
      ][],
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
            <span className="workspace-muted">{file.mime}</span>
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
              srcDoc={
                "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:;\">" +
                file.text
              }
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
            <pre className="workspace-file-text">{file.text}</pre>
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
