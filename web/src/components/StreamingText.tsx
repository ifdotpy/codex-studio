import { memo, useMemo, useState } from "react";
import { Modal } from "@mantine/core";
import DOMPurify from "dompurify";
import { marked, type Token } from "marked";
import RichPreview from "./RichPreview";
import FilePreview, { type PreviewTarget } from "./FilePreview";
import {
  localFileLink,
  markdownUriPattern,
  relativeFileLocation,
} from "./fileLinks";

// Release complete paragraphs. Blank lines inside fenced code are not boundaries.
export function paragraphPrefix(text: string, streaming: boolean) {
  if (!streaming) return text;
  let end = 0,
    offset = 0,
    fence = "";
  for (const line of text.match(/[^\n]*\n|[^\n]+$/g) || []) {
    offset += line.length;
    const marker = line.match(/^ {0,3}(`{3,}|~{3,})/);
    if (marker) {
      if (!fence) fence = marker[1];
      else if (
        marker[1][0] === fence[0] &&
        marker[1].length >= fence.length &&
        /^ {0,3}(`{3,}|~{3,})\s*$/.test(line)
      ) {
        fence = "";
        end = offset;
      }
    } else if (!fence && /^\s*\n$/.test(line)) end = offset;
  }
  return text.slice(0, end);
}
const Block = memo(({ html }: { html: string }) => (
  <div className="markdown-block" dangerouslySetInnerHTML={{ __html: html }} />
));
export default function StreamingText({
  text,
  streaming = false,
  agentId,
}: {
  text: string;
  streaming?: boolean;
  agentId?: string;
}) {
  const [preview, setPreview] = useState<PreviewTarget | null>(null);
  const [linkError, setLinkError] = useState("");
  const visible = paragraphPrefix(text, streaming);
  const blocks = useMemo(() => {
    const tokens = marked.lexer(visible);
    marked.walkTokens(tokens, (token) => {
      if (token.type === "link" && relativeFileLocation(token.href))
        token.href = "./" + token.href;
    });
    const grouped: Token[] = [];
    for (const token of tokens) {
      if (token.type === "space") continue;
      const previous = grouped.at(-1);
      if (token.type === "html" && previous?.type === "html") {
        // Keep adjacent style and markup blocks in one isolated document.
        grouped[grouped.length - 1] = {
          ...previous,
          raw: previous.raw + "\n" + token.raw,
          text: previous.text + "\n" + token.text,
        };
      } else grouped.push(token);
    }
    return grouped.map((token) => {
      if (
        token.type === "code" &&
        /^(mermaid|html)$/i.test(token.lang?.trim() || "")
      ) {
        // An interrupted response can end with an open fence. Retain its source.
        const lines = token.raw.trimEnd().split("\n");
        const opening = lines[0].match(/^ {0,3}(`{3,}|~{3,})/);
        const closing = lines.at(-1)?.match(/^ {0,3}(`{3,}|~{3,})\s*$/);
        if (
          lines.length > 1 &&
          opening &&
          closing &&
          closing[1][0] === opening[1][0] &&
          closing[1].length >= opening[1].length
        ) {
          return {
            kind: token.lang!.trim().toLowerCase() as "mermaid" | "html",
            source: token.text,
          };
        }
      }
      if (token.type === "html")
        return { kind: "html" as const, source: token.text };
      return {
        html: DOMPurify.sanitize(
          marked.parser(Object.assign([token], { links: tokens.links })),
          {
            ALLOWED_URI_REGEXP: markdownUriPattern,
            FORBID_TAGS: [
              "img",
              "form",
              "input",
              "button",
              "iframe",
              "style",
              "script",
              "video",
              "audio",
            ],
            FORBID_ATTR: ["style"],
          },
        ),
      };
    });
  }, [visible]);
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
          if (!agentId)
            throw new Error(
              "This conversation has no workspace for file previews.",
            );
          setPreview({ agent: agentId, ...target });
        } catch (error) {
          event.preventDefault();
          setLinkError(
            error instanceof Error
              ? error.message
              : "Cannot open this file link.",
          );
        }
      }}
    >
      {blocks.map((block, i) =>
        block.kind ? (
          <RichPreview key={i} kind={block.kind} source={block.source} />
        ) : (
          <Block key={i} html={block.html!} />
        ),
      )}
      {preview && (
        <FilePreview target={preview} onClose={() => setPreview(null)} />
      )}
      <Modal
        opened={!!linkError}
        onClose={() => setLinkError("")}
        title="Cannot open file"
      >
        <p role="alert">{linkError}</p>
      </Modal>
    </div>
  );
}
