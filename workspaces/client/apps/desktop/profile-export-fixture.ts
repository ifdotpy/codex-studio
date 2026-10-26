/// <reference lib="webworker" />
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { app, BrowserWindow, session } from "electron";
import type {
  BrowserProfileSnapshot,
  BrowserProfileRecord,
} from "../web/src/native/importBrowserProfile.ts";
import type { ExportManifest } from "./export-profile.ts";
import { importBrowserProfile } from "../web/src/native/importBrowserProfile.ts";
import { exportBrowserProfile } from "./export-browser-profile.ts";

export function profileServiceWorker(
  worker: Pick<
    ServiceWorkerGlobalScope,
    "addEventListener" | "skipWaiting" | "clients"
  >,
) {
  worker.addEventListener("install", () => worker.skipWaiting());
  worker.addEventListener("activate", (event) =>
    event.waitUntil(worker.clients.claim()),
  );
  worker.addEventListener("fetch", (event) =>
    event.respondWith(
      new Response(
        "<!doctype html><script>localStorage.setItem('unexpected-app-run','yes')</script>",
        { headers: { "Content-Type": "text/html" } },
      ),
    ),
  );
}

export async function seedStorage() {
  localStorage.setItem(
    "codex-drafts:workspace",
    JSON.stringify({ room: "Сохранить 🦀" }),
  );
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const r = indexedDB.open("studio-fixture", 1);
    r.onupgradeneeded = () => {
      const store = r.result.createObjectStore("records", { keyPath: "id" });
      store.createIndex("session", "session", { unique: false });
      r.result.createObjectStore("changes", { autoIncrement: true });
      r.result.createObjectStore("nested", {
        autoIncrement: true,
        keyPath: "entry.id",
      });
    };
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
  const tx = db.transaction(["records", "changes", "nested"], "readwrite");
  tx.objectStore("changes").put("deleted high key", 100);
  tx.objectStore("changes").delete(100);
  tx.objectStore("nested").put({ entry: { id: 50 }, text: "deleted high key" });
  tx.objectStore("nested").delete(50);
  for (let index = 0; index < 70; index++)
    tx.objectStore("records").put({
      id: "z" + String(index).padStart(3, "0"),
      payload: "x".repeat(130000),
    });
  tx.objectStore("records").put({
    id: "voice",
    session: "room",
    pcm: new Int16Array([-32768, 0, 32767]),
    file: new File([new Uint8Array([0, 255, 42])], "bytes.bin", {
      type: "application/octet-stream",
      lastModified: 1234,
    }),
    optional: undefined,
  });
  await new Promise<Event>((resolve, reject) => {
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
  });
  db.close();
  await navigator.serviceWorker.register("/sw.js");
  await navigator.serviceWorker.ready;
}

export async function seedFrameStorage() {
  localStorage.setItem("frame-only", "partitioned");
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const r = indexedDB.open("frame-data", 1);
    r.onupgradeneeded = () => r.result.createObjectStore("records");
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
  await new Promise<Event>((resolve, reject) => {
    const tx = db.transaction("records", "readwrite");
    tx.objectStore("records").put(new Uint8Array([0, 42, 255]), "bytes");
    tx.oncomplete = resolve;
    tx.onabort = () => reject(tx.error);
  });
  db.close();
  await navigator.serviceWorker.register("/sw.js");
  await navigator.serviceWorker.ready;
}

export async function createSourceFrame() {
  await new Promise<Event>((resolve) => {
    const frame = document.createElement("iframe");
    frame.onload = resolve;
    frame.src = "http://studio-nrxwgylm.localhost:4623/";
    document.body.append(frame);
  });
}

export async function restoreStorage(
  snapshot: BrowserProfileSnapshot,
  chunks: Record<string, BrowserProfileRecord[]>,
  importer: typeof importBrowserProfile,
  capture: typeof exportBrowserProfile,
) {
  const restore = (snapshot: BrowserProfileSnapshot, id: string) =>
    importer(snapshot, id, (chunk) => JSON.stringify(chunks[chunk.sha256]));
  let corruptRefused = false;
  try {
    await importer(snapshot, "archive-1", () => []);
  } catch {
    corruptRefused =
      localStorage.length === 0 && !(await indexedDB.databases()).length;
  }
  if (!corruptRefused) throw new Error("Corrupt chunks changed destination");
  const busyRefused = await navigator.locks.request(
    "studio-browser-profile-import",
    async () => {
      try {
        await restore(snapshot, "archive-1");
        return false;
      } catch (error) {
        return (
          error instanceof Error &&
          error.message.includes("Another view") &&
          localStorage.length === 0
        );
      }
    },
  );
  if (!busyRefused) throw new Error("Concurrent import was not rejected");
  const count = snapshot.databases.reduce(
    (total, db) =>
      total +
      db.stores.reduce((sum, store) => sum + (store.chunks?.length ?? 0), 0),
    0,
  );
  let reads = 0;
  let chunkInterrupted = false;
  try {
    await importer(snapshot, "archive-1", (chunk) => {
      if (++reads === count + 2) throw new Error("interrupted chunk read");
      return JSON.stringify(chunks[chunk.sha256]);
    });
  } catch (error) {
    chunkInterrupted =
      error instanceof Error &&
      error.message.includes("interrupted chunk read");
  }
  if (
    !chunkInterrupted ||
    !localStorage
      .getItem("studio-browser-profile-import-v1")
      ?.endsWith(":pending")
  )
    throw new Error("Expected resumable partial chunk import");
  const original = Storage.prototype.setItem;
  Storage.prototype.setItem = function (key, value) {
    if (
      key === "studio-browser-profile-import-v1" &&
      value.endsWith(":complete")
    )
      throw new Error("simulated interrupted completion");
    return original.call(this, key, value);
  };
  let interrupted = false;
  try {
    await restore(snapshot, "archive-1");
  } catch (error) {
    interrupted =
      error instanceof Error && error.message.includes("interrupted");
  }
  Storage.prototype.setItem = original;
  if (!interrupted) throw new Error("Expected incomplete import");
  const held = await new Promise<IDBDatabase>((resolve, reject) => {
    const opened = indexedDB.open(snapshot.databases[0]?.name ?? "");
    opened.onsuccess = () => resolve(opened.result);
    opened.onerror = () => reject(opened.error);
  });
  const originalDelete = indexedDB.deleteDatabase.bind(indexedDB);
  let signalBlocked: (event: Event) => void = () => {};
  const blocked = new Promise<Event>((resolve) => {
    signalBlocked = resolve;
  });
  indexedDB.deleteDatabase = (name) => {
    const operation = originalDelete(name);
    operation.addEventListener("blocked", signalBlocked, { once: true });
    return operation;
  };
  const waiting = restore(snapshot, "archive-1").then(
    () => false,
    () => true,
  );
  await blocked;
  let locked = false;
  try {
    await restore(snapshot, "archive-1");
  } catch (error) {
    locked = error instanceof Error && error.message.includes("Another view");
  }
  held.close();
  if (!(await waiting) || !locked)
    throw new Error("Blocked import released its ownership early");
  indexedDB.deleteDatabase = originalDelete;
  await restore(snapshot, "archive-1");
  const restored = await capture();
  localStorage.setItem("new-edit", "keep me");
  await restore(snapshot, "archive-1");
  let conflict = false;
  try {
    await restore(snapshot, "archive-2");
  } catch {
    conflict = true;
  }
  return { restored, kept: localStorage.getItem("new-edit"), conflict };
}

export async function refuseStorage(
  snapshot: BrowserProfileSnapshot,
  importer: typeof importBrowserProfile,
) {
  localStorage.setItem("existing", "unchanged");
  try {
    await importer(snapshot, "archive-1");
    return false;
  } catch {
    return (
      localStorage.length === 1 &&
      localStorage.getItem("existing") === "unchanged" &&
      !(await indexedDB.databases()).length
    );
  }
}

export type ProfileRestoreFixture = {
  profile: string;
  snapshot: BrowserProfileSnapshot;
  chunks: Record<string, BrowserProfileRecord[]>;
  materialized: BrowserProfileSnapshot["databases"];
};
// The paired test writes these two fixture documents immediately before launch.
function readSeedManifest(file: string): ExportManifest {
  return JSON.parse(readFileSync(file, "utf8"));
}
function readRestoreFixture(file: string): ProfileRestoreFixture {
  return JSON.parse(readFileSync(file, "utf8"));
}

async function main() {
  const [mode, file] = process.argv.slice(2);
  if (!file) throw new Error("Supply the fixture document.");
  const seed = mode === "seed" ? readSeedManifest(file) : undefined;
  const restore = mode === "restore" ? readRestoreFixture(file) : undefined;
  const profile = seed?.profile ?? restore?.profile;
  if (!profile) throw new Error("Supply the fixture profile.");
  app.setPath("userData", profile);
  await app.whenReady();
  const workerSource = `(${profileServiceWorker.toString()})(self);`;
  await session.defaultSession.protocol.handle("http", (request) =>
    seed && request.url.endsWith("/sw.js")
      ? new Response(workerSource, {
          headers: { "Content-Type": "text/javascript" },
        })
      : new Response("<!doctype html><title>fixture</title>", {
          headers: { "Content-Type": "text/html" },
        }),
  );
  const window = new BrowserWindow({
    show: false,
    focusable: false,
    webPreferences: {
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  if (seed) {
    await window.loadURL("http://127.0.0.1:4620/");
    await window.webContents.executeJavaScript(`(${seedStorage.toString()})()`);
    await window.webContents.executeJavaScript(
      `(${createSourceFrame.toString()})()`,
    );
    const frame = window.webContents.mainFrame.frames[0];
    assert.ok(frame);
    await frame.executeJavaScript(`(${seedFrameStorage.toString()})()`);
  } else if (restore) {
    await window.loadURL("http://127.0.0.1:4621/");
    const result: Awaited<ReturnType<typeof restoreStorage>> =
      await window.webContents.executeJavaScript(
        `(${restoreStorage.toString()})(${JSON.stringify(restore.snapshot)},${JSON.stringify(restore.chunks)},(${importBrowserProfile.toString()}),(${exportBrowserProfile.toString()}))`,
      );
    assert.deepEqual(result.restored.databases, restore.materialized);
    const preferences = result.restored.preferences;
    delete preferences["studio-browser-profile-import-v1"];
    assert.deepEqual(preferences, restore.snapshot.preferences);
    assert.equal(result.kept, "keep me");
    assert.equal(result.conflict, true);
    await window.loadURL("http://127.0.0.1:4622/");
    const refused: boolean = await window.webContents.executeJavaScript(
      `(${refuseStorage.toString()})(${JSON.stringify(restore.snapshot)},(${importBrowserProfile.toString()}))`,
    );
    assert.equal(refused, true);
  }
  window.destroy();
  app.quit();
}
void main().catch((error) => {
  console.error(error);
  app.exit(1);
});
