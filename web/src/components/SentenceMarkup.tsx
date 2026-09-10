import { createElement, memo, useMemo, useState, type ReactNode } from "react";
import { sentences } from "./sentenceStream";

// The caller sanitizes HTML. React retains old nodes when a paragraph grows.
export default memo(function SentenceMarkup({
  html,
  enter,
}: {
  html: string;
  enter: boolean;
}) {
  const [initialLength] = useState(() => {
    if (enter) return 0;
    const template = document.createElement("template");
    template.innerHTML = html;
    return template.content.textContent?.length || 0;
  });
  const children = useMemo(() => {
    const template = document.createElement("template");
    template.innerHTML = html;
    let offset = 0;
    const render = (node: Node, key: string, literal = false): ReactNode => {
      if (node.nodeType === Node.TEXT_NODE) {
        const text = node.textContent || "";
        const start = offset;
        offset += text.length;
        if (literal || !text.trim()) return text;
        return sentences(text).map((part) => (
          <span
            key={`${key}:${part.index}`}
            className={
              start + part.index >= initialLength ? "sentence-enter" : undefined
            }
          >
            {part.text}
          </span>
        ));
      }
      if (!(node instanceof Element)) return null;
      const tag = node.tagName.toLowerCase();
      const props: Record<string, unknown> = { key };
      for (const attr of node.attributes) {
        const name =
          (
            {
              class: "className",
              for: "htmlFor",
              tabindex: "tabIndex",
              colspan: "colSpan",
              rowspan: "rowSpan",
            } as Record<string, string>
          )[attr.name] || attr.name;
        props[name] = attr.value;
      }
      const childNodes = Array.from(node.childNodes, (child, i) =>
        render(
          child,
          `${key}.${i}`,
          literal || tag === "pre" || tag === "code",
        ),
      );
      return createElement(
        tag,
        props,
        childNodes.length ? childNodes : undefined,
      );
    };
    return Array.from(template.content.childNodes, (node, i) =>
      render(node, String(i)),
    );
  }, [html, initialLength]);
  return <div className="markdown-block">{children}</div>;
});
