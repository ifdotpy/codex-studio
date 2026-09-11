import { ActionIcon, Button, Loader } from "@mantine/core";
import { File, Paperclip, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
import FilePreview from "./FilePreview";

export interface Attachment {
  id: string;
  name: string;
  mime: string;
  image: boolean;
  size: number;
  preview?: string;
}
export { attachmentLimit } from "../sync/uploads";
export default function ComposerAttachments(p: {
  notify: (message: string) => void;
  assets: Attachment[];
  uploading: boolean;
  disabled: boolean;
  add: (files: globalThis.File[]) => Promise<void>;
  remove: (id: string) => void;
}) {
  const input = useRef<HTMLInputElement>(null);
  const [preview, setPreview] = useState<string | null>(null);
  const unsavedFiles = useRef<{
    files: globalThis.File[];
    add: typeof p.add;
  } | null>(null);
  const [saveError, setSaveError] = useState("");
  const saveFiles = async (files: globalThis.File[], add = p.add) => {
    if (!files.length) return;
    const pending = { files, add };
    unsavedFiles.current = pending;
    try {
      await add(files);
      if (unsavedFiles.current !== pending) return;
      unsavedFiles.current = null;
      setSaveError("");
      if (input.current) input.current.value = "";
    } catch (error) {
      if (unsavedFiles.current !== pending) return;
      setSaveError(errorText(error));
      p.notify(errorText(error));
    }
  };
  return (
    <div className="composer-attachments">
      <FilePreview
        target={preview ? { asset: preview } : null}
        onClose={() => setPreview(null)}
      />
      <input
        ref={input}
        type="file"
        multiple
        hidden
        aria-label="Choose attachments"
        onChange={(event) => {
          void saveFiles(Array.from(event.currentTarget.files || []));
        }}
      />
      <ActionIcon
        type="button"
        size={28}
        variant="subtle"
        className="attach-button"
        aria-label="Attach files"
        title="Attach files"
        disabled={p.disabled || p.uploading || p.assets.length >= 8}
        onClick={() => {
          if (!window.codexDesktop) {
            input.current?.click();
            return;
          }
          void window.codexDesktop
            .pickFiles()
            .then((files) => {
              return saveFiles(
                files.map(
                  (file) =>
                    new globalThis.File(
                      [
                        Uint8Array.from(atob(file.data), (c) =>
                          c.charCodeAt(0),
                        ),
                      ],
                      file.name,
                      { type: file.mime },
                    ),
                ),
              );
            })
            .catch((error) => p.notify(errorText(error)));
        }}
      >
        {p.uploading ? <Loader size={13} /> : <Paperclip size={16} />}
      </ActionIcon>
      {saveError && (
        <p role="status">
          {saveError}{" "}
          <button
            type="button"
            onClick={() => {
              const pending = unsavedFiles.current;
              if (pending) void saveFiles(pending.files, pending.add);
            }}
          >
            Retry saving files
          </button>
        </p>
      )}
      {!!p.assets.length && (
        <div className="attachment-list">
          {p.assets.map((asset) => (
            <div
              className="attachment-chip"
              key={asset.id}
              title={`${asset.name} · ${Math.ceil(asset.size / 1024)} KiB`}
            >
              <button
                type="button"
                className="attachment-preview-button"
                aria-label={`Preview ${asset.name}`}
                onClick={() => setPreview(asset.id)}
              >
                {asset.preview ? (
                  <img src={asset.preview} alt="" />
                ) : (
                  <File size={15} />
                )}
                <span>{asset.name}</span>
              </button>
              <ActionIcon
                type="button"
                size="xs"
                aria-label={`Remove ${asset.name}`}
                onClick={() => p.remove(asset.id)}
              >
                <X size={12} />
              </ActionIcon>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function AttachmentImage(p: { asset: Attachment; onPreview: () => void }) {
  const [src, setSrc] = useState<string>();
  const [error, setError] = useState("");
  useEffect(() => {
    let live = true;
    setSrc(undefined);
    setError("");
    api(`/api/file?asset=${encodeURIComponent(p.asset.id)}`)
      .then((file) => {
        if (
          live &&
          ["image/png", "image/jpeg", "image/gif", "image/webp"].includes(
            file.mime,
          )
        )
          setSrc(`data:${file.mime};base64,${file.base64}`);
        else if (live) setError("This image type has no preview.");
      })
      .catch((error) => {
        if (live) setError(errorText(error));
      });
    return () => {
      live = false;
    };
  }, [p.asset.id]);
  return error ? (
    <p role="alert">{error}</p>
  ) : src ? (
    <button
      type="button"
      className="image-preview-button"
      aria-label={`Preview ${p.asset.name}`}
      onClick={p.onPreview}
    >
      <img
        src={src}
        alt={p.asset.name}
        loading="lazy"
        onError={() => setError("Cannot load this image.")}
      />
    </button>
  ) : (
    <span role="status">Load image…</span>
  );
}

export function MessageAttachments(p: {
  assets: Attachment[];
  notify: (message: string) => void;
}) {
  const [preview, setPreview] = useState<string | null>(null);
  if (!p.assets.length) return null;
  return (
    <>
      <FilePreview
        target={preview ? { asset: preview } : null}
        onClose={() => setPreview(null)}
      />
      <div className="message-attachments">
        {p.assets.map((asset) => (
          <div
            key={asset.id}
            className={asset.image ? "message-attachment-image" : ""}
          >
            {asset.image && (
              <AttachmentImage
                asset={asset}
                onPreview={() => setPreview(asset.id)}
              />
            )}
            <Button
              variant="default"
              size="compact-sm"
              leftSection={<File size={14} />}
              onClick={() => setPreview(asset.id)}
            >
              {asset.name}
            </Button>
          </div>
        ))}
      </div>
    </>
  );
}
