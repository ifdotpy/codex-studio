import { expect, it, vi, afterEach } from "vitest";
import { EventStreamParser } from "./eventSource";
afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
});
it("parses signed stream frames across UTF-8, CRLF and blank-line chunk boundaries", () => {
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
it("uses signed fetch for remote events and cancels the stream on close", async () => {
  vi.stubGlobal("location", { origin: "http://studio.test", search: "" });
  const { SignedEventSource } = await import("./eventSource");
  let stream!: ReadableStreamDefaultController<Uint8Array>;
  const fetchRequest = vi.fn(async (request: Request) => {
    expect(request.headers.get("Accept")).toBe("text/event-stream");
    expect(request.redirect).toBe("error");
    return new Response(
      new ReadableStream<Uint8Array>({
        start(controller) {
          stream = controller;
        },
      }),
      { headers: { "Content-Type": "text/event-stream" } },
    );
  });
  const source = new SignedEventSource(
    "/api/sync/stream?protocol=3",
    fetchRequest,
  );
  const frames: string[] = [];
  source.addEventListener("resources", (event) =>
    frames.push((event as MessageEvent).data),
  );
  const errors = vi.fn();
  source.onerror = errors;
  await vi.waitFor(() => expect(fetchRequest).toHaveBeenCalledOnce());
  stream.enqueue(new TextEncoder().encode("event: resources\ndata: {}\n\n"));
  await vi.waitFor(() => expect(frames).toEqual(["{}"]));
  source.close();
  expect(fetchRequest.mock.calls[0][0].signal.aborted).toBe(true);
  stream.enqueue(
    new TextEncoder().encode("event: resources\ndata: ignored\n\n"),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(frames).toEqual(["{}"]);
  expect(errors).not.toHaveBeenCalled();
});
