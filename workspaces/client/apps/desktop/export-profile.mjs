import { app, BrowserWindow, session, safeStorage } from "electron";
import { execFileSync } from "node:child_process";
import { open, link, unlink, mkdir, rm } from "node:fs/promises";
import { readFileSync, realpathSync, writeSync } from "node:fs";
import { randomUUID, createHash } from "node:crypto";
import path from "node:path";
import { exportServerCredentials } from "./export-server-credentials.mjs";
import { exportBrowserProfile } from "./export-browser-profile.mjs";

// Explicit origins are required: an offline profile can contain several old
// frame ports. The migration caller must enumerate them before importing.
async function main() {
  const manifestPath = process.argv[2];
  let outputTemporary;
  let partsDirectory;
  let published = false;
  let exitCode = 1;
  try {
    if (!manifestPath)
      throw new Error("Supply an offline profile export manifest.");
    const input = JSON.parse(readFileSync(manifestPath, "utf8"));
    const profile = realpathSync(input.profile);
    const output = path.resolve(input.output);
    if (output === profile || output.startsWith(profile + path.sep))
      throw new Error("Export outside the original profile.");
    if (!Array.isArray(input.origins) || !input.origins.length)
      throw new Error("Supply every saved Studio browser origin.");
    const origins = [
      ...new Set(
        input.origins.map((value) => {
          const url = new URL(value);
          if (
            url.protocol !== "http:" ||
            url.username ||
            url.password ||
            url.pathname !== "/" ||
            url.search ||
            url.hash ||
            !(
              url.hostname === "127.0.0.1" ||
              url.hostname === "localhost" ||
              /^studio-[a-z2-7.]+\.localhost$/.test(url.hostname)
            )
          )
            throw new Error("Only saved local Studio origins can be exported.");
          return url.origin;
        }),
      ),
    ];
    const contexts = input.contexts ?? {};
    if (
      typeof contexts !== "object" ||
      Array.isArray(contexts) ||
      Object.entries(contexts).some(
        ([origin, parent]) =>
          !origins.includes(origin) ||
          !origins.includes(parent) ||
          origin === parent,
      )
    )
      throw new Error("Invalid source frame context.");
    // Refuse before requesting Electron's singleton lock: contacting a live
    // singleton would send it a second-instance event and could activate it.
    let owners;
    try {
      owners = execFileSync("/usr/sbin/lsof", ["-t", "+D", profile], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
        timeout: 15000,
      }).trim();
    } catch (error) {
      if (error.status !== 1 || String(error.stderr || "").trim()) throw error;
      // lsof can report matching owners and still exit 1 when another lookup
      // in the recursive scan fails. Its stdout remains evidence of use.
      owners = String(error.stdout || "").trim();
    }
    if (owners)
      throw new Error(
        "Close the desktop before exporting its profile. The backend may keep running.",
      );
    app.setPath("userData", profile);
    app.setName("Codex Studio");
    if (!app.requestSingleInstanceLock())
      throw new Error("The profile is already open.");
    // Closing the hidden reader must not quit Electron before the archive fsync.
    app.on("window-all-closed", () => {});
    await app.whenReady();
    session.defaultSession.setPermissionCheckHandler(() => false);
    session.defaultSession.setPermissionRequestHandler(
      (_contents, _permission, reply) => reply(false),
    );
    await session.defaultSession.protocol.handle("http", (request) => {
      const url = new URL(request.url);
      return new Response(
        origins.includes(url.origin) && url.pathname === "/"
          ? "<!doctype html><title>Offline profile export</title>"
          : "",
        {
          status:
            origins.includes(url.origin) && url.pathname === "/" ? 200 : 403,
          headers: {
            "Content-Type": "text/html",
            "Content-Security-Policy":
              "default-src 'none'; frame-src " + origins.join(" "),
          },
        },
      );
    });
    const destinations = input.owners ?? {};
    if (
      typeof destinations !== "object" ||
      Array.isArray(destinations) ||
      Object.entries(destinations).some(
        ([owner, origin]) =>
          !(owner === "root" || /^server:[a-zA-Z0-9_-]+$/.test(owner)) ||
          !origins.includes(origin),
      )
    )
      throw new Error("Invalid destination owner mapping.");
    const id = randomUUID();
    const partsName = path.basename(output) + "." + id + ".parts";
    partsDirectory = path.join(path.dirname(output), partsName);
    await mkdir(partsDirectory, { mode: 0o700 });
    const snapshots = [];
    for (const origin of origins) {
      console.error("Export source " + (snapshots.length + 1));
      const window = new BrowserWindow({
        show: false,
        focusable: false,
        skipTaskbar: true,
        webPreferences: {
          sandbox: true,
          contextIsolation: true,
          nodeIntegration: false,
        },
      });
      try {
        // A saved service worker can otherwise serve and run the old application
        // instead of our inert document, changing the profile during export.
        await window.loadURL("about:blank");
        window.webContents.debugger.attach("1.3");
        await window.webContents.debugger.sendCommand("Network.enable");
        await window.webContents.debugger.sendCommand(
          "Network.setBypassServiceWorker",
          { bypass: true },
        );
        await window.webContents.debugger.sendCommand(
          "Network.setCacheDisabled",
          { cacheDisabled: true },
        );
        await window.webContents.debugger.sendCommand("Runtime.addBinding", {
          name: "studioExportChunk",
        });
        window.webContents.debugger.on(
          "message",
          async (_event, method, params, sessionId) => {
            if (
              method !== "Runtime.bindingCalled" ||
              params.name !== "studioExportChunk"
            )
              return;
            let requestId;
            let result;
            try {
              if (Buffer.byteLength(params.payload) > 64 * 1024 * 1024 + 1024)
                throw new Error("Migration chunk is too large.");
              const payload = JSON.parse(params.payload);
              requestId = payload.id;
              if (
                !Number.isSafeInteger(requestId) ||
                !Array.isArray(payload.records)
              )
                throw new Error("Invalid migration chunk.");
              const bytes = Buffer.from(JSON.stringify(payload.records));
              if (bytes.length > 64 * 1024 * 1024)
                throw new Error("Migration chunk is too large.");
              const sha256 = createHash("sha256").update(bytes).digest("hex");
              let file;
              try {
                file = await open(
                  path.join(partsDirectory, sha256 + ".json"),
                  "wx",
                  0o600,
                );
              } catch (error) {
                if (error.code !== "EEXIST") throw error;
              }
              if (file) {
                try {
                  await file.writeFile(bytes);
                  await file.sync();
                } finally {
                  await file.close();
                }
              }
              result = {
                value: {
                  sha256,
                  bytes: bytes.length,
                  records: payload.records.length,
                },
              };
            } catch {
              result = { error: "Could not save a profile migration chunk." };
            }
            if (requestId === undefined) return;
            await window.webContents.debugger
              .sendCommand(
                "Runtime.evaluate",
                {
                  expression: `window.__studioExportReplies.get(${JSON.stringify(requestId)})?.(${JSON.stringify(result)})`,
                  contextId: params.executionContextId,
                },
                sessionId,
              )
              .catch(() => {});
          },
        );
        await window.loadURL((contexts[origin] || origin) + "/");
        let source = window.webContents;
        if (contexts[origin]) {
          await window.webContents
            .executeJavaScript(`new Promise((resolve,reject) => {
            const frame = document.createElement('iframe');
            window.__studioExportReplies = new Map();
            addEventListener('message', event => {
              if (event.source !== frame.contentWindow || event.origin !== ${JSON.stringify(origin)} || event.data?.kind !== 'profile-export-chunk') return;
              const {id, payload} = event.data;
              window.__studioExportReplies.set(id, result => {
                window.__studioExportReplies.delete(id);
                frame.contentWindow.postMessage({kind:'profile-export-reply', id, result}, ${JSON.stringify(origin)});
              });
              window.studioExportChunk(payload);
            });
            const timer = setTimeout(() => reject(new Error('Source frame did not load')), 30000);
            frame.onload = () => { clearTimeout(timer); resolve(); };
            frame.src = ${JSON.stringify(origin + "/")};
            document.body.append(frame);
          })`);
          source = window.webContents.mainFrame.frames.find(
            (frame) => frame.url === origin + "/",
          );
          if (!source)
            throw new Error("The saved profile frame is unavailable.");
        }
        await window.webContents.debugger.sendCommand("Runtime.addBinding", {
          name: "studioExportChunk",
        });
        const snapshot = await source.executeJavaScript(`(async () => {
          window.__studioExportReplies = new Map();
          const parentOrigin = ${JSON.stringify(contexts[origin] || null)};
          if (parentOrigin) addEventListener('message', event => {
            if (event.source === parent && event.origin === parentOrigin && event.data?.kind === 'profile-export-reply')
              window.__studioExportReplies.get(event.data.id)?.(event.data.result);
          });
          let next = 0;
          const emit = records => new Promise((resolve, reject) => {
            const id = ++next;
            const timer = setTimeout(() => {
              window.__studioExportReplies.delete(id);
              reject(new Error('Saving a profile chunk timed out'));
            }, 120000);
            window.__studioExportReplies.set(id, result => {
              clearTimeout(timer);
              window.__studioExportReplies.delete(id);
              if (result.error) reject(new Error(result.error)); else resolve(result.value);
            });
            const payload = JSON.stringify({id, records});
            if (parentOrigin) parent.postMessage({kind:'profile-export-chunk', id, payload}, parentOrigin);
            else window.studioExportChunk(payload);
          });
          return (${exportBrowserProfile.toString()})(emit);
        })()`);
        if (contexts[origin]) snapshot.topLevelOrigin = contexts[origin];
        snapshots.push(snapshot);
        console.error("Export source complete");
      } finally {
        window.destroy();
      }
    }
    await exportServerCredentials({
      profile,
      destination: input.nativeCredentials,
      safeStorage,
    });
    const bytes = JSON.stringify({
      version: 2,
      parts: partsName,
      id,
      profile,
      created: Date.now(),
      snapshots,
      owners: destinations,
    });
    outputTemporary = output + "." + id + ".tmp";
    const file = await open(outputTemporary, "wx", 0o600);
    try {
      await file.writeFile(bytes);
      await file.sync();
    } finally {
      await file.close();
    }
    // Hard-link publication is atomic and refuses to replace an existing export.
    await link(outputTemporary, output);
    published = true;
    writeSync(
      1,
      JSON.stringify({
        result: "PASS",
        output,
        origins: snapshots.length,
        id,
      }) + "\n",
    );
    exitCode = 0;
  } catch (error) {
    writeSync(
      2,
      (error instanceof Error ? error.message : String(error)) + "\n",
    );
    exitCode = 1;
  } finally {
    if (partsDirectory && !published)
      await rm(partsDirectory, { recursive: true, force: true });
    if (outputTemporary) await unlink(outputTemporary).catch(() => {});
    app.exit(exitCode);
  }
}
void main();
