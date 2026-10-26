import { useEffect, useRef, useState } from "react";
import MarkdownView from "../components/MarkdownView";
export { NativeMarkdownBlock } from "../components/MarkdownView";
import { hasNativeClient, nativeRequest } from "./client";
import { parseMarkdown } from "../wasm/client";
import { completedMarkdown } from "./markdownCache";
import type { MarkdownBlock as Block } from "../generated/rustOwned";
type Context = { agentId?: string; basePath?: string };
export default function NativeMarkdown({
  text,
  streaming,
  agentId,
  basePath,
}: {
  text: string;
  streaming: boolean;
} & Context) {
  const [parsed, setParsed] = useState<
    { text: string; blocks: Block[]; complete: boolean } | undefined
  >(() => {
    const blocks = !streaming
      ? (completedMarkdown.get(text) as Block[] | undefined)
      : undefined;
    return blocks ? { text, blocks, complete: true } : undefined;
  });
  const parsedRef = useRef(parsed);
  parsedRef.current = parsed;
  const [error, setError] = useState("");
  const initial = useRef<number | undefined>(parsed?.blocks.length);
  useEffect(() => {
    let current = true;
    setError("");
    // A cache hit on mount is already the complete tree rendered by this instance.
    // Streaming output still needs a final parse when the turn completes.
    if (
      !streaming &&
      parsedRef.current?.complete &&
      parsedRef.current.text === text
    )
      return;
    const cached = !streaming
      ? (completedMarkdown.get(text) as Block[] | undefined)
      : undefined;
    if (cached) {
      initial.current ??= cached.length;
      setParsed({ text, blocks: cached, complete: true });
      return;
    }
    // Coalesce live tokens only. Completed history must not wait another frame.
    // Late replies cannot replace newer text.
    const controller = new AbortController();
    const parse = () => {
      const request = hasNativeClient()
        ? nativeRequest<Block[]>(
            "markdown.parse",
            { text, streaming },
            undefined,
            controller.signal,
          )
        : parseMarkdown(text, streaming);
      void request.then(
        (blocks) => {
          if (!streaming) completedMarkdown.set(text, blocks);
          if (!current) return;
          initial.current ??= streaming ? 0 : blocks.length;
          setParsed({ text, blocks, complete: !streaming });
        },
        (reason: unknown) => {
          if (current)
            setError(reason instanceof Error ? reason.message : String(reason));
        },
      );
    };
    const timer = streaming ? setTimeout(parse, 16) : undefined;
    if (!streaming) parse();
    return () => {
      current = false;
      clearTimeout(timer);
      controller.abort();
    };
  }, [text, streaming]);
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
