import NativeMarkdown from "../../../native/NativeMarkdown";
import { memo, useEffect, useState } from "react";
import PreviewModal from "../../PreviewModal";
import FilePreview, { type PreviewTarget } from "../../FilePreview";
import { localFileLink } from "../../fileLinks";
import { relativeToDocument } from "../../filePreviewFormats";

export default memo(function StreamingText({
  text,
  streaming = false,
  agentId,
  basePath,
}: {
  text: string;
  streaming?: boolean;
  agentId?: string | undefined;
  basePath?: string | undefined;
}) {
  const [preview, setPreview] = useState<PreviewTarget | null>(null);
  const [linkError, setLinkError] = useState("");
  const [linkDialogMounted, setLinkDialogMounted] = useState(false);
  const [linkDialogReady, setLinkDialogReady] = useState(false);
  // Mantine captures the focus target on false -> true, including first open.
  useEffect(() => {
    if (linkDialogMounted) setLinkDialogReady(true);
  }, [linkDialogMounted]);
  return (
    <div
      className="prose"
      data-streaming={streaming || undefined}
      onClick={(event) => {
        const link = (event.target as Element).closest("a[href]");
        if (
          !(link instanceof HTMLAnchorElement) ||
          link.hasAttribute("download")
        )
          return;
        try {
          const target = localFileLink(link.getAttribute("href") || "");
          if (!target) return;
          event.preventDefault();
          // React portal events still reach the outer transcript Markdown.
          event.stopPropagation();
          if (!agentId)
            throw new Error(
              "This conversation has no workspace for file previews.",
            );
          setPreview({
            agent: agentId,
            ...relativeToDocument(target, basePath),
          });
        } catch (error) {
          event.preventDefault();
          event.stopPropagation();
          setLinkDialogMounted(true);
          setLinkError(
            error instanceof Error
              ? error.message
              : "Cannot open this file link.",
          );
        }
      }}
    >
      <NativeMarkdown
        text={text}
        streaming={streaming}
        agentId={agentId}
        basePath={basePath}
      />
      {preview && (
        <FilePreview target={preview} onClose={() => setPreview(null)} />
      )}
      {linkDialogMounted && (
        <PreviewModal
          opened={!!linkError && linkDialogReady}
          onClose={() => setLinkError("")}
          title="Cannot open file"
        >
          <p role="alert">{linkError}</p>
        </PreviewModal>
      )}
    </div>
  );
});
