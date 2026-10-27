import { randomUUID } from "../uuid";
import { listenNativeEvents, nativeRequest } from "./client";
import type {
  NativeStreamRetry,
  ServerEventSource,
} from "../servers/eventSource";

type NativeStreamMessage = {
  event?: string;
  credit?: unknown;
  consumedCredit?: unknown;
  error?: { message?: string };
  value?: {
    type?: string;
    data?: string;
    message?: string;
    closed?: boolean;
    opened?: boolean;
    retryInMs?: number;
  };
};

/**
 * One server event stream that the native core owns end to end: connect,
 * credits, reconnect with backoff, the silence watchdog and close reasons
 * (`client-core/src/events.rs`). The page never retries. It only answers a
 * resume request with its current path and asks for a restart when it cannot
 * use a connection. `onerror` means the core stopped the stream for good.
 */
export class NativeEventSource
  extends EventTarget
  implements ServerEventSource
{
  onopen: ServerEventSource["onopen"] = null;
  onerror: ServerEventSource["onerror"] = null;
  onretry: ((detail: NativeStreamRetry) => void) | null = null;
  onresume: (() => void) | null = null;
  private readonly subscription = randomUUID();
  private stopped = false;
  private confirmed = false;
  /** The page ended or lost this connection; its late events are dropped. */
  private lost = false;
  private hold: Promise<unknown> = Promise.resolve();
  /** The latest credit from the core, and the one the page last granted. */
  private credit: string | undefined;
  private granted: string | undefined;
  private unlisten: (() => void) | undefined;

  constructor(path: string) {
    super();
    void this.start(path).catch((error: unknown) => this.fail(error));
  }

  holdUntil(ready: Promise<unknown>) {
    this.hold = Promise.all([this.hold, ready]);
  }

  resume(path: string) {
    this.command("view.resume", { path });
  }

  restart(reason: string) {
    this.lost = true;
    this.command("view.restart", { reason: reason.slice(0, 300) });
  }

  /** The reason reaches the desktop stream log. */
  close(reason = "closed by the page") {
    if (this.stopped) return;
    this.stopped = true;
    this.unlisten?.();
    if (this.confirmed)
      void nativeRequest("view.unsubscribe", {
        subscription: this.subscription,
        reason: reason.slice(0, 300),
      }).catch(() => {});
  }

  private async start(path: string) {
    const unlisten = await listenNativeEvents((input) => this.receive(input));
    if (this.stopped) return unlisten();
    this.unlisten = unlisten;
    await nativeRequest("view.subscribe", {
      subscription: this.subscription,
      topic: "sse",
      path,
      flowControl: true,
    });
    this.confirmed = true;
    // A close can race backend verification and subscription creation.
    if (this.stopped)
      void nativeRequest("view.unsubscribe", {
        subscription: this.subscription,
      }).catch(() => {});
  }

  private command(method: string, params: Record<string, unknown>) {
    if (this.stopped) return;
    void nativeRequest(method, {
      subscription: this.subscription,
      ...params,
    }).catch((error: unknown) => this.fail(error));
  }

  private fail(error: unknown) {
    if (this.stopped) return;
    this.close(String((error as Error)?.message ?? error));
    this.onerror?.call(this, new Event("error"));
  }

  private receive(input: unknown) {
    const message = input as NativeStreamMessage | null;
    if (this.stopped || message?.event !== this.subscription) return;
    const value = message.value;
    if (
      message.error ||
      (value?.type === "error" && value.data === undefined)
    ) {
      if (message.error || value?.closed !== false)
        return this.fail(
          new Error(
            message.error?.message ||
              value?.message ||
              "The server event stream closed.",
          ),
        );
      this.lost = true;
      this.onretry?.({
        opened: value.opened === true,
        retryAt: Date.now() + (value.retryInMs ?? 0),
      });
    } else if (value?.type === "resume" && value.data === undefined) {
      this.onresume?.();
    } else if (value?.type === "open" && value.data === undefined) {
      if (typeof message.credit !== "string" || !message.credit)
        return this.fail(new Error("The native event credit is invalid."));
      this.lost = false;
      this.credit = message.credit;
      this.onopen?.call(this, new Event("open"));
      this.grant();
    } else if (typeof value?.data === "string") {
      if (
        this.granted === undefined ||
        message.consumedCredit !== this.granted ||
        typeof message.credit !== "string" ||
        !message.credit ||
        message.credit === this.granted
      )
        return this.fail(new Error("The native event credit is invalid."));
      this.credit = message.credit;
      if (this.lost) return;
      this.dispatchEvent(
        new MessageEvent(value.type || "message", { data: value.data }),
      );
      this.grant();
    }
  }

  /** Grants the current credit once the page has applied earlier events. */
  private grant() {
    const credit = this.credit;
    if (!credit || credit === this.granted) return;
    void this.hold.then(
      () => {
        if (this.stopped || this.lost || credit !== this.credit) return;
        if (credit === this.granted) return;
        this.granted = credit;
        // A lost grant response has an unknown outcome; it is never repeated.
        this.command("view.next", { credit });
      },
      (error: unknown) => this.fail(error),
    );
  }
}
