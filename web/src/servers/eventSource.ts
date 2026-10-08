import { apiOrigin, serverFetch } from "./transport";
import { serverViewId, isolatedServerView } from "./environment";

export type ServerEventSource = Pick<
  EventSource,
  "close" | "onopen" | "onerror"
> &
  Pick<EventTarget, "addEventListener">;
export class EventStreamParser {
  private buffer = "";
  private data: string[] = [];
  private type = "message";
  private first = true;
  constructor(private emit: (type: string, data: string) => void) {}
  push(value: string) {
    if (this.first && value.length) {
      this.first = false;
      value = value.replace(/^\uFEFF/, "");
    }
    this.buffer += value;
    let end: number;
    while ((end = this.buffer.search(/[\r\n]/)) !== -1) {
      if (this.buffer[end] === "\r" && end === this.buffer.length - 1) break;
      const line = this.buffer.slice(0, end);
      this.buffer = this.buffer.slice(
        end + (this.buffer.slice(end, end + 2) === "\r\n" ? 2 : 1),
      );
      if (!line) {
        if (this.data.length) this.emit(this.type, this.data.join("\n"));
        this.data = [];
        this.type = "message";
      } else if (!line.startsWith(":")) {
        const colon = line.indexOf(":");
        const field = colon === -1 ? line : line.slice(0, colon);
        const content =
          colon === -1 ? "" : line.slice(colon + 1).replace(/^ /, "");
        if (field === "data") this.data.push(content);
        if (field === "event") this.type = content || "message";
      }
      if (
        this.buffer.length > 4 * 1024 * 1024 ||
        this.data.join("\n").length > 4 * 1024 * 1024
      )
        throw new Error("The server event exceeds the size limit.");
    }
    if (this.buffer.length > 4 * 1024 * 1024)
      throw new Error("The server event exceeds the size limit.");
  }
}
export class SignedEventSource
  extends EventTarget
  implements ServerEventSource
{
  onopen: EventSource["onopen"] = null;
  onerror: EventSource["onerror"] = null;
  private controller = new AbortController();
  private stopped = false;
  constructor(url: string, fetchRequest = serverFetch) {
    super();
    void (async () => {
      const response = await fetchRequest(
        new Request(new URL(url, apiOrigin()), {
          headers: { Accept: "text/event-stream" },
          signal: this.controller.signal,
          cache: "no-store",
          redirect: "error",
        }),
      );
      if (
        !response.ok ||
        !response.body ||
        !response.headers.get("Content-Type")?.startsWith("text/event-stream")
      )
        throw new Error("The server event stream is unavailable.");
      if (this.stopped) {
        await response.body.cancel();
        return;
      }
      this.onopen?.call(this as unknown as EventSource, new Event("open"));
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      const parser = new EventStreamParser((type, data) => {
        if (!this.stopped) this.dispatchEvent(new MessageEvent(type, { data }));
      });
      try {
        while (!this.stopped) {
          const chunk = await reader.read();
          if (chunk.done) break;
          parser.push(decoder.decode(chunk.value, { stream: true }));
        }
      } finally {
        await reader.cancel().catch(() => {});
        reader.releaseLock();
      }
      if (!this.stopped) throw new Error("The server event stream closed.");
    })().catch(() => {
      if (!this.stopped)
        this.onerror?.call(this as unknown as EventSource, new Event("error"));
    });
  }
  close() {
    this.stopped = true;
    this.controller.abort();
  }
}
export function openServerEvents(url: string): ServerEventSource {
  return serverViewId && (serverViewId !== "local" || isolatedServerView)
    ? new SignedEventSource(url)
    : new EventSource(url);
}
