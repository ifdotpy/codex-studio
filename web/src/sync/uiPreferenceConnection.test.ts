import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";
import {
  mergePreferenceFields,
  type PreferenceFields,
} from "./uiPreferenceMerge";
import { capturePreferenceWrite } from "./uiPreferenceStore";
const streams = vi.hoisted(() => new Map<string, EventTarget>());
vi.mock("../servers/eventSource", () => ({
  SignedEventSource: class extends EventTarget {
    onerror: (() => void) | null = null;
    constructor(url: string) {
      super();
      streams.set(new URL(url).origin, this);
    }
    close() {}
  },
}));
import { connectUiPreferences } from "./uiPreferenceConnection";
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
  const fetch = async (request: Request) => {
    if (!online) throw new TypeError("offline");
    const url = new URL(request.url);
    let value: unknown;
    if (url.pathname === "/api/session") value = { token: "fixture" };
    else if (url.pathname === "/api/sync/preferences") {
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
    } else
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
    set online(value: boolean) {
      online = value;
    },
    set loseResponse(value: boolean) {
      loseResponse = value;
    },
  };
}
beforeEach(() => {
  streams.clear();
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal("localStorage", storage());
  vi.stubGlobal("StorageEvent", FakeStorageEvent);
});
describe("paired server preference replication", () => {
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
      await vi.waitFor(() => {
        expect(a.fields[size]?.value).toBe(18);
        expect(b.fields[theme]?.value).toBe("dark");
      });
      b.online = false;
      capturePreferenceWrite(
        "codex-studio-preferences-v1",
        '{"mainFontSize":20}',
        '{"mainFontSize":18}',
      );
      await vi.waitFor(() => expect(a.fields[size]?.value).toBe(20));
      b.online = true;
      window.dispatchEvent(new Event("online"));
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
      await vi.waitFor(() =>
        expect(streams.has("https://lost.invalid")).toBe(true),
      );
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
    } finally {
      stop();
    }
  });
});
