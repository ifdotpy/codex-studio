import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";
import {
  mergePreferenceFields,
  type PreferenceFields,
} from "./uiPreferenceMerge";
import {
  capturePreferenceWrite,
  receivePreferenceFields,
} from "./uiPreferenceStore";
const streams = vi.hoisted(() => new Map<string, EventTarget>());
const failedStreamOrigins = vi.hoisted(() => new Set<string>());
vi.mock("../servers/eventSource", () => ({
  SignedEventSource: class extends EventTarget {
    onerror: (() => void) | null = null;
    origin: string;
    constructor(url: string) {
      super();
      this.origin = new URL(url).origin;
      if (failedStreamOrigins.has(this.origin))
        throw new TypeError("no stream");
      streams.set(this.origin, this);
    }
    close() {
      streams.delete(this.origin);
    }
  },
}));
import { connectUiPreferences } from "./uiPreferenceConnection";
import { legacyPreferenceSeed } from "../servers/legacyPreferenceSeed";
import type { StudioServer } from "../servers/registry";
class FakeStorageEvent extends Event {
  key: string | null;
  newValue: string | null;
  constructor(type: string, options: { key?: string; newValue?: string } = {}) {
    super(type);
    this.key = options.key ?? null;
    this.newValue = options.newValue ?? null;
  }
}
function storage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    key: (i) => [...values.keys()][i] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      values.set(key, value);
    },
    removeItem: (key) => {
      values.delete(key);
    },
    clear: () => values.clear(),
  };
}
function backend(origin: string, initial: PreferenceFields) {
  let fields = initial;
  let sequence = 1;
  let online = true;
  let loseResponse = false;
  const writes: { fields: PreferenceFields; requestId: string | null }[] = [];
  let sessionReads = 0;
  let pullReads = 0;
  const seedBaseline = () => {
    const key = JSON.stringify([`server-label:test:${new URL(origin).host}`]);
    const value = `baseline:${origin}`;
    fields = mergePreferenceFields(fields, {
      [key]: { value, timestamp: 0, writer: "fixture" },
    });
    return { key, value };
  };
  const fetch = async (request: Request) => {
    if (!online) throw new TypeError("offline");
    const url = new URL(request.url);
    let value: unknown;
    if (url.pathname === "/api/session") {
      sessionReads++;
      value = { token: "fixture" };
    } else if (url.pathname === "/api/sync/preferences") {
      expect(request.headers.get("X-Canvas-Token")).toBe("fixture");
      const body = await request.json();
      fields = mergePreferenceFields(fields, body.fields);
      writes.push({
        fields: body.fields,
        requestId: request.headers.get("X-Studio-Request-Id"),
      });
      sequence++;
      if (loseResponse) {
        loseResponse = false;
        throw new TypeError("lost success");
      }
      value = { fields };
    } else {
      pullReads++;
      value = {
        workspaceId: origin,
        documents:
          Number(url.searchParams.get("after")) < sequence
            ? [
                {
                  id: "entity:uiPreferences:user",
                  payload: JSON.stringify({
                    collection: "uiPreferences",
                    id: "user",
                    value: { fields },
                  }),
                  seq: sequence,
                  _deleted: false,
                },
              ]
            : [],
        checkpoint: { seq: sequence },
        maxSeq: sequence,
      };
    }
    return new Response(JSON.stringify(value), {
      headers: {
        [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH,
        "Content-Type": "application/json",
      },
    });
  };
  return {
    fetch,
    writes,
    get fields() {
      return fields;
    },
    get sessionReads() {
      return sessionReads;
    },
    get pullReads() {
      return pullReads;
    },
    seedBaseline,
    get sequence() {
      return sequence;
    },
    set sequence(value: number) {
      sequence = value;
    },
    set online(value: boolean) {
      online = value;
    },
    set loseResponse(value: boolean) {
      loseResponse = value;
    },
  };
}
function emitStateFrame(origin: string, frame: Record<string, unknown>) {
  const stream = streams.get(origin)!;
  const event = new Event("resources");
  Object.defineProperty(event, "data", { value: JSON.stringify(frame) });
  stream.dispatchEvent(event);
}
function initialFrame(origin: string, revision: number) {
  return {
    protocol: 3,
    workspaceId: origin,
    epoch: "epoch-one",
    revision,
    reason: "initial",
    resources: [{ kind: "state" }],
    resourceVersions: [{ revision, entityChanges: null }],
  };
}
async function waitForBaseline(
  origin: string,
  server: ReturnType<typeof backend>,
) {
  await vi.waitFor(() => expect(streams.has(origin)).toBe(true));
  const marker = server.seedBaseline();
  emitStateFrame(origin, initialFrame(origin, server.sequence));
  await waitForAppliedBaselineMarker(marker);
}
async function waitForAppliedBaselineMarker(marker: {
  key: string;
  value: string;
}) {
  await vi.waitFor(() => {
    const cache = JSON.parse(
      localStorage.getItem("studio-ui-preference-fields-v1:user") || "{}",
    );
    expect(cache[marker.key]?.value).toBe(marker.value);
  });
}
beforeEach(() => {
  streams.clear();
  failedStreamOrigins.clear();
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal("localStorage", storage());
  vi.stubGlobal("StorageEvent", FakeStorageEvent);
});
describe("paired server preference replication", () => {
  it("does not read a session or post defaults on a fresh local profile", async () => {
    const server = backend("https://defaults.invalid", {});
    const seed = legacyPreferenceSeed(
      [
        {
          id: "local",
          origin: "http://localhost",
          workspaceId: "local-workspace",
          label: "This computer",
          alias: "LOC",
        } satisfies StudioServer,
      ],
      new Map([["local", "local-workspace"]]),
      {},
      null,
    );
    receivePreferenceFields("user", seed);
    const stop = connectUiPreferences(
      "https://defaults.invalid",
      server.fetch,
      localStorage,
    );
    try {
      await waitForBaseline("https://defaults.invalid", server);
      expect(seed).toEqual({});
      expect(server.sessionReads).toBe(0);
      expect(server.writes).toHaveLength(0);
    } finally {
      stop();
    }
  });

  it("merges both servers and catches up an offline server without new edit versions", async () => {
    const theme = '["codex-studio-preferences-v1","theme"]';
    const size = '["codex-studio-preferences-v1","mainFontSize"]';
    const a = backend("https://a.invalid", {
      [theme]: { value: "dark", timestamp: 5, writer: "a" },
    });
    const b = backend("https://b.invalid", {
      [size]: { value: 18, timestamp: 6, writer: "b" },
    });
    const stops = [
      connectUiPreferences("https://a.invalid", a.fetch, localStorage),
      connectUiPreferences("https://b.invalid", b.fetch, storage()),
    ];
    try {
      await Promise.all([
        waitForBaseline("https://a.invalid", a),
        waitForBaseline("https://b.invalid", b),
      ]);
      await vi.waitFor(() => {
        expect(a.fields[size]?.value).toBe(18);
        expect(b.fields[theme]?.value).toBe("dark");
      });
      b.online = false;
      window.dispatchEvent(new Event("offline"));
      capturePreferenceWrite(
        "codex-studio-preferences-v1",
        '{"mainFontSize":20}',
        '{"mainFontSize":18}',
      );
      await vi.waitFor(() => expect(a.fields[size]?.value).toBe(20));
      b.online = true;
      window.dispatchEvent(new Event("online"));
      emitStateFrame(
        "https://b.invalid",
        initialFrame("https://b.invalid", b.sequence),
      );
      await vi.waitFor(() => expect(b.fields[size]).toEqual(a.fields[size]));
      expect(b.fields[theme].value).toBe("dark");
      expect(
        a.writes.every((write) =>
          write.requestId?.startsWith("ui-preferences:"),
        ),
      ).toBe(true);
    } finally {
      stops.forEach((stop) => stop());
    }
  });
  it("recovers a lost success through entity pull without changing the field version", async () => {
    const theme = '["codex-studio-preferences-v1","theme"]';
    const server = backend("https://lost.invalid", {});
    const stop = connectUiPreferences(
      "https://lost.invalid",
      server.fetch,
      localStorage,
    );
    try {
      await waitForBaseline("https://lost.invalid", server);
      server.loseResponse = true;
      capturePreferenceWrite(
        "codex-studio-preferences-v1",
        '{"theme":"dark"}',
        '{"theme":"auto"}',
      );
      await vi.waitFor(() => expect(server.fields[theme]?.value).toBe("dark"));
      const version = server.fields[theme];
      window.dispatchEvent(new Event("online"));
      await vi.waitFor(() =>
        expect(
          JSON.parse(
            localStorage.getItem("studio-ui-preference-fields-v1:user")!,
          )[theme],
        ).toEqual(version),
      );
      expect(server.writes).toHaveLength(1);
      expect(server.writes[0].fields[theme]).toEqual(
        expect.objectContaining({ value: "dark" }),
      );
    } finally {
      stop();
    }
  });

  it("ignores unrelated state changes until a preference write is pending", async () => {
    const server = backend("https://idle.invalid", {});
    const stop = connectUiPreferences(
      "https://idle.invalid",
      server.fetch,
      localStorage,
    );
    try {
      await waitForBaseline("https://idle.invalid", server);
      for (let revision = 2; revision < 14; revision++)
        emitStateFrame("https://idle.invalid", {
          protocol: 3,
          workspaceId: "https://idle.invalid",
          epoch: "epoch-one",
          revision,
          reason: "change",
          resources: [{ kind: "state" }],
          resourceVersions: [{ revision, entityChanges: null }],
        });
      await vi.waitFor(() => expect(server.pullReads).toBeGreaterThan(1));
      expect(server.sessionReads).toBe(0);
      expect(server.writes).toHaveLength(0);

      capturePreferenceWrite(
        "codex-studio-preferences-v1",
        '{"theme":"dark"}',
        '{"theme":"auto"}',
      );
      await vi.waitFor(() => expect(server.writes).toHaveLength(1));
      expect(server.sessionReads).toBe(1);
      expect(server.writes).toHaveLength(1);
    } finally {
      stop();
    }
  });

  it("skips pulls for complete unrelated state changes", async () => {
    const server = backend("https://unrelated.invalid", {});
    const stop = connectUiPreferences(
      "https://unrelated.invalid",
      server.fetch,
      localStorage,
    );
    try {
      await waitForBaseline("https://unrelated.invalid", server);
      for (let revision = 2; revision < 14; revision++)
        emitStateFrame("https://unrelated.invalid", {
          protocol: 3,
          workspaceId: "https://unrelated.invalid",
          epoch: "epoch-one",
          revision,
          reason: "change",
          resources: [{ kind: "state" }],
          resourceVersions: [
            {
              revision,
              entityChanges: {
                after: revision - 1,
                through: revision,
                documents: [
                  {
                    id: `entity:project:project-${revision}`,
                    seq: revision,
                    payload: JSON.stringify({
                      collection: "project",
                      id: `project-${revision}`,
                      value: {},
                    }),
                    _deleted: false,
                  },
                ],
              },
            },
          ],
        });
      expect(server.pullReads).toBe(1);
      expect(server.sessionReads).toBe(0);
      expect(server.writes).toHaveLength(0);
    } finally {
      stop();
    }
  });

  it("applies complete preference rows from a state frame without a pull", async () => {
    const server = backend("https://frame.invalid", {});
    const stop = connectUiPreferences(
      "https://frame.invalid",
      server.fetch,
      localStorage,
    );
    try {
      await waitForBaseline("https://frame.invalid", server);
      const theme = '["codex-studio-preferences-v1","theme"]';
      expect(server.fields[theme]).toBeUndefined();
      emitStateFrame("https://frame.invalid", {
        protocol: 3,
        workspaceId: "https://frame.invalid",
        epoch: "epoch-one",
        revision: 2,
        reason: "change",
        resources: [{ kind: "state" }],
        resourceVersions: [
          {
            revision: 2,
            entityChanges: {
              after: 1,
              through: 2,
              documents: [
                {
                  id: "entity:uiPreferences:user",
                  seq: 2,
                  payload: JSON.stringify({
                    collection: "uiPreferences",
                    id: "user",
                    value: {
                      fields: {
                        ...server.fields,
                        [theme]: {
                          value: "dark",
                          timestamp: 2,
                          writer: "remote",
                        },
                      },
                    },
                  }),
                  _deleted: false,
                },
              ],
            },
          },
        ],
      });
      await vi.waitFor(() => {
        expect(
          JSON.parse(
            localStorage.getItem("studio-ui-preference-fields-v1:user")!,
          )[theme]?.value,
        ).toBe("dark");
      });
      expect(server.pullReads).toBe(1);
      expect(server.sessionReads).toBe(0);
      expect(server.writes).toHaveLength(0);
    } finally {
      stop();
    }
  });

  it("pulls once when a state frame omits its bounded entity changes", async () => {
    const server = backend("https://omitted.invalid", {});
    const stop = connectUiPreferences(
      "https://omitted.invalid",
      server.fetch,
      localStorage,
    );
    try {
      await waitForBaseline("https://omitted.invalid", server);
      emitStateFrame("https://omitted.invalid", {
        protocol: 3,
        workspaceId: "https://omitted.invalid",
        epoch: "epoch-one",
        revision: 2,
        reason: "change",
        resources: [{ kind: "state" }],
        resourceVersions: [{ revision: 2, entityChanges: null }],
      });
      await vi.waitFor(() => expect(server.pullReads).toBe(2));
      expect(server.sessionReads).toBe(0);
      expect(server.writes).toHaveLength(0);
    } finally {
      stop();
    }
  });

  it("waits for the initial baseline and pulls once when it is newer", async () => {
    const server = backend("https://initial.invalid", {});
    const stop = connectUiPreferences(
      "https://initial.invalid",
      server.fetch,
      localStorage,
    );
    try {
      await vi.waitFor(() =>
        expect(streams.has("https://initial.invalid")).toBe(true),
      );
      expect(server.pullReads).toBe(0);
      const marker = server.seedBaseline();
      server.sequence = 2;
      emitStateFrame("https://initial.invalid", {
        ...initialFrame("https://initial.invalid", 2),
      });
      await waitForAppliedBaselineMarker(marker);
      expect(server.pullReads).toBe(1);
      expect(server.sessionReads).toBe(0);
      expect(server.writes).toHaveLength(0);
    } finally {
      stop();
    }
  });

  it("skips a baseline already covered by the applied checkpoint", async () => {
    const server = backend("https://caught-up.invalid", {});
    const stop = connectUiPreferences(
      "https://caught-up.invalid",
      server.fetch,
      localStorage,
    );
    try {
      await waitForBaseline("https://caught-up.invalid", server);
      emitStateFrame(
        "https://caught-up.invalid",
        initialFrame("https://caught-up.invalid", server.sequence),
      );
      expect(server.pullReads).toBe(1);
    } finally {
      stop();
    }
  });

  it("skips a sequence-only state change already covered by the checkpoint", async () => {
    const origin = "https://covered-sequence-change.invalid";
    const server = backend(origin, {});
    const stop = connectUiPreferences(origin, server.fetch, localStorage);
    try {
      await waitForBaseline(origin, server);
      emitStateFrame(origin, {
        protocol: 3,
        workspaceId: origin,
        epoch: "epoch-one",
        reason: "change",
        resources: [{ kind: "state" }],
        resourceVersions: [
          { revision: server.sequence, entitySequences: [server.sequence] },
        ],
      });
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(server.pullReads).toBe(1);
    } finally {
      stop();
    }
  });

  it("does not repeat a pull for a baseline covered by the in-flight response", async () => {
    vi.useFakeTimers();
    const server = backend("https://in-flight-baseline.invalid", {});
    let releasePull!: () => void;
    const heldPull = new Promise<void>((resolve) => {
      releasePull = resolve;
    });
    let pullStarted!: () => void;
    const started = new Promise<void>((resolve) => {
      pullStarted = resolve;
    });
    let pullRequests = 0;
    const fetch = async (request: Request) => {
      if (new URL(request.url).pathname === "/api/sync/pull") {
        pullRequests++;
        if (pullRequests === 1) {
          pullStarted();
          await heldPull;
        }
      }
      return server.fetch(request);
    };
    const stop = connectUiPreferences(
      "https://in-flight-baseline.invalid",
      fetch,
      localStorage,
    );
    try {
      await vi.waitFor(() =>
        expect(streams.has("https://in-flight-baseline.invalid")).toBe(true),
      );
      const marker = server.seedBaseline();
      emitStateFrame(
        "https://in-flight-baseline.invalid",
        initialFrame("https://in-flight-baseline.invalid", server.sequence),
      );
      await started;
      // A reopened state stream can replay the same initial baseline while
      // reconciliation is still in flight.
      emitStateFrame(
        "https://in-flight-baseline.invalid",
        initialFrame("https://in-flight-baseline.invalid", server.sequence),
      );
      releasePull();
      await waitForAppliedBaselineMarker(marker);
      await vi.advanceTimersByTimeAsync(0);
      expect(server.pullReads).toBe(1);
      expect(pullRequests).toBe(1);
    } finally {
      releasePull();
      stop();
      vi.useRealTimers();
    }
  });

  it.each([
    { responseSequence: 2, expectedPulls: 1 },
    { responseSequence: 1, expectedPulls: 2 },
  ])(
    "reconciles an in-flight state change after the response checkpoint ($responseSequence)",
    async ({ responseSequence, expectedPulls }) => {
      const origin = "https://in-flight-change.invalid";
      const server = backend(origin, {});
      let releasePull!: () => void;
      const heldPull = new Promise<void>((resolve) => {
        releasePull = resolve;
      });
      let pullRequests = 0;
      let firstStarted!: () => void;
      const started = new Promise<void>((resolve) => {
        firstStarted = resolve;
      });
      const fetch = async (request: Request) => {
        if (new URL(request.url).pathname === "/api/sync/pull") {
          pullRequests++;
          if (pullRequests === 1) {
            firstStarted();
            await heldPull;
          }
        }
        return server.fetch(request);
      };
      const stop = connectUiPreferences(origin, fetch, localStorage);
      try {
        await vi.waitFor(() => expect(streams.has(origin)).toBe(true));
        const marker = server.seedBaseline();
        emitStateFrame(origin, initialFrame(origin, server.sequence));
        await started;

        emitStateFrame(origin, {
          protocol: 3,
          workspaceId: origin,
          epoch: "epoch-one",
          reason: "change",
          resources: [{ kind: "state" }],
          resourceVersions: [
            {
              revision: 2,
              entitySequences: [2],
              entityChanges: {
                after: 1,
                through: 2,
                documents: [],
              },
            },
          ],
        });
        // Choose whether the held response already sees the frame's sequence.
        server.sequence = responseSequence;
        releasePull();
        await waitForAppliedBaselineMarker(marker);
        await vi.waitFor(() => expect(pullRequests).toBe(expectedPulls));
        expect(server.pullReads).toBe(expectedPulls);
      } finally {
        releasePull();
        stop();
      }
    },
  );

  it.each([
    { responseSequence: 2, expectedPulls: 1 },
    { responseSequence: 1, expectedPulls: 2 },
  ])(
    "reconciles an in-flight sequence-only hint against checkpoint $responseSequence",
    async ({ responseSequence, expectedPulls }) => {
      const origin = "https://in-flight-sequence-only.invalid";
      const server = backend(origin, {});
      let releasePull!: () => void;
      const heldPull = new Promise<void>((resolve) => {
        releasePull = resolve;
      });
      let pullRequests = 0;
      let firstStarted!: () => void;
      const started = new Promise<void>((resolve) => {
        firstStarted = resolve;
      });
      const fetch = async (request: Request) => {
        if (new URL(request.url).pathname === "/api/sync/pull") {
          pullRequests++;
          if (pullRequests === 1) {
            firstStarted();
            await heldPull;
          }
        }
        return server.fetch(request);
      };
      const stop = connectUiPreferences(origin, fetch, localStorage);
      try {
        await vi.waitFor(() => expect(streams.has(origin)).toBe(true));
        const marker = server.seedBaseline();
        emitStateFrame(origin, initialFrame(origin, server.sequence));
        await started;

        emitStateFrame(origin, {
          protocol: 3,
          workspaceId: origin,
          epoch: "epoch-one",
          reason: "change",
          resources: [{ kind: "state" }],
          resourceVersions: [{ revision: 2, entitySequences: [2] }],
        });
        server.sequence = responseSequence;
        releasePull();
        await waitForAppliedBaselineMarker(marker);
        await vi.waitFor(() => expect(pullRequests).toBe(expectedPulls));
        expect(server.pullReads).toBe(expectedPulls);
      } finally {
        releasePull();
        stop();
      }
    },
  );

  it("pulls directly when the state stream cannot be opened", async () => {
    vi.useFakeTimers();
    failedStreamOrigins.add("https://no-stream.invalid");
    const server = backend("https://no-stream.invalid", {});
    const stop = connectUiPreferences(
      "https://no-stream.invalid",
      server.fetch,
      localStorage,
    );
    try {
      await vi.advanceTimersByTimeAsync(0);
      expect(server.pullReads).toBe(1);
      expect(streams.has("https://no-stream.invalid")).toBe(false);
    } finally {
      stop();
      vi.useRealTimers();
    }
  });

  it("does not pull on focus while the state stream is healthy", async () => {
    const origin = "https://focus-healthy.invalid";
    const server = backend(origin, {});
    const stop = connectUiPreferences(origin, server.fetch, localStorage);
    try {
      await waitForBaseline(origin, server);
      expect(server.pullReads).toBe(1);
      const stream = streams.get(origin);
      window.dispatchEvent(new Event("focus"));
      expect(streams.get(origin)).toBe(stream);
      expect(server.pullReads).toBe(1);
    } finally {
      stop();
    }
  });

  it("reopens and reconciles from a baseline after returning online", async () => {
    const origin = "https://focus-online.invalid";
    const server = backend(origin, {});
    const stop = connectUiPreferences(origin, server.fetch, localStorage);
    try {
      await waitForBaseline(origin, server);
      const previousStream = streams.get(origin);
      window.dispatchEvent(new Event("offline"));
      window.dispatchEvent(new Event("online"));
      const reopenedStream = streams.get(origin);
      expect(reopenedStream).toBeDefined();
      expect(reopenedStream).not.toBe(previousStream);
      emitStateFrame(origin, initialFrame(origin, server.sequence));
      expect(server.pullReads).toBe(1);
      server.sequence = 2;
      emitStateFrame(origin, initialFrame(origin, server.sequence));
      await vi.waitFor(() => expect(server.pullReads).toBe(2));
    } finally {
      stop();
    }
  });

  it("reopens a stream that is still connecting when focus returns", async () => {
    const origin = "https://focus-connecting.invalid";
    const server = backend(origin, {});
    const stop = connectUiPreferences(origin, server.fetch, localStorage);
    try {
      const connectingStream = streams.get(origin);
      expect(connectingStream).toBeDefined();
      window.dispatchEvent(new Event("focus"));
      expect(streams.get(origin)).toBeDefined();
      expect(streams.get(origin)).not.toBe(connectingStream);
      emitStateFrame(origin, initialFrame(origin, server.sequence));
      await vi.waitFor(() => expect(server.pullReads).toBe(1));
    } finally {
      stop();
    }
  });

  it("reconciles a dropped stream from its reopened baseline", async () => {
    const origin = "https://reconnect.invalid";
    const server = backend(origin, {});
    const stop = connectUiPreferences(origin, server.fetch, localStorage);
    try {
      await waitForBaseline(origin, server);
      expect(server.pullReads).toBe(1);
      vi.useFakeTimers();
      const stream = streams.get(origin) as
        | (EventTarget & { onerror?: (() => void) | null })
        | undefined;
      expect(stream?.onerror).toBeTypeOf("function");
      stream?.onerror?.();
      expect(server.pullReads).toBe(1);
      await vi.advanceTimersByTimeAsync(3000);
      expect(streams.has(origin)).toBe(true);
      emitStateFrame(origin, initialFrame(origin, server.sequence));
      expect(server.pullReads).toBe(1);
      server.sequence = 2;
      emitStateFrame(origin, initialFrame(origin, server.sequence));
      await vi.waitFor(() => expect(server.pullReads).toBe(2));
    } finally {
      stop();
      vi.useRealTimers();
    }
  });

  it("pulls when a failed stream cannot be reopened", async () => {
    const origin = "https://reopen-fails.invalid";
    const server = backend(origin, {});
    const stop = connectUiPreferences(origin, server.fetch, localStorage);
    try {
      await waitForBaseline(origin, server);
      vi.useFakeTimers();
      failedStreamOrigins.add(origin);
      const stream = streams.get(origin) as
        | (EventTarget & { onerror?: (() => void) | null })
        | undefined;
      stream?.onerror?.();
      expect(server.pullReads).toBe(1);
      await vi.advanceTimersByTimeAsync(3000);
      expect(server.pullReads).toBe(2);
      expect(streams.has(origin)).toBe(false);
    } finally {
      stop();
      failedStreamOrigins.delete(origin);
      vi.useRealTimers();
    }
  });

  it("ignores an error delivered after the connector closes its stream", async () => {
    const origin = "https://closed-stream.invalid";
    const server = backend(origin, {});
    const stop = connectUiPreferences(origin, server.fetch, localStorage);
    await waitForBaseline(origin, server);
    const stream = streams.get(origin) as
      | (EventTarget & { onerror?: (() => void) | null })
      | undefined;
    stop();
    stream?.onerror?.();
    expect(server.pullReads).toBe(1);
  });

  it("does not request or retry after the API schema gate is raised", async () => {
    vi.useFakeTimers();
    const rawFetch = vi.fn(async () => Response.json({}));
    vi.stubGlobal("fetch", rawFetch);
    vi.stubGlobal("location", { origin: "https://gated.invalid" });
    const { fetchApiRequest, markApiSchemaMismatch } = await import("../api");
    markApiSchemaMismatch();
    const stop = connectUiPreferences(
      "https://gated.invalid",
      fetchApiRequest,
      storage(),
    );
    try {
      await vi.advanceTimersByTimeAsync(0);
      expect(rawFetch).not.toHaveBeenCalled();
      expect(vi.getTimerCount()).toBe(0);
      await vi.advanceTimersByTimeAsync(6000);
      expect(rawFetch).not.toHaveBeenCalled();
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      stop();
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("stops and does not retry when a server response has another schema", async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(
      async () =>
        new Response("{}", {
          headers: { [API_SCHEMA_HASH_HEADER]: "foreign-schema" },
        }),
    );
    const stop = connectUiPreferences(
      "https://mismatch.invalid",
      fetch,
      storage(),
    );
    try {
      await vi.waitFor(() =>
        expect(streams.has("https://mismatch.invalid")).toBe(true),
      );
      emitStateFrame(
        "https://mismatch.invalid",
        initialFrame("https://mismatch.invalid", 1),
      );
      await vi.advanceTimersByTimeAsync(0);
      expect(fetch).toHaveBeenCalledOnce();
      expect(vi.getTimerCount()).toBe(0);
      await vi.advanceTimersByTimeAsync(6000);
      expect(fetch).toHaveBeenCalledOnce();
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      stop();
      vi.useRealTimers();
    }
  });

  it("keeps a healthy server connected when another server mismatches", async () => {
    const healthy = backend("https://healthy.invalid", {});
    const mismatchedFetch = vi.fn(
      async () =>
        new Response("{}", {
          headers: { [API_SCHEMA_HASH_HEADER]: "foreign-schema" },
        }),
    );
    const stopHealthy = connectUiPreferences(
      "https://healthy.invalid",
      healthy.fetch,
      storage(),
    );
    const stopMismatched = connectUiPreferences(
      "https://mismatch.invalid",
      mismatchedFetch,
      storage(),
    );
    try {
      await vi.waitFor(() =>
        expect(streams.has("https://healthy.invalid")).toBe(true),
      );
      await vi.waitFor(() =>
        expect(streams.has("https://mismatch.invalid")).toBe(true),
      );
      emitStateFrame(
        "https://mismatch.invalid",
        initialFrame("https://mismatch.invalid", 1),
      );
      await vi.waitFor(() => expect(mismatchedFetch).toHaveBeenCalledOnce());
      expect(streams.has("https://mismatch.invalid")).toBe(false);
    } finally {
      stopHealthy();
      stopMismatched();
    }
  });
});
