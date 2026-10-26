import { fileURLToPath } from "node:url";
import type { components } from "../../src/generated/api.ts";
import type { PerformanceTranscriptItem as TranscriptWireItem } from "./performance-fixture.ts";
import type { TranscriptPageResponse } from "../playwright.ts";
import { required, transcriptFixtureAgent } from "./performance-fixture.ts";
import { build } from "esbuild";
import { createServer, request as httpRequest } from "node:http";
import { writeFile, appendFile, mkdir } from "node:fs/promises";
import {
  fixture,
  fixtureWorkspaceId,
} from "../servers/multi-server-fixture.ts";
import { entityPullFixtureForRequest } from "../playwright.ts";
// Manual, bounded interaction benchmark. All writes stay in a disposable output directory.
const dir = process.argv[2];
if (!dir?.startsWith("/"))
  throw new Error("Pass an absolute output directory outside the checkout.");
await mkdir(dir, { recursive: true });
const messages: TranscriptWireItem[] = Array.from({ length: 360 }, (_, i) => ({
  id: `msg-${i}`,
  role: i % 2 ? "assistant" : "user",
  text:
    i % 2
      ? `Answer ${i}.\n\n` +
        "A paragraph with **evidence**, a [source](./file.ts), and a clear explanation. ".repeat(
          15,
        ) +
        `\n\n\`\`\`ts\nconst value = ${i};\n\`\`\``
      : `Question ${i}`,
  turnId: `turn-${Math.floor(i / 2)}`,
  turnStatus: "completed",
  ...(i % 2 ? { phase: "final_answer" } : {}),
}));
let instance: Awaited<ReturnType<typeof fixture>>;
const transcript = (
  id: string | null,
  before?: string | null,
): TranscriptPageResponse => {
  const found = instance.snapshot.threads.find((a) => a.id === id);
  const agent = found ? transcriptFixtureAgent(found) : found;
  const all = id === "short" ? messages.slice(0, 4) : messages;
  const end = before
    ? Math.max(
        0,
        all.findIndex((m) => m.id === before),
      )
    : all.length;
  const start = Math.max(0, end - 120);
  return {
    agent,
    items: all.slice(start, end),
    historyVersion: "perf-1",
    truncated: start > 0,
    nextCursor: start ? required(all[start]).id : null,
    nextAfterCursor: end < all.length ? all[end - 1]?.id : null,
    tail: null,
    unavailable: null,
  };
};
instance = await fixture("Performance", false, {
  ...(process.argv[4] ? { dist: process.argv[4] } : {}),
  handle({ url, json, snapshot }) {
    if (url.pathname === "/api/changes") {
      json({ scope: "chat", git: true, files: [], diff: "" } satisfies Pick<
        components["schemas"]["ChangesResponse"],
        "scope" | "git" | "files" | "diff"
      >);
      return true;
    }
    if (url.pathname === "/api/linux-vm/settings") {
      json({
        settings: {
          cpus: 4,
          memoryBytes: 4294967296,
          systemDiskBytes: 17179869184,
          dataDiskBytes: 137438953472,
        },
        state: "running",
        allocatedDiskBytes: 1024,
        error: null,
      } satisfies components["schemas"]["LinuxVMSettingsResponse"]);
      return true;
    }
    if (url.pathname === "/api/desktop") {
      json({
        application: "codex-agents",
        protocol: 1,
        pid: process.pid,
        stateDir: dir,
        backendBuild: "a".repeat(64),
      } satisfies Pick<
        components["schemas"]["DesktopResponse"],
        "application" | "protocol" | "pid" | "stateDir" | "backendBuild"
      >);
      return true;
    }
    if (url.pathname === "/api/session") {
      json({
        token: "local-only",
      } satisfies components["schemas"]["SessionResponse"]);
      return true;
    }
    if (
      url.pathname === "/api/sync/pull" &&
      url.searchParams.get("scope")?.startsWith("transcript:")
    ) {
      const id = url.searchParams.get("scope")?.slice(11) ?? "";
      json({
        workspaceId: fixtureWorkspaceId("Performance"),
        ...entityPullFixtureForRequest(snapshot, url.href, {
          transcript: transcript(id),
          transcriptSeq: 1,
        }),
      });
      return true;
    }
    if (
      url.pathname === "/api/transcript" ||
      url.pathname === "/api/transcript/page"
    ) {
      json(
        transcript(url.searchParams.get("id"), url.searchParams.get("before")),
      );
      return true;
    }
  },
});
const base = required(instance.snapshot.threads[0]);
instance.setChats(
  ["short", "long", "other"].map((id, i) => ({
    ...base,
    id,
    rootId: id,
    threadId: `thread-${id}`,
    name: required(
      ["Short conversation", "Long conversation", "Other conversation"][i],
    ),
  })),
);
const scenario = await build({
  entryPoints: [
    fileURLToPath(new URL("./interaction-scenario.ts", import.meta.url)),
  ],
  bundle: true,
  write: false,
  format: "iife",
  platform: "browser",
});
const server = createServer(async (req, res) => {
  const requestUrl = required(req.url);
  if (requestUrl.startsWith("/api/"))
    await appendFile(
      dir + "/requests.log",
      req.method + " " + requestUrl + "\n",
    );
  if (requestUrl === "/benchmark-result") {
    let body = "";
    for await (const chunk of req) body += chunk;
    await writeFile(dir + "/browser-latest.json", body);
    res.end("ok");
    return;
  }
  if (requestUrl === "/scenario.js") {
    res.setHeader("Content-Type", "text/javascript");
    res.end(required(scenario.outputFiles[0]).contents);
    return;
  }
  const forward = httpRequest(
    instance.origin + requestUrl,
    { method: req.method, headers: req.headers },
    (up) => {
      const headers = { ...up.headers };
      if (typeof headers["content-security-policy"] === "string")
        headers["content-security-policy"] = headers[
          "content-security-policy"
        ].replace("script-src 'self'", "script-src 'self' 'wasm-unsafe-eval'");
      delete headers["content-length"];
      headers["cache-control"] = "no-store";
      if (requestUrl === "/" || requestUrl.startsWith("/?")) {
        let body = "";
        up.on("data", (v) => (body += v));
        up.on("end", () => {
          res.writeHead(required(up.statusCode), headers);
          res.end(
            body.replace(
              "<head>",
              '<head><script defer src="' + "/scenario.js" + '"></script>',
            ),
          );
        });
      } else {
        if (requestUrl.endsWith(".wasm"))
          headers["content-type"] = "application/wasm";
        res.writeHead(required(up.statusCode), headers);
        up.pipe(res);
      }
    },
  );
  forward.on("error", (e) => {
    res.writeHead(502);
    res.end(String(e));
  });
  req.pipe(forward);
  res.on("close", () => forward.destroy());
});
server.listen(Number(process.argv[3] || 0), "127.0.0.1", async () => {
  const address = server.address();
  if (!address || typeof address === "string")
    throw new Error("Missing fixture server address");
  const origin = `http://127.0.0.1:${address.port}`;
  await writeFile(
    dir + "/fixture.json",
    JSON.stringify({ origin, pid: process.pid }),
  );
  console.log(origin);
});
for (const signal of ["SIGTERM", "SIGINT"])
  process.on(signal, async () => {
    server.closeAllConnections();
    server.close();
    await instance.close();
    process.exit(0);
  });
