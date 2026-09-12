import { createElement, type ReactNode } from "react";
import { marked } from "marked";
import { localFileLink } from "./fileLinks";

interface ProgressToken {
  type: string;
  text?: string;
  tokens?: ProgressToken[];
  items?: ProgressToken[];
  depth?: number;
  ordered?: boolean;
  start?: number;
  task?: boolean;
  checked?: boolean;
  href?: string;
  title?: string | null;
}

export function progressMarkdown(source: string): {
  nodes: ReactNode;
  supported: boolean;
} {
  let count = 0;
  const decoder = document.createElement("textarea");
  const decode = (text = "") => {
    // Decode Markdown character references without admitting HTML elements.
    decoder.innerHTML = text.replaceAll("<", "&lt;").replaceAll(">", "&gt;");
    return decoder.value;
  };
  const render = (tokens: ProgressToken[], depth = 0): ReactNode[] => {
    if (depth > 16)
      throw new Error("Progress nesting exceeds the renderer limit");
    return tokens.map((token, index) => {
      if (++count > 2048)
        throw new Error("Progress exceeds the renderer token limit");
      const children = () =>
        token.tokens ? render(token.tokens, depth + 1) : decode(token.text);
      switch (token.type) {
        case "space":
        case "def":
          return null;
        case "paragraph":
          return <p key={index}>{children()}</p>;
        case "heading":
          return createElement(`h${token.depth}`, { key: index }, children());
        case "strong":
          return <strong key={index}>{children()}</strong>;
        case "em":
          return <em key={index}>{children()}</em>;
        case "text":
          return <span key={index}>{children()}</span>;
        case "escape":
          return <span key={index}>{decode(token.text)}</span>;
        case "codespan":
          return <code key={index}>{token.text}</code>;
        case "br":
          return <br key={index} />;
        case "list":
          return createElement(
            token.ordered ? "ol" : "ul",
            {
              key: index,
              ...(token.ordered ? { start: token.start } : {}),
            },
            render(token.items || [], depth + 1),
          );
        case "list_item":
          return (
            <li key={index}>
              {token.task ? (token.checked ? "[x] " : "[ ] ") : null}
              {children()}
            </li>
          );
        case "link": {
          const href = decode(token.href).trim();
          const local = localFileLink(href);
          if (!local && !/^https?:\/\//i.test(href))
            throw new Error("Unsupported progress link");
          return (
            <a
              key={index}
              href={href}
              title={token.title || undefined}
              rel="noreferrer"
            >
              {children()}
            </a>
          );
        }
        default:
          throw new Error("Unsupported progress Markdown");
      }
    });
  };
  try {
    return {
      nodes: render(marked.lexer(source) as ProgressToken[]),
      supported: true,
    };
  } catch {
    // The whole file is rejected. No unsupported node is silently removed.
    return { nodes: null, supported: false };
  }
}
