import type { ResourceChangeEvent } from "../playwright.ts";

// The removed persister probe is kept as a test-owned observation surface.
export type PersisterCompletion = {
  at: number;
  sequences: number[];
  checkpoint: number;
  advanced: boolean;
};
declare global {
  interface Window {
    __entityFrames: Array<{
      at: number;
      streamIndex: number;
      event: ResourceChangeEvent;
    }>;
    __entityStreamUrls: string[];
    __entityStreamAttempts: Array<{
      url: string;
      opened: boolean;
      errored: boolean;
    }>;
    __entityPersisterCompletions: PersisterCompletion[];
    __entityMutationDecodes: Array<{ at: number; sequences: number[] }>;
    __studioSyncEntityPersisterProbe: (value: PersisterCompletion) => number;
  }
}

export function installEntityPropagationProbe() {
  const OriginalEventSource = window.EventSource;
  window.__entityFrames = [];
  window.__entityStreamUrls = [];
  window.__entityStreamAttempts = [];
  window.__entityPersisterCompletions = [];
  window.__entityMutationDecodes = [];
  const responseJson = Response.prototype.json;
  Response.prototype.json = async function (...args) {
    const value: unknown = await responseJson.apply(this, args);
    const rows =
      value &&
      typeof value === "object" &&
      "_syncEntities" in value &&
      Array.isArray(value._syncEntities)
        ? value._syncEntities
        : [];
    if (this.url && new URL(this.url).pathname === "/api/leads" && rows.length)
      window.__entityMutationDecodes.push({
        at: performance.timeOrigin + performance.now(),
        sequences: rows.map((row: unknown) => {
          if (
            !row ||
            typeof row !== "object" ||
            !("seq" in row) ||
            typeof row.seq !== "number"
          )
            throw new Error("Legacy mutation row has no sequence");
          return row.seq;
        }),
      });
    return value;
  };
  window.__studioSyncEntityPersisterProbe = (value) =>
    window.__entityPersisterCompletions.push(value);
  window.EventSource = class extends OriginalEventSource {
    constructor(...args: ConstructorParameters<typeof EventSource>) {
      super(...args);
      const streamIndex = window.__entityStreamUrls.length;
      const url = String(args[0]);
      const attempt = { url, opened: false, errored: false };
      window.__entityStreamUrls.push(url);
      window.__entityStreamAttempts.push(attempt);
      this.addEventListener("open", () => (attempt.opened = true));
      this.addEventListener("error", () => (attempt.errored = true));
      this.addEventListener("resources", (event) =>
        window.__entityFrames.push({
          at: performance.timeOrigin + performance.now(),
          streamIndex,
          event: JSON.parse(event.data),
        }),
      );
    }
  };
}
