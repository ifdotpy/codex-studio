import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, it } from "vitest";
import { NativeMarkdownBlock } from "./NativeMarkdown";

it("renders the native tree as text and preserves file links and sentence offsets", () => {
  const html = renderToStaticMarkup(
    createElement(NativeMarkdownBlock, {
      enter: false,
      block: {
        length: 5,
        nodes: [
          {
            tag: "p",
            children: [
              {
                text: "<script>",
                sentences: [
                  { text: "<script>", index: 0 },
                  { text: "🦀 New", index: 8 },
                ],
              },
              {
                tag: "a",
                props: { href: "src/file.ts:12" },
                children: [{ text: "file" }],
              },
            ],
          },
        ],
      },
    }),
  );
  expect(html).toContain("&lt;script&gt;");
  expect(html).not.toContain("<script>");
  expect(html).toContain('class="sentence-enter">🦀 New');
  expect(html).toContain('href="src/file.ts:12"');
});
