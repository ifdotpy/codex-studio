import {
  createElement,
  memo,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import CodeBlock from "../components/CodeBlock";
import MarkdownImage from "../components/MarkdownImage";
import RichPreview from "../components/RichPreview";
import { hasNativeClient, nativeRequest } from "./client";
import { parseMarkdown } from "../wasm/client";
import { completedMarkdown } from "./markdownCache";

import type {
  MarkdownNode,
  MarkdownTree as Tree,
  MarkdownBlock as Block,
} from "../generated/rustOwned";
type Context = { agentId?: string | undefined; basePath?: string | undefined };
const textContent = (node: MarkdownNode): string =>
  (node.text || "") + (node.children || []).map(textContent).join("");

// Rust supplies an inert tree with an allowlisted tag/attribute vocabulary.
// Both native and WASM hosts use the same Rust parser and sentence segmenter.
export const NativeMarkdownBlock = memo(function NativeMarkdownBlock({
  block,
  enter,
  agentId,
  basePath,
}: { block: Tree; enter: boolean } & Context) {
  const [initialLength] = useState(enter ? 0 : block.length);
  function render(node: MarkdownNode, key: string): ReactNode {
    if (!node.tag) {
      if (!node.sentences?.length) return node.text || "";
      return node.sentences.map((sentence) => (
        <span
          key={`${key}:${sentence.index}`}
          className={
            sentence.index >= initialLength ? "sentence-enter" : undefined
          }
        >
          {sentence.text}
        </span>
      ));
    }
    if (node.tag === "img")
      return (
        <MarkdownImage
          key={key}
          src={node.props?.src || ""}
          alt={node.props?.alt || ""}
          agentId={agentId}
          basePath={basePath}
        />
      );
    const code = node.children?.[0];
    if (node.tag === "pre" && code?.tag === "code")
      return (
        <CodeBlock
          key={key}
          source={textContent(code)}
          language={code.props?.className?.match(/(?:^|\s)language-(\S+)/)?.[1]}
        />
      );
    const children = node.children?.map((child, index) =>
      render(child, `${key}.${index}`),
    );
    return createElement(
      node.tag,
      { ...node.props, key },
      children?.length ? children : undefined,
    );
  }
  return (
    <div className="markdown-block">
      {block.nodes.map((node, index) => render(node, String(index)))}
    </div>
  );
});

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
        parsed.blocks.map((block, index) =>
          "kind" in block ? (
            <RichPreview key={index} kind={block.kind} source={block.source} />
          ) : (
            <NativeMarkdownBlock
              key={index}
              block={block}
              enter={streaming && index >= (initial.current ?? 0)}
              agentId={agentId}
              basePath={basePath}
            />
          ),
        )
      ) : (
        <div className="markdown-block" style={{ whiteSpace: "pre-wrap" }}>
          {text}
        </div>
      )}
    </>
  );
}
