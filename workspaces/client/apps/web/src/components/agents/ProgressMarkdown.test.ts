import { afterEach, expect, test } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { progressMarkdown } from "./ProgressMarkdown";

const originalDocument = globalThis.document;

afterEach(() => {
  globalThis.document = originalDocument;
});

test("plain email and unsupported autolinks render as text", () => {
  let html = "";
  globalThis.document = {
    createElement: () => ({
      set innerHTML(value: string) {
        html = value;
      },
      get value() {
        return html.replaceAll("&lt;", "<").replaceAll("&gt;", ">");
      },
    }),
  } as unknown as Document;
  const result = progressMarkdown(
    "Contact person@example.com. See <ftp://example.com/file> and [the site](https://example.com).",
  );
  expect(result.supported).toBe(true);
  const markup = renderToStaticMarkup(result.nodes);
  expect(markup).toContain("person@example.com");
  expect(markup).toContain("ftp://example.com/file");
  expect(markup).not.toContain("mailto:");
  expect(markup).not.toContain('href="ftp:');
  expect(markup).toContain('href="https://example.com"');
  expect(
    progressMarkdown("[person@example.com](mailto:person@example.com)")
      .supported,
  ).toBe(false);
});
