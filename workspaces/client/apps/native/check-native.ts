// Exercise the native host through hidden WebViews and disposable fixtures.
import type { BackendIdentity } from "../desktop/backend.ts";
import type { components } from "../web/src/generated/api.ts";
import type { BrowserProfileArchive } from "../web/src/native/importBrowserProfile.ts";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import {
  mkdtemp,
  mkdir,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import type { IncomingHttpHeaders, OutgoingHttpHeaders } from "node:http";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);
const BINARY = path.join(ROOT, "target/debug/studio-native-client");
const SCENARIO_TIMEOUT_MS = 60_000;
const ERROR_TAIL_CHARACTERS = 4_000;

async function run(
  directory: string,
  mode: string,
  extra: NodeJS.ProcessEnv = {},
) {
  const report = path.join(directory, "webview-result.json");
  await rm(report, { force: true });
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    STUDIO_NATIVE_CHECK_DIR: directory,
    STUDIO_NATIVE_CHECK_MODE: mode,
    STUDIO_NATIVE_CHECK_NETWORK: "0",
    CODEX_AGENTS_SUPERVISOR_MODE: "0",
  };
  for (const key of [
    "CODEX_DESKTOP_PROFILE",
    "CODEX_DESKTOP_EPHEMERAL_PROFILE",
    "CODEX_NATIVE_PROFILE_IMPORT",
    "STUDIO_NATIVE_CHECK_STORE_ID",
  ])
    delete env[key];
  Object.assign(env, extra);
  const child = spawn(BINARY, ["--hidden", "--attar-check-scenario"], {
    env,
    stdio: ["ignore", "ignore", "pipe"],
  });
  let stderr = "";
  child.stderr.setEncoding("utf8").on("data", (data) => {
    stderr += data;
  });
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    child.kill("SIGKILL");
  }, SCENARIO_TIMEOUT_MS);
  let code;
  try {
    [code] = await once(child, "close");
  } finally {
    clearTimeout(timer);
  }
  let result;
  try {
    result = JSON.parse(await readFile(report, "utf8"));
  } catch (error) {
    throw new Error(
      `Native ${mode} ${timedOut ? "timed out" : `failed (${code})`}: ${stderr.slice(-ERROR_TAIL_CHARACTERS)}`,
      { cause: error },
    );
  }
  assert.equal(
    timedOut,
    false,
    `Native ${mode} timed out: ${stderr.slice(-ERROR_TAIL_CHARACTERS)}`,
  );
  assert.equal(
    code,
    0,
    `Native ${mode} failed: ${stderr.slice(-ERROR_TAIL_CHARACTERS)}`,
  );
  assert.deepEqual(result, { passed: true, mode });
  for (const line of stderr.split("\n"))
    if (line.startsWith("Hidden native step:")) console.log(line);
  console.log(`Hidden WebView ${mode}: passed`);
}

export async function createFixture(directory: string) {
  const commands: { headers: IncomingHttpHeaders; body: Buffer }[] = [];
  let identityState = path.join(directory, "wrong-state");
  const server = createServer(async (request, response) => {
    const send = (
      status: number,
      body: string | Buffer,
      headers: OutgoingHttpHeaders = {},
    ) => {
      const bytes = Buffer.isBuffer(body) ? body : Buffer.from(body);
      response.writeHead(status, {
        "Content-Length": bytes.length,
        Connection: "close",
        ...headers,
      });
      response.end(bytes);
    };
    const json = (body: unknown) =>
      send(200, JSON.stringify(body), { "Content-Type": "application/json" });
    if (request.method === "GET") {
      if (request.url === "/api/native-check-events")
        return send(
          200,
          "id: rev-1\nevent: updated\ndata: Привет 🦀\ndata: second\n\n",
          { "Content-Type": "text/event-stream" },
        );
      if (request.url === "/api/sync/identity")
        return json({
          workspaceId: "a".repeat(32),
          chatState: true,
        } satisfies Pick<
          components["schemas"]["SyncIdentityResponse"],
          "workspaceId"
        > &
          Record<string, unknown>);
      if (request.url?.startsWith("/api/sync/pull?scope=drafts&"))
        return json({
          workspaceId: "a".repeat(32),
          documents: [],
          checkpoint: { seq: 0 },
        } satisfies Pick<
          components["schemas"]["SyncPullResponse"],
          "workspaceId" | "documents" | "checkpoint"
        >);
      if (request.url === "/api/desktop")
        return json({
          application: "codex-agents",
          protocol: 1,
          backendKind: "rust",
          pid: process.pid,
          stateDir: identityState,
          backendBuild: "a".repeat(64),
        } satisfies BackendIdentity);
      if (request.url === "/api/session")
        return json({
          token: "fixture-native-token",
        } satisfies components["schemas"]["SessionResponse"]);
      return send(404, '{"error":"Unknown fixture path"}');
    }
    if (request.method === "POST") {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = Buffer.concat(chunks);
      if (request.url === "/api/sync/drafts") {
        const { rows } = JSON.parse(body.toString());
        for (const row of rows)
          for (const field of ["newDocumentState", "assumedMasterState"]) {
            const document = row[field];
            if (
              document != null &&
              (Object.hasOwn(document, "_deleted") ||
                typeof document.deleted !== "boolean")
            )
              return send(400, '{"error":"Invalid draft wire tombstone"}');
          }
        return send(200, "[]");
      }
      if (request.url !== "/api/native-check") return send(404, "{}");
      commands.push({ headers: request.headers, body });
      return send(409, Buffer.from([97, 0, 255]), {
        "X-Studio-API-Schema-Mismatch": "1",
      });
    }
    send(404, "{}");
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  return {
    port: address.port,
    commands,
    acceptIdentity() {
      identityState = directory;
    },
    close() {
      return new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
    },
  };
}

async function main() {
  // The client canonicalizes its state folder (macOS /var is /private/var), and
  // the fixture identity must name the same folder.
  const directory = await realpath(
    await mkdtemp(path.join(tmpdir(), "studio-native-webview-")),
  );
  try {
    for (const [profile, mode] of [
      ["store-a", "profile-store-write"],
      ["store-b", "profile-store-empty"],
      ["store-a", "profile-store-read"],
    ] as const) {
      const target = path.join(directory, profile);
      await run(target, mode, { CODEX_DESKTOP_PROFILE: target });
    }
    const archive = path.join(directory, "profile.json");
    const combined: BrowserProfileArchive = {
      version: 1,
      id: "fixture-profile",
      owners: {
        root: "http://127.0.0.1:4620",
      },
      snapshots: [
        {
          version: 1,
          origin: "http://127.0.0.1:4620",
          preferences: {
            migrated: "saved Ж🦀",
          },
          databases: [
            {
              name: "migration-fixture",
              version: 1,
              stores: [
                {
                  name: "records",
                  keyPath: "id",
                  autoIncrement: false,
                  indexes: [],
                  records: [
                    [
                      ["value", "voice"],
                      [
                        "object",
                        [
                          ["id", ["value", "voice"]],
                          [
                            "pcm",
                            ["view", "Int16Array", ["bytes", "AIAAAP9/"]],
                          ],
                          [
                            "file",
                            [
                              "file",
                              "bytes.bin",
                              "application/octet-stream",
                              1234,
                              ["bytes", "AP8q"],
                            ],
                          ],
                        ],
                      ],
                    ],
                  ],
                },
                {
                  name: "changes",
                  keyPath: null,
                  autoIncrement: true,
                  nextKey: 101,
                  indexes: [],
                  records: [],
                },
              ],
            },
          ],
        },
      ],
    };
    combined.owners["server:remote"] = "http://studio-remote.localhost:4623";
    combined.snapshots.push({
      version: 1,
      origin: "http://studio-remote.localhost:4623",
      preferences: { ":server:remote:draft": "remote draft survives" },
      databases: [
        {
          name: "migration-fixture:server:remote",
          version: 1,
          stores: [
            {
              name: "records",
              keyPath: null,
              autoIncrement: false,
              indexes: [],
              records: [
                [
                  ["value", "voice"],
                  ["value", "remote bytes"],
                ],
              ],
            },
          ],
        },
      ],
    });
    await writeFile(archive, JSON.stringify(combined));
    const storeId = randomUUID().replaceAll("-", "");
    console.log(`Isolated WebKit profile store: ${storeId}`);
    await run(path.join(directory, "profile-client"), "profile-import", {
      CODEX_NATIVE_PROFILE_IMPORT: archive,
      STUDIO_NATIVE_CHECK_STORE_ID: storeId,
    });
    combined.version = 2;
    combined.parts = "profile.parts";
    const parts = path.join(directory, combined.parts);
    await mkdir(parts, { mode: 0o700 });
    for (const snapshot of combined.snapshots) {
      snapshot.version = 2;
      for (const database of snapshot.databases)
        for (const store of database.stores) {
          const records = store.records;
          assert.ok(records);
          delete store.records;
          store.chunks = [];
          if (records.length) {
            const data = Buffer.from(JSON.stringify(records));
            const digest = createHash("sha256").update(data).digest("hex");
            await writeFile(path.join(parts, `${digest}.json`), data);
            store.chunks.push({
              sha256: digest,
              bytes: data.length,
              records: records.length,
            });
          }
        }
    }
    await writeFile(archive, JSON.stringify(combined));
    await run(path.join(directory, "chunk-profile-client"), "profile-import", {
      CODEX_NATIVE_PROFILE_IMPORT: archive,
      STUDIO_NATIVE_CHECK_STORE_ID: randomUUID().replaceAll("-", ""),
    });
    for (const mode of ["write", "read", "frames"]) await run(directory, mode);
    const fixture = await createFixture(directory);
    try {
      const env: NodeJS.ProcessEnv = {
        CODEX_AGENTS_STATE_DIR: directory,
        CODEX_DESKTOP_PORT: String(fixture.port),
        STUDIO_NATIVE_CHECK_NETWORK: "1",
      };
      await run(directory, "network-invalid", env);
      assert.equal(
        fixture.commands.length,
        0,
        "An invalid backend received a command",
      );
      fixture.acceptIdentity();
      await run(directory, "network", env);
      await run(path.join(directory, "shell-client"), "network-shell", env);
      for (const mode of [
        "network-stream",
        "network-drafts-write",
        "network-drafts-read",
      ])
        await run(directory, mode, env);
      for (const mode of ["drafts-offline-write", "drafts-offline-read"])
        await run(directory, mode);
      assert.equal(
        fixture.commands.length,
        2,
        "Expected two distinct commands",
      );
      const [command, shell] = fixture.commands;
      assert.ok(command && shell);
      assert.equal(shell.body.toString(), "null");
      assert.equal(shell.headers["x-studio-request-id"], "shell-http-command");
      assert.equal(command.body.toString(), "null");
      assert.equal(command.headers["x-canvas-token"], "fixture-native-token");
      assert.equal(
        command.headers["x-studio-request-id"],
        "original-http-command",
      );
      assert.equal(
        command.headers["x-studio-api-schema"],
        "fixture-client-schema",
      );
      assert.equal(command.headers["x-canvas-workspace"], "fixture-workspace");
    } finally {
      await fixture.close();
    }
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href
) {
  await main();
}
