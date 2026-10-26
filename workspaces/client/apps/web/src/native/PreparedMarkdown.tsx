import { useEffect, useRef, useState } from "react";
import MarkdownView from "../components/MarkdownView";
import type { MarkdownBlock } from "../generated/rustOwned";
import { useServerServices } from "./servicesContext";
export { NativeMarkdownBlock } from "../components/MarkdownView";

/** Rust prepares the tree. This view retains only the current presentation. */
export default function PreparedMarkdown({
  text,
  streaming = false,
  agentId,
  basePath,
}: {
  text: string;
  streaming?: boolean;
  agentId?: string;
  basePath?: string;
}) {
  const services = useServerServices();
  const [parsed, setParsed] = useState<{
    text: string;
    blocks: MarkdownBlock[];
  } | null>(null);
  const [error, setError] = useState("");
  const initial = useRef<number | undefined>(undefined);
  useEffect(() => {
    let current = true;
    setError("");
    void services.context
      .then((context) =>
        context.request<MarkdownBlock[]>("markdown.parse", { text, streaming }),
      )
      .then(
        (blocks) => {
          if (!current) return;
          initial.current ??= streaming ? 0 : blocks.length;
          setParsed({ text, blocks });
        },
        (error: unknown) => {
          if (current)
            setError(error instanceof Error ? error.message : String(error));
        },
      );
    return () => {
      current = false;
    };
  }, [services, text, streaming]);
  const usable =
    parsed &&
    (parsed.text === text || (streaming && text.startsWith(parsed.text)));
  return (
    <>
      {error && <p role="alert">{error}</p>}
      {usable ? (
        <MarkdownView
          blocks={parsed.blocks}
          streaming={streaming}
          initial={initial.current ?? 0}
          agentId={agentId}
          basePath={basePath}
        />
      ) : (
        <div className="markdown-block" style={{ whiteSpace: "pre-wrap" }}>
          {text}
        </div>
      )}
    </>
  );
}
