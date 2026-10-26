import type { ChunkReply } from "./profile-export-reader.ts";
import {
  loadSourceFrame,
  captureSourceProfile,
} from "./profile-export-reader.ts";
import type { WebContents, WebFrameMain } from "electron";
import type {
  BrowserProfileSnapshot,
  BrowserProfileArchive,
} from "../web/src/native/importBrowserProfile.ts";
import { app, BrowserWindow, session, safeStorage } from "electron";
import { execFileSync } from "node:child_process";
import { open, link, unlink, mkdir, rm } from "node:fs/promises";
import { readFileSync, realpathSync, writeSync } from "node:fs";
import { randomUUID, createHash } from "node:crypto";
import path from "node:path";
import { exportServerCredentials } from "./export-server-credentials.ts";
import { exportBrowserProfile } from "./export-browser-profile.ts";

export type { BrowserProfileArchive } from "../web/src/native/importBrowserProfile.ts";
export type ExportManifest = {
  profile: string;
  output: string;
  origins: string[];
  contexts?: Record<string, string>;
  owners?: Record<string, string>;
  nativeCredentials?: Parameters<
    typeof exportServerCredentials
  >[0]["destination"];
};

// Explicit origins are required: an offline profile can contain several old
// frame ports. The migration caller must enumerate them before importing.
async function main() {
  const manifestPath = process.argv[2];
  let outputTemporary;
  let partsDirectory: string | undefined;
  let published = false;
  let exitCode = 1;
  try {
    if (!manifestPath)
      throw new Error("Supply an offline profile export manifest.");
    const input: unknown = JSON.parse(readFileSync(manifestPath, "utf8"));
    if (
      !input ||
      typeof input !== "object" ||
      !("profile" in input) ||
      typeof input.profile !== "string" ||
      !("output" in input) ||
      typeof input.output !== "string"
    )
      throw new Error("Supply an offline profile export manifest.");
    const profile = realpathSync(input.profile);
    const output = path.resolve(input.output);
    if (output === profile || output.startsWith(profile + path.sep))
      throw new Error("Export outside the original profile.");
    if (
      !("origins" in input) ||
      !Array.isArray(input.origins) ||
      !input.origins.length
    )
      throw new Error("Supply every saved Studio browser origin.");
    const origins = [
      ...new Set(
        input.origins.map((value: unknown) => {
          if (typeof value !== "string")
            throw new Error("Only saved local Studio origins can be exported.");
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
    const sourceContexts = "contexts" in input ? (input.contexts ?? {}) : {};
    if (
      typeof sourceContexts !== "object" ||
      Array.isArray(sourceContexts) ||
      Object.entries(sourceContexts).some(
        ([origin, parent]) =>
          !origins.includes(origin) ||
          typeof parent !== "string" ||
          !origins.includes(parent) ||
          origin === parent,
      )
    )
      throw new Error("Invalid source frame context.");
    const contexts = sourceContexts as Record<string, string>;
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
      if (
        !(error instanceof Error) ||
        !("status" in error) ||
        error.status !== 1 ||
        String("stderr" in error ? error.stderr || "" : "").trim()
      )
        throw error;
      // lsof can report matching owners and still exit 1 when another lookup
      // in the recursive scan fails. Its stdout remains evidence of use.
      owners = String("stdout" in error ? error.stdout || "" : "").trim();
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
    const rawDestinations = "owners" in input ? (input.owners ?? {}) : {};
    if (
      typeof rawDestinations !== "object" ||
      Array.isArray(rawDestinations) ||
      Object.entries(rawDestinations).some(
        ([owner, origin]) =>
          !(owner === "root" || /^server:[a-zA-Z0-9_-]+$/.test(owner)) ||
          typeof origin !== "string" ||
          !origins.includes(origin),
      )
    )
      throw new Error("Invalid destination owner mapping.");
    const destinations = rawDestinations as Record<string, string>;
    const id = randomUUID();
    const partsName = path.basename(output) + "." + id + ".parts";
    partsDirectory = path.join(path.dirname(output), partsName);
    await mkdir(partsDirectory, { mode: 0o700 });
    const parts = partsDirectory;
    const snapshots: BrowserProfileSnapshot[] = [];
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
        let failTransfer: (reason: unknown) => void = () => {};
        const transferFailed = new Promise<never>((_, reject) => {
          failTransfer = reject;
        });
        // Observe failures that arrive while the source document is loading.
        void transferFailed.catch(() => {});
        window.webContents.debugger.on(
          "message",
          async (_event, method, params: unknown, sessionId) => {
            if (
              method !== "Runtime.bindingCalled" ||
              !params ||
              typeof params !== "object" ||
              !("name" in params) ||
              params.name !== "studioExportChunk"
            )
              return;
            let requestId: number | undefined;
            let result: ChunkReply;
            try {
              if (!("payload" in params) || typeof params.payload !== "string")
                throw new Error("Invalid migration chunk.");
              if (Buffer.byteLength(params.payload) > 64 * 1024 * 1024 + 1024)
                throw new Error("Migration chunk is too large.");
              const payload: unknown = JSON.parse(params.payload);
              if (!payload || typeof payload !== "object" || !("id" in payload))
                throw new Error("Invalid migration chunk.");
              requestId =
                typeof payload.id === "number" ? payload.id : undefined;
              if (
                !payload ||
                typeof payload !== "object" ||
                !("id" in payload) ||
                typeof payload.id !== "number" ||
                !Number.isSafeInteger(payload.id) ||
                !("records" in payload) ||
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
                  path.join(parts, sha256 + ".json"),
                  "wx",
                  0o600,
                );
              } catch (error) {
                if (
                  !(
                    error instanceof Error &&
                    "code" in error &&
                    error.code === "EEXIST"
                  )
                )
                  throw error;
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
                  contextId:
                    "executionContextId" in params
                      ? params.executionContextId
                      : undefined,
                },
                // Electron reports the root session as an empty string, but
                // sendCommand accepts only an omitted id for that session.
                sessionId || undefined,
              )
              .then((response: unknown) => {
                if (
                  response &&
                  typeof response === "object" &&
                  "exceptionDetails" in response &&
                  response.exceptionDetails
                )
                  throw new Error(
                    "The profile reader rejected a chunk acknowledgement.",
                  );
              })
              .catch(failTransfer);
          },
        );
        await window.loadURL((contexts[origin] || origin) + "/");
        let source: WebContents | WebFrameMain | undefined = window.webContents;
        if (contexts[origin]) {
          await window.webContents.executeJavaScript(
            `(${loadSourceFrame.toString()})(${JSON.stringify(origin)})`,
          );
          source = window.webContents.mainFrame.frames.find(
            (frame) => frame.url === origin + "/",
          );
          if (!source)
            throw new Error("The saved profile frame is unavailable.");
        }
        await window.webContents.debugger.sendCommand("Runtime.addBinding", {
          name: "studioExportChunk",
        });
        const snapshot: BrowserProfileSnapshot = await Promise.race([
          transferFailed,
          source.executeJavaScript(
            `(${captureSourceProfile.toString()})(${JSON.stringify(contexts[origin] || null)},(${exportBrowserProfile.toString()}))`,
          ),
        ]);
        if (contexts[origin]) snapshot.topLevelOrigin = contexts[origin];
        snapshots.push(snapshot);
        console.error("Export source complete");
      } finally {
        window.destroy();
      }
    }
    await exportServerCredentials({
      profile,
      destination:
        "nativeCredentials" in input ? input.nativeCredentials : undefined,
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
    } satisfies BrowserProfileArchive);
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
