import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import {
  mkdtemp,
  writeFile,
  readFile,
  stat,
  readdir,
  open,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import electron from "electron";
import { exportBrowserProfile } from "./export-browser-profile.mjs";
import { importBrowserProfile } from "../web/src/native/importBrowserProfile.ts";
import { combineBrowserProfiles } from "../web/src/native/combineBrowserProfiles.ts";

const run = promisify(execFile);
const fixture = await mkdtemp(path.join(tmpdir(), "studio-profile-export-"));
const profile = path.join(fixture, "profile");
const output = path.join(fixture, "snapshot.json");
const seed = path.join(fixture, "seed.mjs");
const manifest = path.join(fixture, "manifest.json");
const serviceWorker = `self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', event => event.waitUntil(self.clients.claim()));
self.addEventListener('fetch', event => event.respondWith(new Response("<!doctype html><script>localStorage.setItem('unexpected-app-run','yes')</script>", {headers:{'Content-Type':'text/html'}})));`;
await writeFile(
  manifest,
  JSON.stringify({
    profile,
    output,
    origins: ["http://127.0.0.1:4620", "http://studio-nrxwgylm.localhost:4623"],
    contexts: {
      "http://studio-nrxwgylm.localhost:4623": "http://127.0.0.1:4620",
    },
  }),
);
await writeFile(
  seed,
  `
import {app, BrowserWindow, session} from 'electron';
async function seed() {
app.setPath('userData', ${JSON.stringify(profile)});
await app.whenReady();
await session.defaultSession.protocol.handle('http', (request) => request.url.endsWith('/sw.js')
  ? new Response(${JSON.stringify(serviceWorker)}, {headers:{'Content-Type':'text/javascript'}})
  : new Response('<!doctype html><title>fixture</title>', {headers:{'Content-Type':'text/html'}}));
const window = new BrowserWindow({show:false, focusable:false, webPreferences:{sandbox:true,contextIsolation:true,nodeIntegration:false}});
await window.loadURL('http://127.0.0.1:4620/');
await window.webContents.executeJavaScript(\`(async () => {
  localStorage.setItem('codex-drafts:workspace', JSON.stringify({room:'Сохранить 🦀'}));
  const db = await new Promise((resolve,reject) => {
    const r = indexedDB.open('studio-fixture', 1);
    r.onupgradeneeded = () => {
      const store = r.result.createObjectStore('records', {keyPath:'id'});
      store.createIndex('session', 'session', {unique:false});
      r.result.createObjectStore('changes', {autoIncrement:true});
      r.result.createObjectStore('nested', {autoIncrement:true, keyPath:'entry.id'});
    };
    r.onsuccess = () => resolve(r.result); r.onerror = () => reject(r.error);
  });
  const tx = db.transaction(['records','changes','nested'], 'readwrite');
  tx.objectStore('changes').put('deleted high key', 100);
  tx.objectStore('changes').delete(100);
  tx.objectStore('nested').put({entry:{id:50},text:'deleted high key'});
  tx.objectStore('nested').delete(50);
  for (let index=0; index<70; index++) tx.objectStore('records').put({id:'z'+String(index).padStart(3,'0'), payload:'x'.repeat(130000)});
  tx.objectStore('records').put({id:'voice', session:'room', pcm:new Int16Array([-32768,0,32767]), file:new File([new Uint8Array([0,255,42])], 'bytes.bin', {type:'application/octet-stream',lastModified:1234}), optional:undefined});
  await new Promise((resolve,reject) => {tx.oncomplete=resolve;tx.onerror=()=>reject(tx.error)});
  db.close();
  await navigator.serviceWorker.register('/sw.js');
  await navigator.serviceWorker.ready;
})()\`);
await window.webContents.executeJavaScript("new Promise(resolve => { const frame=document.createElement('iframe');frame.onload=resolve;frame.src='http://studio-nrxwgylm.localhost:4623/';document.body.append(frame); })");
await window.webContents.mainFrame.frames[0].executeJavaScript("(async()=>{localStorage.setItem('frame-only','partitioned');const db=await new Promise((resolve,reject)=>{const r=indexedDB.open('frame-data',1);r.onupgradeneeded=()=>r.result.createObjectStore('records');r.onsuccess=()=>resolve(r.result);r.onerror=()=>reject(r.error);});await new Promise((resolve,reject)=>{const tx=db.transaction('records','readwrite');tx.objectStore('records').put(new Uint8Array([0,42,255]),'bytes');tx.oncomplete=resolve;tx.onabort=()=>reject(tx.error);});db.close();await navigator.serviceWorker.register('/sw.js');await navigator.serviceWorker.ready;})()");
window.destroy();
app.quit();
}
void seed().catch(error => { console.error(error); app.exit(1); });
`,
);
await run(electron, [seed], { timeout: 90000 });
const exporter = path.join(import.meta.dirname, "export-profile.mjs");
const heldProfile = await open(path.join(profile, "Preferences"), "r");
try {
  await assert.rejects(
    run(electron, [exporter, manifest], { timeout: 90000 }),
    /Close the desktop/,
  );
} finally {
  await heldProfile.close();
}
const exported = await run(electron, [exporter, manifest], { timeout: 90000 });
assert.match(
  exported.stdout,
  /"result":"PASS"/,
  `Exporter did not finish: ${exported.stderr}`,
);
const archive = JSON.parse(await readFile(output, "utf8"));
assert.equal(archive.version, 2);
assert.equal(archive.snapshots.length, 2);
assert.equal(archive.snapshots[1].topLevelOrigin, "http://127.0.0.1:4620");
assert.equal(archive.snapshots[1].databases[0].stores[0].chunks[0].records, 1);
assert.deepEqual(archive.snapshots[1].preferences, {
  "frame-only": "partitioned",
});
const snapshot = combineBrowserProfiles(
  archive.snapshots.map((snapshot, index) => ({
    owner: index === 0 ? "root" : "server:remote",
    snapshot,
  })),
);
assert.equal(snapshot.preferences["unexpected-app-run"], undefined);
assert.equal(
  JSON.parse(snapshot.preferences["codex-drafts:workspace"]).room,
  "Сохранить 🦀",
);
const store = snapshot.databases
  .find((db) => db.name === "studio-fixture")
  .stores.find((store) => store.name === "records");
assert.equal(
  snapshot.databases
    .find((db) => db.name === "studio-fixture")
    .stores.find((store) => store.name === "changes").nextKey,
  101,
);
assert.equal(
  snapshot.databases
    .find((db) => db.name === "studio-fixture")
    .stores.find((store) => store.name === "nested").nextKey,
  51,
);
assert.equal(store.keyPath, "id");
assert.ok(store.chunks.length > 1);
assert.equal(
  store.chunks.reduce((sum, chunk) => sum + chunk.records, 0),
  71,
);
assert.deepEqual(store.indexes, [
  { name: "session", keyPath: "session", unique: false, multiEntry: false },
]);
const chunks = {};
const materialized = structuredClone(snapshot.databases);
for (const db of materialized)
  for (const savedStore of db.stores) {
    savedStore.records = [];
    for (const chunk of savedStore.chunks) {
      const chunkPath = path.join(
        fixture,
        archive.parts,
        chunk.sha256 + ".json",
      );
      assert.equal((await stat(chunkPath)).mode & 0o777, 0o600);
      const bytes = await readFile(chunkPath);
      assert.equal(bytes.length, chunk.bytes);
      chunks[chunk.sha256] = JSON.parse(bytes);
      savedStore.records.push(...chunks[chunk.sha256]);
    }
    delete savedStore.chunks;
  }
const records = materialized
  .find((db) => db.name === "studio-fixture")
  .stores.find((s) => s.name === "records");
const value = Object.fromEntries(records.records[0][1][1]);
assert.deepEqual(value.pcm, ["view", "Int16Array", ["bytes", "AIAAAP9/"]]);
assert.deepEqual(value.file, [
  "file",
  "bytes.bin",
  "application/octet-stream",
  1234,
  ["bytes", "AP8q"],
]);
assert.deepEqual(value.optional, ["undefined"]);
assert.equal((await stat(output)).mode & 0o777, 0o600);
await assert.rejects(
  run(electron, [exporter, manifest], { timeout: 90000 }),
  /EEXIST/,
);
assert.equal(await readFile(output, "utf8"), JSON.stringify(archive));
assert.ok(!(await readdir(fixture)).some((name) => name.endsWith(".tmp")));
const restoreInBrowser = `(async () => {
    const snapshot = ${JSON.stringify(snapshot)};
    const chunks = ${JSON.stringify(chunks)};
    const importer = (${importBrowserProfile.toString()});
    const restore = (snapshot, id) => importer(snapshot, id, chunk => JSON.stringify(chunks[chunk.sha256]));
    let corruptRefused = false;
    try { await importer(snapshot, 'archive-1', () => []); }
    catch { corruptRefused = localStorage.length === 0 && !(await indexedDB.databases()).length; }
    if (!corruptRefused) throw new Error('Corrupt chunks changed destination');
    const capture = (${exportBrowserProfile.toString()});
    const busyRefused = await navigator.locks.request('studio-browser-profile-import', async () => {
      try {await restore(snapshot, 'archive-1'); return false;}
      catch(error) {return error.message.includes('Another view') && localStorage.length===0;}
    });
    if (!busyRefused) throw new Error('Concurrent import was not rejected');
    const count = snapshot.databases.reduce((total, db) => total + db.stores.reduce((sum, store) => sum + store.chunks.length, 0), 0);
    let reads = 0;
    let chunkInterrupted = false;
    try {
      await importer(snapshot, 'archive-1', chunk => {
        if (++reads === count + 2) throw new Error('interrupted chunk read');
        return JSON.stringify(chunks[chunk.sha256]);
      });
    } catch(error) { chunkInterrupted = error.message.includes('interrupted chunk read'); }
    if (!chunkInterrupted || !localStorage.getItem('studio-browser-profile-import-v1')?.endsWith(':pending'))
      throw new Error('Expected resumable partial chunk import');
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function(key,value) {
      if (key === 'studio-browser-profile-import-v1' && value.endsWith(':complete'))
        throw new Error('simulated interrupted completion');
      return original.call(this,key,value);
    };
    let interrupted = false;
    try {await restore(snapshot, 'archive-1');} catch(error) {interrupted = error.message.includes('interrupted');}
    Storage.prototype.setItem = original;
    if (!interrupted) throw new Error('Expected incomplete import');
    const held = await new Promise((resolve, reject) => {
      const opened = indexedDB.open(snapshot.databases[0].name);
      opened.onsuccess = () => resolve(opened.result);
      opened.onerror = () => reject(opened.error);
    });
    const originalDelete = indexedDB.deleteDatabase.bind(indexedDB);
    let signalBlocked;
    const blocked = new Promise(resolve => { signalBlocked = resolve; });
    indexedDB.deleteDatabase = name => {
      const operation = originalDelete(name);
      operation.addEventListener('blocked', signalBlocked, { once:true });
      return operation;
    };
    const waiting = restore(snapshot, 'archive-1').then(() => false, () => true);
    await blocked;
    let locked = false;
    try { await restore(snapshot, 'archive-1'); }
    catch(error) { locked = error.message.includes('Another view'); }
    held.close();
    if (!await waiting || !locked) throw new Error('Blocked import released its ownership early');
    indexedDB.deleteDatabase = originalDelete;
    await restore(snapshot, 'archive-1');
    const restored = await capture();
    localStorage.setItem('new-edit', 'keep me');
    await restore(snapshot, 'archive-1');
    let conflict = false;
    try {await restore(snapshot, 'archive-2');} catch {conflict=true;}
    return {restored, kept:localStorage.getItem('new-edit'), conflict};
  })()`;
const refuseInBrowser = `(async () => {
    localStorage.setItem('existing', 'unchanged');
    try {await (${importBrowserProfile.toString()})(${JSON.stringify(snapshot)}, 'archive-1'); return false;}
    catch {return localStorage.length===1 && localStorage.getItem('existing')==='unchanged' && !(await indexedDB.databases()).length;}
  })()`;
const restore = path.join(fixture, "restore.mjs");
await writeFile(
  restore,
  `
import {app, BrowserWindow, session} from 'electron';
import assert from 'node:assert/strict';
async function restore() {
  app.setPath('userData', ${JSON.stringify(path.join(fixture, "destination"))});
  await app.whenReady();
  await session.defaultSession.protocol.handle('http', () => new Response('<!doctype html>', {headers:{'Content-Type':'text/html'}}));
  const window = new BrowserWindow({show:false, focusable:false, webPreferences:{sandbox:true,contextIsolation:true,nodeIntegration:false}});
  await window.loadURL('http://127.0.0.1:4621/');
  const result = await window.webContents.executeJavaScript(${JSON.stringify(restoreInBrowser)});
  assert.deepEqual(result.restored.databases, ${JSON.stringify(materialized)});
  const preferences = result.restored.preferences;
  delete preferences['studio-browser-profile-import-v1'];
  assert.deepEqual(preferences, ${JSON.stringify(snapshot.preferences)});
  assert.equal(result.kept, 'keep me');
  assert.equal(result.conflict, true);
  await window.loadURL('http://127.0.0.1:4622/');
  const refused = await window.webContents.executeJavaScript(${JSON.stringify(refuseInBrowser)});
  assert.equal(refused, true);
  window.destroy();
  app.quit();
}
void restore().catch(error => {console.error(error); app.exit(1);});
`,
);
await run(process.execPath, ["--check", restore]);
await run(electron, [restore], { timeout: 90000 });
console.log(
  JSON.stringify({
    result: "PASS",
    fixture,
    checks:
      "profile round trip preserves preferences, IndexedDB schema, PCM and files; interrupted import resumes; completed retry preserves edits; conflicts and archive replacement rejected",
  }),
);
