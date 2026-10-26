import { Button, Loader } from "@mantine/core";
import { Check, Code, Copy, Download, Eye } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import "./rich-preview.css";

export type RichPreviewMode = "preview" | "source";
export default function RichPreviewView({
  kind,
  source,
  preview,
  error,
  busy,
  copy,
  save,
}: {
  kind: "mermaid" | "html";
  source: string;
  preview: ReactNode;
  error?: string;
  busy?: boolean;
  copy(text: string): Promise<unknown>;
  save(mode: RichPreviewMode): Promise<unknown>;
}) {
  const [mode, setMode] = useState<RichPreviewMode>("preview");
  const [copied, setCopied] = useState(false);
  const [actionError, setActionError] = useState("");
  useEffect(() => {
    setCopied(false);
    setActionError("");
  }, [source]);
  return (
    <section
      className="rich-preview"
      data-preview-kind={kind}
      aria-label={kind === "mermaid" ? "Mermaid diagram" : "HTML preview"}
    >
      <div className="rich-preview-toolbar">
        <span className="rich-preview-label">
          {kind === "mermaid" ? "Mermaid" : "HTML"}
        </span>
        <div
          className="rich-preview-modes"
          role="group"
          aria-label="Preview display"
        >
          <Button
            size="compact-xs"
            variant={mode === "preview" ? "light" : "subtle"}
            aria-pressed={mode === "preview"}
            onClick={() => setMode("preview")}
            leftSection={<Eye size={13} />}
          >
            Preview
          </Button>
          <Button
            size="compact-xs"
            variant={mode === "source" ? "light" : "subtle"}
            aria-pressed={mode === "source"}
            onClick={() => setMode("source")}
            leftSection={<Code size={13} />}
          >
            Source
          </Button>
        </div>
        <Button
          size="compact-xs"
          variant="subtle"
          aria-label="Copy source"
          title="Copy source"
          onClick={() => {
            void copy(source).then(
              () => {
                setCopied(true);
                setActionError("");
              },
              () =>
                setActionError(
                  "Cannot copy. Open Source and select the text to copy it.",
                ),
            );
          }}
        >
          {copied ? <Check size={14} /> : <Copy size={14} />}
        </Button>
        <Button
          size="compact-xs"
          variant="subtle"
          aria-label="Download preview"
          title="Download preview"
          onClick={() => {
            void save(mode).catch((error: unknown) =>
              setActionError(
                error instanceof Error ? error.message : String(error),
              ),
            );
          }}
        >
          <Download size={14} />
        </Button>
      </div>
      {actionError && (
        <p role="alert" className="rich-preview-error">
          {actionError}
        </p>
      )}
      {mode === "source" ? (
        <pre className="rich-preview-source">
          <code>{source}</code>
        </pre>
      ) : error ? (
        <div className="rich-preview-error" role="alert">
          <strong>
            {kind === "mermaid"
              ? "Cannot render this diagram"
              : "Preview error"}
          </strong>
          <pre>{error}</pre>
          <p>Open Source to inspect the original text.</p>
        </div>
      ) : busy ? (
        <div className="rich-preview-loading">
          <Loader size="sm" />
          {kind === "mermaid" ? "Render diagram" : "Render preview"}
        </div>
      ) : (
        preview
      )}
      <div className="rich-preview-note">
        {kind === "html" ? "Static preview." : "Diagram preview."}
      </div>
    </section>
  );
}
