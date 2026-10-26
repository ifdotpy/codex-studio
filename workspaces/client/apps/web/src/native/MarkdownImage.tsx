import { useEffect, useMemo, useState } from "react";
import MarkdownImageView from "../components/MarkdownImageView";
import { localFileLink, relativeToDocument } from "../components/fileLinks";
import type { PreviewTarget } from "../components/FilePreviewView";
import ImageViewer from "../components/ImageViewer";
import PreviewModal from "./PreviewModal";
import FilePreview from "./FilePreview";
import { useNativePreview, useNativePreviewPort } from "./usePreview";
import type { NativeFilePreview } from "./preview";

export default function MarkdownImage({
  src,
  alt,
  agentId,
  basePath,
  localPath,
}: {
  src: string;
  alt: string;
  agentId?: string;
  basePath?: string;
  localPath?: string;
}) {
  const owner = useNativePreview();
  const port = useNativePreviewPort();
  const [loaded, setLoaded] = useState("");
  const [error, setError] = useState("");
  const [opened, setOpened] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [external, setExternal] = useState<{
    src: string;
    work: Promise<NativeFilePreview>;
  } | null>(null);
  const loadExternal = () => {
    // Send the native action in this button callback, before any await or effect.
    const work = port.externalImage(src);
    void work.catch(() => {});
    setExternal({ src, work });
  };
  const target = useMemo((): PreviewTarget | null => {
    try {
      const local =
        localPath === undefined ? localFileLink(src) : { path: localPath };
      return local && agentId
        ? {
            agent: agentId,
            ...relativeToDocument(
              local,
              localPath === undefined ? basePath : undefined,
            ),
          }
        : null;
    } catch {
      return null;
    }
  }, [src, agentId, basePath, localPath]);
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
  useEffect(() => {
    setOpened(false);
  }, [src, target]);
  useEffect(() => {
    let active = true;
    let session: NativeFilePreview | undefined;
    setLoaded("");
    setError("");
    if (
      /^data:image\/(?:png|jpeg|gif|webp);base64,[A-Za-z0-9+/=]+$/i.test(src)
    ) {
      setLoaded(src);
      return;
    }
    if (!target && !remoteHost) {
      setError("This image cannot be opened here.");
      return;
    }
    if (!target && external?.src !== src) return;
    const request = target
      ? owner.then((port) => port.open(target))
      : external!.work;
    void request
      .then(async (value) => {
        session = value;
        if (!active) {
          await value.release();
          return;
        }
        if (!["image", "svg"].includes(value.file.format))
          throw new Error("This image type is not supported.");
        setLoaded(value.file.url);
      })
      .catch((error: unknown) => {
        if (active)
          setError(error instanceof Error ? error.message : String(error));
      });
    return () => {
      active = false;
      if (session)
        void session
          .release()
          .catch((error: unknown) =>
            setError(error instanceof Error ? error.message : String(error)),
          );
    };
  }, [owner, src, target, remoteHost, external, attempt]);
  return (
    <MarkdownImageView
      image={loaded}
      error={error}
      alt={alt}
      remoteHost={remoteHost}
      onPreview={() => setOpened(true)}
      onError={() => setError("Cannot load this image.")}
      onRetry={() => {
        if (target) setAttempt(attempt + 1);
        else loadExternal();
      }}
      onLoad={loadExternal}
    >
      {target ? (
        <FilePreview
          target={opened ? target : null}
          onClose={() => setOpened(false)}
        />
      ) : (
        <PreviewModal
          opened={opened}
          onClose={() => setOpened(false)}
          title={alt || "Image preview"}
          size="xl"
        >
          {opened && <ImageViewer src={loaded} alt={alt} />}
        </PreviewModal>
      )}
    </MarkdownImageView>
  );
}
