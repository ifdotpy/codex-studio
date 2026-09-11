import { useEffect, useMemo, useState } from "react";
import { Modal } from "@mantine/core";
import { api, errorText } from "../api";
import FilePreview, { type PreviewTarget } from "./FilePreview";
import { localFileLink } from "./fileLinks";

const raster = /^data:image\/(png|jpeg|gif|webp);base64,/i;
export default function MarkdownImage({
  src,
  alt,
  agentId,
}: {
  src: string;
  alt: string;
  agentId?: string;
}) {
  const [loaded, setLoaded] = useState("");
  const [error, setError] = useState("");
  const [remoteAllowed, setRemoteAllowed] = useState(false);
  const [preview, setPreview] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const target = useMemo((): PreviewTarget | null => {
    try {
      const local = localFileLink(src);
      return local && agentId ? { agent: agentId, ...local } : null;
    } catch {
      return null;
    }
  }, [src, agentId]);
  const remoteHost = useMemo(() => {
    try {
      const url = new URL(src);
      return /^https?:$/.test(url.protocol) && !url.username && !url.password
        ? url.hostname
        : "";
    } catch {
      return "";
    }
  }, [src]);
  const remote = !!remoteHost;
  useEffect(() => {
    setError("");
    setLoaded("");
    setRemoteAllowed(false);
    setPreview(false);
    if (raster.test(src)) {
      setLoaded(src);
      return;
    }
    if (!target) {
      if (!remote) setError("This image cannot be opened here.");
      return;
    }
    let active = true;
    const query = new URLSearchParams({
      agent: target.agent!,
      path: target.path!,
    });
    api(`/api/file?${query}`)
      .then((file) => {
        if (!active) return;
        if (!/^image\/(png|jpeg|gif|webp)$/.test(file.mime))
          throw new Error("This image type is not supported.");
        setLoaded(`data:${file.mime};base64,${file.base64}`);
      })
      .catch((e) => {
        if (active) setError(errorText(e));
      });
    return () => {
      active = false;
    };
  }, [src, target, remote, attempt]);
  const image = loaded || (remoteAllowed ? src : "");
  return (
    <span className="markdown-image">
      {image && !error ? (
        <button
          type="button"
          className="image-preview-button"
          aria-label={`Preview ${alt || "image"}`}
          onClick={(event) => {
            event.preventDefault();
            event.stopPropagation();
            setPreview(true);
          }}
        >
          <img
            src={image}
            alt={alt}
            loading="lazy"
            referrerPolicy="no-referrer"
            onError={() => setError("Cannot load this image.")}
          />
        </button>
      ) : (
        <span className="markdown-image-placeholder">
          <span>{alt || "Image"}</span>
          {error ? (
            <>
              <span role="alert">{error}</span>
              <button
                type="button"
                onClick={() => {
                  setError("");
                  if (remote) setRemoteAllowed(true);
                  else setAttempt(attempt + 1);
                }}
              >
                Retry image
              </button>
            </>
          ) : remote ? (
            <>
              <span>External image. Load it to contact {remoteHost}.</span>
              <button type="button" onClick={() => setRemoteAllowed(true)}>
                Load image
              </button>
            </>
          ) : (
            <span role="status">Load image…</span>
          )}
        </span>
      )}
      {target ? (
        <FilePreview
          target={preview ? target : null}
          onClose={() => setPreview(false)}
        />
      ) : (
        <Modal
          opened={preview}
          onClose={() => setPreview(false)}
          title={alt || "Image preview"}
          size="xl"
        >
          <img
            className="workspace-image"
            src={preview ? image : undefined}
            alt={alt}
            referrerPolicy="no-referrer"
          />
        </Modal>
      )}
    </span>
  );
}
