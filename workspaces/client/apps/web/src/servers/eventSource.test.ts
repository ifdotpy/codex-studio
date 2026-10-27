import assert from "node:assert/strict";
/// <reference lib="es2024.promise" />
import { expect, it, vi, afterEach } from "vitest";
import { EventStreamParser } from "./eventSource";
afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
});
it("parses signed stream frames across BOM, CRLF and blank-line chunk boundaries", () => {
  const frames: [string, string][] = [];
  const parser = new EventStreamParser((type, data) =>
    frames.push([type, data]),
  );
  parser.push("\uFEFF: keepalive\r");
  parser.push('\nevent: api-schema\r\ndata: {"hash":"abc"}\r\n\r');
  parser.push("\nevent: resources\ndata: line one\ndata: line two\n\n");
  expect(frames).toEqual([
    ["api-schema", '{"hash":"abc"}'],
    ["resources", "line one\nline two"],
  ]);
});
it("rejects an oversized incomplete event", () => {
  const parser = new EventStreamParser(() => {});
  expect(() => parser.push("x".repeat(4 * 1024 * 1024 + 1))).toThrow(
    "size limit",
  );
});
it.each(["", "&tokenRates=false", "&tokenRates=true"])(
  "uses signed fetch for remote events with selector %s and cancels the stream on close",
  async (selector) => {
    vi.stubGlobal("location", { origin: "http://studio.test", search: "" });
    const { SignedEventSource } = await import("./eventSource");
    let stream!: ReadableStreamDefaultController<Uint8Array>;
    const cancelled = Promise.withResolvers<void>();
    const delivered = Promise.withResolvers<void>();
    const fetchRequest = vi.fn(async (request: Request) => {
      expect(new URL(request.url).search).toBe(`?protocol=3${selector}`);
      expect(request.headers.get("Accept")).toBe("text/event-stream");
      expect(request.redirect).toBe("error");
      return new Response(
        new ReadableStream<Uint8Array>({
          start(controller) {
            stream = controller;
          },
          cancel: () => cancelled.resolve(),
        }),
        { headers: { "Content-Type": "text/event-stream" } },
      );
    });
    const source = new SignedEventSource(
      `/api/sync/stream?protocol=3${selector}`,
      fetchRequest,
    );
    const frames: string[] = [];
    source.addEventListener("resources", (event) => {
      frames.push((event as MessageEvent).data);
      delivered.resolve();
    });
    const errors = vi.fn();
    source.onerror = errors;
    expect(fetchRequest).toHaveBeenCalledOnce();
    const bytes = new TextEncoder().encode(
      "event: resources\r\ndata: Привет 🦀\r\n\r\n",
    );
    // Single-byte chunks split both multibyte UTF-8 characters and CRLF.
    for (const byte of bytes) stream.enqueue(Uint8Array.of(byte));
    await delivered.promise;
    expect(frames).toEqual(["Привет 🦀"]);
    source.close();
    assert.ok(fetchRequest.mock.calls[0] != null);
    expect(fetchRequest.mock.calls[0][0].signal.aborted).toBe(true);
    expect(() =>
      stream.enqueue(
        new TextEncoder().encode("event: resources\ndata: ignored\n\n"),
      ),
    ).toThrow("closed");
    await cancelled.promise;
    expect(frames).toEqual(["Привет 🦀"]);
    expect(errors).not.toHaveBeenCalled();
  },
);
it("cancels a rejected event response", async () => {
  vi.stubGlobal("location", { origin: "http://studio.test", search: "" });
  const { SignedEventSource } = await import("./eventSource");
  const cancel = vi.fn();
  const source = new SignedEventSource(
    "/api/sync/stream",
    async () =>
      new Response(new ReadableStream({ cancel }), {
        headers: { "Content-Type": "text/html" },
      }),
  );
  const rejected = Promise.withResolvers<void>();
  const error = vi.fn(() => rejected.resolve());
  source.onerror = error;
  await rejected.promise;
  expect(error).toHaveBeenCalledOnce();
  expect(cancel).toHaveBeenCalledOnce();
  source.close();
});
it("backpressures reads while staging and close interrupts an unfinished hold", async () => {
  vi.stubGlobal("location", { origin: "http://studio.test", search: "" });
  const { SignedEventSource } = await import("./eventSource");
  let stream!: ReadableStreamDefaultController<Uint8Array>;
  const cancelled = Promise.withResolvers<void>();
  const received = Promise.withResolvers<void>();
  const source = new SignedEventSource(
    "/api/sync/stream",
    async () =>
      new Response(
        new ReadableStream<Uint8Array>({
          start(controller) {
            stream = controller;
          },
          cancel() {
            cancelled.resolve();
          },
        }),
        { headers: { "Content-Type": "text/event-stream" } },
      ),
  );
  const frames: string[] = [];
  source.addEventListener("entity-transfer", (event) => {
    frames.push((event as MessageEvent).data);
    source.holdUntil(new Promise(() => {}));
    received.resolve();
  });
  stream.enqueue(
    new TextEncoder().encode("event: entity-transfer\ndata: first\n\n"),
  );
  await received.promise;
  stream.enqueue(
    new TextEncoder().encode("event: entity-transfer\ndata: second\n\n"),
  );
  await Promise.resolve();
  expect(frames).toEqual(["first"]);
  source.close();
  await cancelled.promise;
  expect(frames).toEqual(["first"]);
});
it("settled staging holds retain no lifetime result chain", async () => {
  vi.stubGlobal("location", { origin: "http://studio.test", search: "" });
  const { SignedEventSource } = await import("./eventSource");
  const source = new SignedEventSource(
    "/api/sync/stream",
    async () =>
      new Response(
        new ReadableStream({
          start(controller) {
            controller.close();
          },
        }),
        { headers: { "Content-Type": "text/event-stream" } },
      ),
  );
  try {
    for (let count = 0; count < 100; count++) {
      source.holdUntil(Promise.resolve());
      await (source as unknown as { hold: Promise<unknown> }).hold;
    }
    let result: unknown = await (
      source as unknown as { hold: Promise<unknown> }
    ).hold;
    let retainedLevels = 0;
    while (Array.isArray(result)) {
      retainedLevels++;
      result = result[0];
    }
    expect(retainedLevels).toBe(0);
  } finally {
    source.close();
  }
});
