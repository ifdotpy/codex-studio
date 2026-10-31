import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, expect, it, vi } from "vitest";
import NativeMarkdown, { NativeMarkdownBlock } from "./NativeMarkdown";
import { completedMarkdown } from "./markdownCache";

const { effects, request } = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  request: vi.fn(
    async (
      _method: string,
      _params: unknown,
      _options: unknown,
      _signal: AbortSignal,
    ) => [],
  ),
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

it("shows streaming markdown literally without invoking the parser", async () => {
  vi.useFakeTimers();
  const text = "**Still streaming**\n\n```ts\nconst x = 1;";
  const html = renderToStaticMarkup(
    createElement(NativeMarkdown, { text, streaming: true }),
  );
  for (const effect of effects.splice(0)) effect();
  await vi.runAllTimersAsync();
  expect(html).toContain("**Still streaming**");
  expect(html).toContain("white-space:pre-wrap");
  expect(request).not.toHaveBeenCalled();
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

it("renders the native tree as text and preserves file links", () => {
  const html = renderToStaticMarkup(
    createElement(NativeMarkdownBlock, {
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
  expect(html).toContain("🦀 New");
  expect(html).toContain('href="src/file.ts:12"');
});

it("renders live text literally even when a completed tree has the same source", async () => {
  vi.useFakeTimers();
  const text = "Streaming source.";
  completedMarkdown.set(text, []);
  const html = renderToStaticMarkup(
    createElement(NativeMarkdown, { text, streaming: true }),
  );
  const cleanups = effects.splice(0).map((effect) => effect());
  await vi.runAllTimersAsync();
  expect(html).toContain(text);
  expect(request).not.toHaveBeenCalled();
  for (const cleanup of cleanups) cleanup?.();
});

it("starts completed history parsing immediately and cancels it on cleanup", () => {
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
  const signal = request.mock.calls[0]?.[3];
  completedCleanups.forEach((cleanup) => cleanup?.());
  expect(signal?.aborted).toBe(true);
});
