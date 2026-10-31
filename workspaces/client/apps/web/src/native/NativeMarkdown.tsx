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
  agentId,
  basePath,
}: { block: Tree } & Context) {
  function render(node: MarkdownNode, key: string): ReactNode {
    if (!node.tag) {
      if (!node.sentences?.length) return node.text || "";
      return node.sentences.map((sentence) => sentence.text).join("");
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
    { text: string; blocks: Block[] } | undefined
  >(() => {
    const blocks = !streaming
      ? (completedMarkdown.get(text) as Block[] | undefined)
      : undefined;
    return blocks ? { text, blocks } : undefined;
  });
  const parsedRef = useRef(parsed);
  parsedRef.current = parsed;
  const [error, setError] = useState("");
  useEffect(() => {
    if (streaming) return;
    let current = true;
    setError("");
    if (parsedRef.current?.text === text) return;
    const cached = completedMarkdown.get(text) as Block[] | undefined;
    if (cached) {
      setParsed({ text, blocks: cached });
      return;
    }
    const controller = new AbortController();
    const request = hasNativeClient()
      ? nativeRequest<Block[]>(
          "markdown.parse",
          { text, streaming: false },
          undefined,
          controller.signal,
        )
      : parseMarkdown(text);
    void request.then(
      (blocks) => {
        completedMarkdown.set(text, blocks);
        if (current) setParsed({ text, blocks });
      },
      (reason: unknown) => {
        if (current)
          setError(reason instanceof Error ? reason.message : String(reason));
      },
    );
    return () => {
      current = false;
      controller.abort();
    };
  }, [text, streaming]);
  const usable = !streaming && parsed?.text === text;
  return (
    <>
      {!streaming && error && <p role="alert">{error}</p>}
      {usable ? (
        parsed.blocks.map((block, index) =>
          "kind" in block ? (
            <RichPreview key={index} kind={block.kind} source={block.source} />
          ) : (
            <NativeMarkdownBlock
              key={index}
              block={block}
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
