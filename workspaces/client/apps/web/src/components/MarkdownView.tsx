import { createElement, memo, useState, type ReactNode } from "react";
import CodeBlock from "./CodeBlock";
import MarkdownImage from "./MarkdownImage";
import RichPreview from "./RichPreview";
import type {
  MarkdownNode,
  MarkdownTree as Tree,
  MarkdownBlock as Block,
} from "../generated/rustOwned";
type Context = { agentId?: string; basePath?: string };
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

export default function MarkdownView({
  blocks,
  streaming,
  initial,
  agentId,
  basePath,
}: {
  blocks: Block[];
  streaming: boolean;
  initial: number;
} & Context) {
  return (
    <>
      {blocks.map((block, index) =>
        "kind" in block ? (
          <RichPreview key={index} kind={block.kind} source={block.source} />
        ) : (
          <NativeMarkdownBlock
            key={index}
            block={block}
            enter={streaming && index >= initial}
            agentId={agentId}
            basePath={basePath}
          />
        ),
      )}
    </>
  );
}
