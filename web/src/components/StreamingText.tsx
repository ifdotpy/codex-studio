import { memo, useMemo } from "react";
import DOMPurify from "dompurify";
import { marked } from "marked";

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
}: {
  text: string;
  streaming?: boolean;
}) {
  const visible = paragraphPrefix(text, streaming);
  const blocks = useMemo(() => {
    const tokens = marked.lexer(visible);
    return tokens
      .filter((t) => t.type !== "space")
      .map((token) =>
        DOMPurify.sanitize(
          marked.parser(Object.assign([token], { links: tokens.links })),
          {
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
      );
  }, [visible]);
  return (
    <div className="prose" data-streaming={streaming || undefined}>
      {blocks.map((html, i) => (
        <Block key={i} html={html} />
      ))}
    </div>
  );
}
