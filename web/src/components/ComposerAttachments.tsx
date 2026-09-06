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
export const attachmentLimit = 20 * 1024 * 1024;
export async function uploadAttachment(
  agent: string,
  file: globalThis.File,
): Promise<Attachment> {
  if (!file.size || file.size > attachmentLimit)
    throw new Error("Files must contain 1 byte to 20 MiB.");
  const base64 = await new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error(`Cannot read ${file.name}.`));
    reader.onload = () => resolve(String(reader.result).split(",")[1]);
    reader.readAsDataURL(file);
  });
  const response = await api("/api/assets", {
    agent,
    name: file.name,
    base64,
    id: crypto.randomUUID(),
  });
  const asset: Attachment = response.asset || response;
  return {
    ...asset,
    preview: asset.image ? `data:${asset.mime};base64,${base64}` : undefined,
  };
}
export default function ComposerAttachments(p: {
  notify: (message: string) => void;
  assets: Attachment[];
  uploading: boolean;
  disabled: boolean;
  add: (files: globalThis.File[]) => void;
  remove: (id: string) => void;
}) {
  const input = useRef<HTMLInputElement>(null);
  return (
    <div className="composer-attachments">
      <input
        ref={input}
        type="file"
        multiple
        hidden
        aria-label="Choose attachments"
        onChange={(event) => {
          p.add(Array.from(event.target.files || []));
          event.target.value = "";
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
              p.add(
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
      {!!p.assets.length && (
        <div className="attachment-list">
          {p.assets.map((asset) => (
            <div
              className="attachment-chip"
              key={asset.id}
              title={`${asset.name} · ${Math.ceil(asset.size / 1024)} KiB`}
            >
              {asset.preview ? (
                <img src={asset.preview} alt="" />
              ) : (
                <File size={15} />
              )}
              <span>{asset.name}</span>
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

function AttachmentImage(p: { asset: Attachment }) {
  const [src, setSrc] = useState<string>();
  useEffect(() => {
    let live = true;
    api(`/api/file?asset=${encodeURIComponent(p.asset.id)}`)
      .then((file) => {
        if (
          live &&
          ["image/png", "image/jpeg", "image/gif", "image/webp"].includes(
            file.mime,
          )
        )
          setSrc(`data:${file.mime};base64,${file.base64}`);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [p.asset.id]);
  return src ? <img src={src} alt={p.asset.name} loading="lazy" /> : null;
}

export function MessageAttachments(p: {
  assets: Attachment[];
  notify: (message: string) => void;
}) {
  const [preview, setPreview] = useState<string | null>(null);
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
            {asset.image && <AttachmentImage asset={asset} />}
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
