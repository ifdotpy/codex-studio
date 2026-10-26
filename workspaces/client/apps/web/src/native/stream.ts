import { randomUUID } from "../uuid";
import { listenNativeEvents, nativeRequest } from "./client";

// Maximum bytes in one encoded native SSE event, independent of read demand.
export const NATIVE_EVENT_MAX_BYTES = 4 * 1024 * 1024;

// The core owns HTTP and SSE framing. This adapter carries its decoded events
// through the existing per-server stream bridge without exposing credentials.
export async function nativeEventResponse(
  request: Request,
  path: string,
): Promise<Response> {
  request.signal.throwIfAborted();
  const subscription = randomUUID();
  const encoder = new TextEncoder();
  let controller: ReadableStreamDefaultController<Uint8Array>;
  let stopped = false;
  let confirmed = false;
  let unlisten: (() => void) | undefined;
  let opened = false;
  let credit: string | undefined;
  let pending:
    | { credit: string; resolve: () => void; reject: (error: unknown) => void }
    | undefined;
  let resolveOpen: () => void;
  let rejectOpen: (error: unknown) => void;
  const ready = new Promise<void>((resolve, reject) => {
    resolveOpen = resolve;
    rejectOpen = reject;
  });
  // A rejection can precede listener registration completing.
  void ready.catch(() => {});
  let reason = "closed by the page";
  const stop = () => {
    if (stopped) return;
    stopped = true;
    pending?.resolve();
    pending = undefined;
    credit = undefined;
    request.signal.removeEventListener("abort", abort);
    unlisten?.();
    if (confirmed)
      void nativeRequest("view.unsubscribe", { subscription, reason }).catch(
        () => {},
      );
  };
  const fail = (error: unknown) => {
    if (stopped) return;
    reason = String((error as Error)?.message ?? error).slice(0, 300);
    rejectOpen(error);
    pending?.reject(error);
    pending = undefined;
    controller.error(error);
    stop();
  };
  const abort = () => fail(request.signal.reason);
  const body = new ReadableStream<Uint8Array>(
    {
      start(value) {
        controller = value;
      },
      pull() {
        if (stopped) return;
        if (!credit || pending) {
          fail(new Error("The native event credit is invalid."));
          return;
        }
        const token = credit;
        return new Promise<void>((resolve, reject) => {
          pending = { credit: token, resolve, reject };
          void nativeRequest("view.next", { subscription, credit: token }).then(
            (result) => {
              if (!result || (result as { granted?: boolean }).granted !== true)
                fail(new Error("The native event credit was not accepted."));
            },
            fail,
          );
        });
      },
      cancel(cause) {
        reason = `reader cancelled: ${String(cause?.message ?? cause)}`.slice(
          0,
          300,
        );
        stop();
      },
    },
    { highWaterMark: 0, size: (chunk) => chunk.byteLength },
  );
  try {
    unlisten = await listenNativeEvents((input) => {
      if (stopped || !input || typeof input !== "object") return;
      const event = input as {
        event?: string;
        credit?: unknown;
        consumedCredit?: unknown;
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
        if (opened || typeof event.credit !== "string" || !event.credit) {
          fail(new Error("The native event credit is invalid."));
          return;
        }
        credit = event.credit;
        opened = true;
        resolveOpen();
      } else if (typeof event.value?.data === "string") {
        if (!opened) {
          fail(new Error("The native event stream is not open."));
          return;
        }
        if (
          !pending ||
          event.consumedCredit !== pending.credit ||
          typeof event.credit !== "string" ||
          !event.credit ||
          event.credit === pending.credit
        ) {
          fail(new Error("The native event credit is invalid."));
          return;
        }
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
        if (bytes.byteLength > NATIVE_EVENT_MAX_BYTES) {
          fail(new Error("The native event exceeds its size limit."));
          return;
        }
        const delivered = pending;
        pending = undefined;
        credit = event.credit;
        controller.enqueue(bytes);
        delivered.resolve();
      }
    });
    request.signal.addEventListener("abort", abort, { once: true });
    request.signal.throwIfAborted();
    void nativeRequest("view.subscribe", {
      subscription,
      topic: "sse",
      path,
      flowControl: true,
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
