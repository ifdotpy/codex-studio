import { required } from "../components/componentTestFixtures.ts";
import { storageKeys } from "../storageKeys";
import { scopedStorage } from "../servers/storage";
import { maxPreferenceKeyBytes } from "./preferenceLimits";
import { expect, it, vi } from "vitest";
import { NativePreferenceStorage, preferenceScope } from "./preferenceState";

const journalKey = 'browser:"root":studio-native-preferences-journal-v1';

function backend() {
  const values: Record<string, string> = {};
  const receipts = new Map<string, string>();
  let lose = false;
  let rejectJournal = false;
  const invoke = vi.fn(
    async (method: string, params: unknown, id?: string): Promise<unknown> => {
      if (method === "preferences.raw_all") return { ...values };
      const data = params as {
        key: string;
        raw: string;
        entries: Record<string, string>;
      };
      if (
        method === "preferences.raw_set" &&
        data.key.endsWith("studio-native-preferences-journal-v1") &&
        rejectJournal
      )
        throw new Error("quota");
      const fingerprint = JSON.stringify([method, params]);
      if (receipts.has(id!)) {
        expect(receipts.get(id!)).toBe(fingerprint);
        return null;
      }
      if (method === "preferences.raw_set") values[data.key] = data.raw;
      else if (method === "preferences.raw_remove") delete values[data.key];
      else if (method === "preferences.import")
        Object.assign(values, data.entries);
      else throw new Error("unexpected method");
      receipts.set(id!, fingerprint);
      // The loss applies to the next preference write, not to a journal save.
      if (
        lose &&
        !String(data.key).endsWith("studio-native-preferences-journal-v1")
      ) {
        lose = false;
        throw new Error("lost reply");
      }
      return null;
    },
  );
  return {
    values,
    invoke,
    loseNext: () => {
      lose = true;
    },
    setJournalRejected: (value: boolean) => {
      rejectJournal = value;
    },
  };
}

it("imports legacy browser entries once into an empty core, then never reads them", async () => {
  const core = backend();
  const invoke = preferenceScope(core.invoke, "root");
  const legacy = vi.fn(() => ({ theme: "dark" }));
  let storage = await NativePreferenceStorage.open(invoke, vi.fn(), legacy);
  expect(storage.getItem("theme")).toBe("dark");
  storage.setItem("theme", "light");
  await storage.flush();
  storage = await NativePreferenceStorage.open(invoke, vi.fn(), legacy);
  expect(storage.getItem("theme")).toBe("light");
  expect(legacy).toHaveBeenCalledTimes(1);
});

it("keeps native edits and clear across restart without resurrecting legacy entries", async () => {
  const core = backend();
  const invoke = preferenceScope(core.invoke, "root");
  const legacy = vi.fn(() => ({ theme: "dark" }));
  let storage = await NativePreferenceStorage.open(invoke, vi.fn(), legacy);
  storage.clear();
  await storage.flush();
  storage = await NativePreferenceStorage.open(invoke, vi.fn(), legacy);
  expect(storage.length).toBe(0);
  expect(legacy).toHaveBeenCalledTimes(1);
});

it("does not import legacy entries into a core that already has preferences", async () => {
  const core = backend();
  core.values['browser:"root":server-kept'] = "1";
  const legacy = vi.fn(() => ({ theme: "dark" }));
  const storage = await NativePreferenceStorage.open(
    preferenceScope(core.invoke, "root"),
    vi.fn(),
    legacy,
  );
  expect(storage.getItem("theme")).toBeNull();
  expect(storage.getItem("server-kept")).toBe("1");
  expect(core.values['browser:"root":theme']).toBeUndefined();
});

it("replays an uncertain head with its exact ID before a later edit after restart", async () => {
  const core = backend();
  const invoke = preferenceScope(core.invoke, "root");
  const error = vi.fn();
  const storage = await NativePreferenceStorage.open(invoke, error, () => ({}));
  core.loseNext();
  storage.setItem("theme", "dark");
  await expect(storage.flush()).rejects.toThrow("lost reply");
  storage.setItem("theme", "light");
  await vi.waitFor(() =>
    expect(core.values[journalKey]).toContain('"raw":"light"'),
  );
  const recovered = await NativePreferenceStorage.open(
    invoke,
    vi.fn(),
    () => ({}),
  );
  expect(recovered.getItem("theme")).toBe("light");
  const writes = core.invoke.mock.calls.filter(
    ([method, params]) =>
      method === "preferences.raw_set" &&
      (params as { key: string }).key === 'browser:"root":theme',
  );
  expect(writes).toHaveLength(3);
  expect(required(writes[0])[2]).toBe(required(writes[1])[2]);
  expect(required(writes[2])[2]).not.toBe(required(writes[0])[2]);
  expect(error).toHaveBeenCalledOnce();
});

it("keeps the visible value but sends nothing when the journal cannot be saved", async () => {
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    preferenceScope(core.invoke, "root"),
    vi.fn(),
    () => ({}),
  );
  core.setJournalRejected(true);
  storage.setItem("theme", "dark");
  await expect(storage.flush()).rejects.toThrow("quota");
  expect(storage.getItem("theme")).toBe("dark");
  expect(core.values['browser:"root":theme']).toBeUndefined();
  expect(
    core.invoke.mock.calls.some(
      ([method, params]) =>
        method === "preferences.raw_set" &&
        (params as { key: string }).key === "theme",
    ),
  ).toBe(false);
});

it("writes the pending journal only to core rows", async () => {
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    preferenceScope(core.invoke, "root"),
    vi.fn(),
    () => ({}),
  );
  core.loseNext();
  storage.setItem("theme", "dark");
  await expect(storage.flush()).rejects.toThrow("lost reply");
  expect(core.values[journalKey]).toContain('"key":"theme"');
  storage.setItem("theme", "light");
  await storage.flush();
  expect(core.values[journalKey]).toBeUndefined();
  expect(core.values['browser:"root":theme']).toBe("light");
});

it("isolates server namespaces and rejects non-preference operations", async () => {
  const core = backend();
  const first = preferenceScope(core.invoke, "server:first");
  const second = preferenceScope(core.invoke, "server:second");
  await first("preferences.raw_set", { key: "theme", raw: "dark" }, "one");
  expect(await first("preferences.raw_all", {})).toEqual({ theme: "dark" });
  expect(await second("preferences.raw_all", {})).toEqual({});
  await expect(second("api.request", { path: "/api/session" })).rejects.toThrow(
    "unavailable",
  );
});

it("rejects oversized scoped keys before queuing, without blocking later writes", async () => {
  const core = backend();
  const scope = "server:remote";
  const storage = await NativePreferenceStorage.open(
    preferenceScope(core.invoke, scope),
    vi.fn(),
    () => ({}),
    scope,
  );
  expect(() =>
    storage.setItem("Ж".repeat(maxPreferenceKeyBytes / 2), "bad"),
  ).toThrow("Invalid preference key");
  expect(storage.length).toBe(0);
  expect(core.values['browser:"server:remote":journal']).toBeUndefined();
  storage.setItem("theme", "dark");
  await storage.flush();
  expect(core.values['browser:"server:remote":theme']).toBe("dark");
});

it("flush sends a write queued while the previous drain runs", async () => {
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    preferenceScope(core.invoke, "root"),
    vi.fn(),
    () => ({}),
  );
  storage.setItem("first", "one");
  const draining = storage.flush();
  storage.setItem("second", "two");
  await draining;
  expect(core.values['browser:"root":second']).toBe("two");
  expect(core.values[journalKey]).toBeUndefined();
});

it("keeps draft recovery records out of the preference import", async () => {
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    preferenceScope(core.invoke, "root"),
    vi.fn(),
    () => ({
      "studio-native-draft-journal-v1:workspace:request": "saved draft",
      theme: "dark",
    }),
  );
  expect(storage.getItem("theme")).toBe("dark");
  expect(
    storage.getItem("studio-native-draft-journal-v1:workspace:request"),
  ).toBeNull();
  expect(
    core.values[
      'browser:"root":studio-native-draft-journal-v1:workspace:request'
    ],
  ).toBeUndefined();
});

it("imports long saved answer draft keys without dropping the draft", async () => {
  const key = "studio-answer-draft:" + "question".repeat(220);
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    preferenceScope(core.invoke, "root"),
    vi.fn(),
    () => ({ [key]: "saved answer" }),
    "root",
  );
  expect(storage.getItem(key)).toBe("saved answer");
  expect(core.values['browser:"root":' + key]).toBe("saved answer");
});

it("enumerates owner journals without rescanning or copying all native keys per key", async () => {
  const core = backend();
  for (let i = 0; i < 40; i++) {
    core.values[`browser:"root":server:a:journal:${i}`] = "a";
    core.values[`browser:"root":server:b:journal:${i}`] = "b";
  }
  const storage = await NativePreferenceStorage.open(
    preferenceScope(core.invoke, "root"),
    vi.fn(),
    () => ({}),
  );
  const a = scopedStorage(() => storage, "server:a:");
  const b = scopedStorage(() => storage, "server:b:");
  const key = vi.spyOn(storage, "key");
  const initial = storageKeys(storage);
  const selected = storageKeys(a);
  const enumerated: string[] = [];
  for (let i = 0; i < a.length; i++) enumerated.push(a.key(i)!);
  expect(enumerated).toEqual(selected);
  expect(key).not.toHaveBeenCalled();
  expect(storageKeys(storage)).toBe(initial);
  expect(storageKeys(a)).toBe(selected);
  a.setItem("journal:0", "updated");
  expect(storageKeys(storage)).toBe(initial);
  expect(a.getItem("journal:0")).toBe("updated");
  b.setItem("new", "b");
  expect(storageKeys(storage)).not.toBe(initial);
  expect(a.length).toBe(40);
  expect(b.key(40)).toBe("new");
  storage.removeItem("server:a:journal:0");
  expect(a.length).toBe(39);
  expect(a.key(0)).toBe("journal:1");
  a.clear();
  expect(a.length).toBe(0);
  expect(b.length).toBe(41);
  await storage.flush();
});

it("does not enqueue an unchanged native value or discard its uncertain receipt", async () => {
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    preferenceScope(core.invoke, "root"),
    vi.fn(),
    () => ({}),
  );
  core.loseNext();
  storage.setItem("theme", "dark");
  await expect(storage.flush()).rejects.toThrow("lost reply");
  const pending = core.values[journalKey];
  storage.setItem("theme", "dark");
  expect(core.values[journalKey]).toBe(pending);
  await storage.flush();
  storage.setItem("theme", "dark");
  await storage.flush();
  const writes = core.invoke.mock.calls.filter(
    ([method, params]) =>
      method === "preferences.raw_set" &&
      (params as { key: string }).key === 'browser:"root":theme',
  );
  expect(writes).toHaveLength(2);
  expect(required(writes[0])[2]).toBe(required(writes[1])[2]);
  storage.setItem("theme", "light");
  await storage.flush();
  expect(storage.getItem("theme")).toBe("light");
});
