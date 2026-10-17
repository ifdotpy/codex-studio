import { listenNativeEvents, nativeRequest } from "./client";

// The core owns HTTP and SSE framing. This adapter carries its decoded events
// through the existing per-server stream bridge without exposing credentials.
export async function nativeEventResponse(
  request: Request,
  path: string,
): Promise<Response> {
  request.signal.throwIfAborted();
  const subscription = crypto.randomUUID();
  const encoder = new TextEncoder();
  let controller: ReadableStreamDefaultController<Uint8Array>;
  let stopped = false;
  let confirmed = false;
  let unlisten: (() => void) | undefined;
  let opened = false;
  let resolveOpen: () => void;
  let rejectOpen: (error: unknown) => void;
  const ready = new Promise<void>((resolve, reject) => {
    resolveOpen = resolve;
    rejectOpen = reject;
  });
  // A rejection can precede listener registration completing.
  void ready.catch(() => {});
  const stop = () => {
    if (stopped) return;
    stopped = true;
    request.signal.removeEventListener("abort", abort);
    unlisten?.();
    if (confirmed)
      void nativeRequest("view.unsubscribe", { subscription }).catch(() => {});
  };
  const fail = (error: unknown) => {
    if (stopped) return;
    rejectOpen(error);
    controller.error(error);
    stop();
  };
  const abort = () => fail(request.signal.reason);
  const body = new ReadableStream<Uint8Array>(
    {
      start(value) {
        controller = value;
      },
      cancel() {
        stop();
      },
    },
    { highWaterMark: 4 * 1024 * 1024, size: (chunk) => chunk.byteLength },
  );
  try {
    unlisten = await listenNativeEvents((input) => {
      if (stopped || !input || typeof input !== "object") return;
      const event = input as {
        event?: string;
        error?: { message?: string };
        value?: { type?: string; data?: string; id?: string; message?: string };
      };
      if (event.event !== subscription) return;
      if (
        event.error ||
        (event.value?.type === "error" && event.value.data === undefined)
      ) {
        fail(
          new Error(
            event.error?.message ||
              event.value?.message ||
              "The server event stream closed.",
          ),
        );
      } else if (
        event.value?.type === "open" &&
        event.value.data === undefined
      ) {
        opened = true;
        resolveOpen();
      } else if (opened && typeof event.value?.data === "string") {
        const value = event.value;
        const type = value.type || "message";
        if (/[\r\n]/.test(type) || /[\r\n\0]/.test(value.id || "")) {
          fail(new Error("The native event metadata is invalid."));
          return;
        }
        const bytes = encoder.encode(
          `id: ${value.id || ""}\nevent: ${type}\n${value
            .data!.split("\n")
            .map((line) => `data: ${line}\n`)
            .join("")}\n`,
        );
        if (bytes.byteLength > (controller.desiredSize ?? 0)) {
          fail(new Error("The server event consumer fell behind."));
          return;
        }
        controller.enqueue(bytes);
      }
    });
    request.signal.addEventListener("abort", abort, { once: true });
    request.signal.throwIfAborted();
    void nativeRequest("view.subscribe", {
      subscription,
      topic: "sse",
      path,
    }).then(() => {
      confirmed = true;
      // A close can race backend verification and subscription creation.
      if (stopped)
        void nativeRequest("view.unsubscribe", { subscription }).catch(
          () => {},
        );
    }, fail);
    await ready;
    return new Response(body, {
      headers: { "Content-Type": "text/event-stream" },
    });
  } catch (error) {
    stop();
    throw error;
  }
}
