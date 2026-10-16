import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, expect, it, vi } from "vitest";
import NativeMarkdown, { NativeMarkdownBlock } from "./NativeMarkdown";
import { completedMarkdown } from "./markdownCache";

const { effects, request } = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  request: vi.fn(async () => []),
}));
vi.mock("./client", async (original) => ({
  ...(await original<typeof import("./client")>()),
  hasNativeClient: () => true,
  nativeRequest: request,
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useEffect: (effect: () => void | (() => void)) => effects.push(effect),
}));
afterEach(() => {
  effects.length = 0;
  vi.useRealTimers();
  request.mockClear();
  vi.restoreAllMocks();
});

it("reuses the completed tree hydrated on mount instead of decoding it again in the effect", () => {
  const text = "Cached completed paragraph.";
  completedMarkdown.set(text, [
    { length: text.length, nodes: [{ tag: "p", children: [{ text }] }] },
  ]);
  const read = vi.spyOn(completedMarkdown, "get");
  const html = renderToStaticMarkup(
    createElement(NativeMarkdown, { text, streaming: false }),
  );
  expect(html).toContain(text);
  for (const effect of effects.splice(0)) effect();
  expect(read).toHaveBeenCalledTimes(1);
});

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

it("still parses streaming text even when a completed tree has the same source", async () => {
  vi.useFakeTimers();
  const text = "Streaming source.";
  completedMarkdown.set(text, []);
  renderToStaticMarkup(
    createElement(NativeMarkdown, { text, streaming: true }),
  );
  const cleanups = effects.splice(0).map((effect) => effect());
  await vi.runAllTimersAsync();
  expect(request).toHaveBeenCalledWith(
    "markdown.parse",
    { text, streaming: true },
    undefined,
    expect.any(AbortSignal),
  );
  for (const cleanup of cleanups) cleanup?.();
});

it("starts completed history parsing immediately and still cancels a superseded stream", () => {
  vi.useFakeTimers();
  renderToStaticMarkup(
    createElement(NativeMarkdown, {
      text: "Uncached finished history.",
      streaming: false,
    }),
  );
  const completedCleanups = effects.splice(0).map((effect) => effect());
  expect(request).toHaveBeenCalledTimes(1);
  expect(vi.getTimerCount()).toBe(0);
  completedCleanups.forEach((cleanup) => cleanup?.());
  request.mockClear();
  renderToStaticMarkup(
    createElement(NativeMarkdown, {
      text: "Superseded live token",
      streaming: true,
    }),
  );
  const streamingCleanups = effects.splice(0).map((effect) => effect());
  expect(request).not.toHaveBeenCalled();
  streamingCleanups.forEach((cleanup) => cleanup?.());
  vi.runAllTimers();
  expect(request).not.toHaveBeenCalled();
});
