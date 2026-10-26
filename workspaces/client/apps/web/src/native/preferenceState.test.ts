import { storageKeys } from "../storageKeys";
import { scopedStorage } from "../servers/storage";
import { maxPreferenceKeyBytes } from "./preferenceLimits";
import { expect, it, vi } from "vitest";
import { NativePreferenceStorage, preferenceScope } from "./preferenceState";

function memory(): Storage {
  const data = new Map<string, string>();
  return {
    get length() {
      return data.size;
    },
    key: (index) => [...data.keys()][index] ?? null,
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => {
      data.set(key, value);
    },
    removeItem: (key) => {
      data.delete(key);
    },
    clear: () => data.clear(),
  };
}
function backend() {
  const values: Record<string, string> = {};
  const receipts = new Map<string, string>();
  let lose = false;
  const invoke = vi.fn(
    async (method: string, params: unknown, id?: string): Promise<unknown> => {
      if (method === "preferences.raw_all") return { ...values };
      const fingerprint = JSON.stringify([method, params]);
      if (receipts.has(id!)) {
        expect(receipts.get(id!)).toBe(fingerprint);
        return null;
      }
      const data = params as {
        key: string;
        raw: string;
        entries: Record<string, string>;
      };
      if (method === "preferences.raw_set") values[data.key] = data.raw;
      else if (method === "preferences.raw_remove") delete values[data.key];
      else if (method === "preferences.import")
        Object.assign(values, data.entries);
      else throw new Error("unexpected method");
      receipts.set(id!, fingerprint);
      if (lose) {
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
  };
}

it("imports existing preferences once, then retains native edits and clear across restart", async () => {
  const journal = memory();
  journal.setItem("theme", "dark");
  const core = backend();
  const invoke = preferenceScope(core.invoke, "root");
  let storage = await NativePreferenceStorage.open(journal, invoke, vi.fn());
  expect(storage.getItem("theme")).toBe("dark");
  storage.setItem("theme", "light");
  await storage.flush();
  storage = await NativePreferenceStorage.open(journal, invoke, vi.fn());
  expect(storage.getItem("theme")).toBe("light");
  storage.clear();
  await storage.flush();
  storage = await NativePreferenceStorage.open(journal, invoke, vi.fn());
  expect(storage.length).toBe(0);
  expect(journal.getItem("theme")).toBe("dark"); // old WebView cache cannot resurrect it
});

it("replays an uncertain head with its exact ID before a later edit after restart", async () => {
  const journal = memory();
  const core = backend();
  const invoke = preferenceScope(core.invoke, "root");
  const error = vi.fn();
  const storage = await NativePreferenceStorage.open(journal, invoke, error);
  core.loseNext();
  storage.setItem("theme", "dark");
  await expect(storage.flush()).rejects.toThrow("lost reply");
  storage.setItem("theme", "light");
  const recovered = await NativePreferenceStorage.open(
    journal,
    invoke,
    vi.fn(),
  );
  expect(recovered.getItem("theme")).toBe("light");
  const writes = core.invoke.mock.calls.filter(
    ([method]) => method === "preferences.raw_set",
  );
  expect(writes).toHaveLength(3);
  expect(writes[0]?.[2]).toBe(writes[1]?.[2]);
  expect(writes[2]?.[2]).not.toBe(writes[0]?.[2]);
  expect(error).toHaveBeenCalledOnce();
});

it("does not change the visible value or submit a write if journal persistence fails", async () => {
  const journal = memory();
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    journal,
    preferenceScope(core.invoke, "root"),
    vi.fn(),
  );
  journal.setItem = () => {
    throw new Error("quota");
  };
  expect(() => storage.setItem("theme", "dark")).toThrow("quota");
  expect(storage.getItem("theme")).toBeNull();
  expect(
    core.invoke.mock.calls.every(
      ([method]) => method === "preferences.raw_all",
    ),
  ).toBe(true);
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

it("rejects oversized scoped keys before journaling, without blocking later writes", async () => {
  const journal = memory();
  const core = backend();
  const scope = "server:remote";
  const storage = await NativePreferenceStorage.open(
    journal,
    preferenceScope(core.invoke, scope),
    vi.fn(),
    scope,
  );
  expect(() =>
    storage.setItem("Ж".repeat(maxPreferenceKeyBytes / 2), "bad"),
  ).toThrow("Invalid preference key");
  expect(storage.length).toBe(0);
  expect(journal.getItem("studio-native-preferences-journal-v1")).toBeNull();
  storage.setItem("theme", "dark");
  await storage.flush();
  expect(core.values['browser:"server:remote":theme']).toBe("dark");
});

it("flush includes a write queued at drain completion", async () => {
  const journal = memory();
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    journal,
    preferenceScope(core.invoke, "root"),
    vi.fn(),
  );
  const original = journal.setItem;
  let inject = true;
  journal.setItem = (key, value) => {
    original(key, value);
    if (
      key === "studio-native-preferences-journal-v1" &&
      value === "[]" &&
      inject
    ) {
      inject = false;
      queueMicrotask(() => storage.setItem("second", "two"));
    }
  };
  storage.setItem("first", "one");
  await storage.flush();
  expect(core.values['browser:"root":second']).toBe("two");
  expect(journal.getItem("studio-native-preferences-journal-v1")).toBe("[]");
});

it("keeps draft recovery records out of the preference import", async () => {
  const journal = memory();
  journal.setItem(
    "studio-native-draft-journal-v1:workspace:request",
    "saved draft",
  );
  journal.setItem("theme", "dark");
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    journal,
    preferenceScope(core.invoke, "root"),
    vi.fn(),
  );
  expect(storage.getItem("theme")).toBe("dark");
  expect(
    storage.getItem("studio-native-draft-journal-v1:workspace:request"),
  ).toBeNull();
  storage.clear();
  await storage.flush();
  expect(
    journal.getItem("studio-native-draft-journal-v1:workspace:request"),
  ).toBe("saved draft");
});

it("imports long saved answer draft keys without dropping the draft", async () => {
  const journal = memory();
  const key = "studio-answer-draft:" + "question".repeat(220);
  journal.setItem(key, "saved answer");
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    journal,
    preferenceScope(core.invoke, "root"),
    vi.fn(),
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
    memory(),
    preferenceScope(core.invoke, "root"),
    vi.fn(),
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
  const journal = memory();
  const core = backend();
  const storage = await NativePreferenceStorage.open(
    journal,
    preferenceScope(core.invoke, "root"),
    vi.fn(),
  );
  core.loseNext();
  storage.setItem("theme", "dark");
  await expect(storage.flush()).rejects.toThrow("lost reply");
  const pending = journal.getItem("studio-native-preferences-journal-v1");
  storage.setItem("theme", "dark");
  expect(journal.getItem("studio-native-preferences-journal-v1")).toBe(pending);
  await storage.flush();
  storage.setItem("theme", "dark");
  await storage.flush();
  const writes = core.invoke.mock.calls.filter(
    ([method]) => method === "preferences.raw_set",
  );
  expect(writes).toHaveLength(2);
  expect(writes[0]?.[2]).toBe(writes[1]?.[2]);
  storage.setItem("theme", "light");
  await storage.flush();
  expect(storage.getItem("theme")).toBe("light");
});
