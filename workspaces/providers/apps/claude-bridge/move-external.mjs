// Teleport catalog proof uses only MCP metadata requests. Credentials stay local.
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { ListToolsResultSchema } from "@modelcontextprotocol/sdk/types.js";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { SSEClientTransport } from "@modelcontextprotocol/sdk/client/sse.js";
const run = promisify(execFile);
const hash = (value) =>
  createHash("sha256").update(JSON.stringify(value)).digest("hex");
export const serverAlias = (name) => name.replace(/[^a-zA-Z0-9_-]/g, "_");
const safeName = (name) =>
  /^[A-Za-z0-9_.:+ -]{1,128}$/.test(name) ? name : "<unavailable>";
export function externalRefusal(source, target = [], changed = []) {
  const names = (items) => items.map(safeName);
  const detail = {
    kind: "external_tools",
    sourceNames: names(source),
    targetNames: names(target),
    changedNames: names(changed),
  };
  return Object.assign(
    new Error(
      "The external MCP catalog is unavailable or differs. Source names [" +
        detail.sourceNames.join(", ") +
        "]; target names [" +
        detail.targetNames.join(", ") +
        "]; changed definitions [" +
        detail.changedNames.join(", ") +
        "]",
    ),
    { data: { moveRefusal: detail } },
  );
}
export function externalAliases(proof) {
  const names =
    proof.inlineTools === false
      ? proof.activeDeferredNames || []
      : proof.tools.map((tool) => tool.name);
  return [
    ...new Set(
      names
        .filter(
          (name) =>
            name.startsWith("mcp__") && !name.startsWith("mcp__studio__"),
        )
        .map((name) => name.slice(5, name.lastIndexOf("__"))),
    ),
  ];
}
async function localConfigs(cwd) {
  const configured = process.env.CLAUDE_CONFIG_DIR;
  const user = configured
    ? path.join(configured, ".claude.json")
    : path.join(os.homedir(), ".claude.json");
  const read = async (file) => {
    try {
      return JSON.parse(await fs.readFile(file, "utf8"));
    } catch (error) {
      if (error.code === "ENOENT") return {};
      throw error;
    }
  };
  const settings = await read(user);
  const project = await read(path.join(cwd, ".mcp.json"));
  return {
    ...settings.mcpServers,
    ...project.mcpServers,
    ...settings.projects?.[cwd]?.mcpServers,
  };
}
async function proxyToken() {
  const configured = process.env.CLAUDE_CONFIG_DIR;
  let bytes;
  if (process.platform === "darwin") {
    const suffix = configured
      ? "-" +
        createHash("sha256")
          .update(configured.replace(/\/$/, ""))
          .digest("hex")
          .slice(0, 8)
      : "";
    try {
      bytes = (
        await run(
          "security",
          [
            "find-generic-password",
            "-s",
            "Claude Code-credentials" + suffix,
            "-w",
          ],
          { timeout: 10000, maxBuffer: 1024 * 1024 },
        )
      ).stdout;
    } catch {
      /* Try the CLI credential file. */
    }
  }
  bytes ??= await fs.readFile(
    path.join(
      configured || path.join(os.homedir(), ".claude"),
      ".credentials.json",
    ),
    "utf8",
  );
  const token = JSON.parse(bytes).claudeAiOauth;
  if (
    !token?.accessToken ||
    !Number.isFinite(token.expiresAt) ||
    token.expiresAt <= Date.now() + 30000
  )
    throw new Error("The local Claude proxy credential is unavailable");
  return token.accessToken;
}
async function readExternalServer(config, initializeOnly = false) {
  const client = new Client({ name: "studio-teleport-catalog", version: "1" });
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 15000);
  timer.unref();
  let transport;
  try {
    if (config.type === "stdio" || !config.type) {
      transport = new StdioClientTransport({
        command: config.command,
        args: config.args || [],
        env: { ...process.env, ...config.env },
        cwd: config.cwd,
        stderr: "ignore",
      });
    } else {
      const proxy = config.type === "claudeai-proxy";
      if (!["http", "sse", "claudeai-proxy"].includes(config.type))
        throw new Error("Unsupported MCP transport");
      if (proxy && !/^mcp(?:srv|rs)_[A-Za-z0-9_-]+$/.test(config.id))
        throw new Error("Invalid Claude proxy identity");
      const url = new URL(
        proxy
          ? "https://mcp-proxy.anthropic.com/v1/mcp/" + config.id
          : config.url,
      );
      const headers = proxy
        ? {
            Authorization: "Bearer " + (await proxyToken()),
            "Accept-Encoding": "identity",
            "X-Mcp-Client-Session-Id": randomUUID(),
          }
        : config.headers;
      const options = {
        requestInit: { headers },
        fetch: (url, init) =>
          fetch(url, {
            ...init,
            redirect: "error",
            signal: AbortSignal.any([
              controller.signal,
              ...(init?.signal ? [init.signal] : []),
              AbortSignal.timeout(10000),
            ]),
          }),
      };
      transport =
        config.type === "sse"
          ? new SSEClientTransport(url, options)
          : new StreamableHTTPClientTransport(url, options);
    }
    await client.connect(transport, {
      timeout: 10000,
      signal: controller.signal,
    });
    const instructions = client.getInstructions() ?? null;
    if (initializeOnly || !client.getServerCapabilities()?.tools)
      return { tools: [], instructions };
    const tools = [];
    let cursor;
    let pages = 0;
    do {
      if (++pages > 100)
        throw new Error("MCP catalog pagination exceeds its limit");
      // tools/list needs no validators for tools/call output schemas.
      const result = await client.request(
        { method: "tools/list", params: cursor ? { cursor } : {} },
        ListToolsResultSchema,
        { timeout: 10000, signal: controller.signal },
      );
      tools.push(...result.tools);
      if (tools.length > 1000) throw new Error("MCP catalog exceeds its limit");
      if (result.nextCursor && result.nextCursor === cursor)
        throw new Error("MCP catalog cursor did not advance");
      cursor = result.nextCursor;
    } while (cursor);
    return { tools, instructions };
  } finally {
    clearTimeout(timer);
    await client.close();
    await transport?.close();
  }
}
export async function listExternalTools(config) {
  return (await readExternalServer(config)).tools;
}
export async function readExternalInstructions(config) {
  return (await readExternalServer(config, true)).instructions;
}
export async function externalCatalog(
  statuses,
  cwd,
  aliases,
  dependencies = {},
) {
  const local = await (dependencies.localConfigs || localConfigs)(cwd);
  const read = dependencies.listTools || listExternalTools;
  const selected = aliases.map((alias) => {
    const row = statuses.find(
      (entry) => serverAlias(entry.name) === alias && entry.source !== "sdk",
    );
    if (!row || row.status !== "connected" || !row.config)
      throw externalRefusal(
        aliases,
        statuses.map((entry) => entry.name),
        [alias],
      );
    const config =
      row.config.type === "claudeai-proxy" ? row.config : local[row.name];
    if (!config) throw externalRefusal(aliases, [row.name], [row.name]);
    // Keep auth headers and process environment local. Compare the transport
    // identity and the actual ordered tools, rather than credential values.
    const identity = Object.fromEntries(
      (config.type === "claudeai-proxy"
        ? ["type", "id"]
        : ["type", "command", "args", "url"]
      )
        .filter((key) => config[key] !== undefined)
        .map((key) => [key, config[key]]),
    );
    return { name: row.name, alias, config, configHash: hash(identity) };
  });
  const results = await Promise.allSettled(
    selected.map(async (server) => {
      try {
        const raw = await read(server.config);
        const tools = raw.map((entry) => {
          if (
            typeof entry.name !== "string" ||
            !entry.name ||
            (entry.description !== undefined &&
              typeof entry.description !== "string") ||
            !entry.inputSchema ||
            typeof entry.inputSchema !== "object" ||
            Array.isArray(entry.inputSchema)
          )
            throw new Error("Incomplete MCP tool schema");
          return {
            name: "mcp__" + server.alias + "__" + entry.name,
            description: entry.description || "",
            input_schema: entry.inputSchema,
          };
        });
        return {
          name: server.name,
          alias: server.alias,
          configHash: server.configHash,
          ...(server.config.type === "claudeai-proxy"
            ? { claudeAI: true, proxyId: server.config.id }
            : {}),
          tools,
        };
      } catch {
        throw externalRefusal(aliases, [server.name], [server.name]);
      }
    }),
  );
  const failed = results.find((result) => result.status === "rejected");
  if (failed) throw failed.reason;
  const catalogs = results.map((result) => result.value);
  return {
    catalogs,
    configs: Object.fromEntries(
      selected.map((server) => [server.name, server.config]),
    ),
  };
}
export function verifyExternalCatalog(source, target) {
  if (JSON.stringify(source) === JSON.stringify(target)) return;
  const sourceNames = source.flatMap((server) => [
    server.name,
    ...server.tools.map((tool) => tool.name),
  ]);
  const targetNames = target.flatMap((server) => [
    server.name,
    ...server.tools.map((tool) => tool.name),
  ]);
  const changed = source
    .filter(
      (server, index) =>
        JSON.stringify(server) !== JSON.stringify(target[index]),
    )
    .map((server) => server.name);
  throw externalRefusal(sourceNames, targetNames, changed);
}

// Target metadata comes only from the required local MCP entries and the same
// account's explicitly selected cloud connector ids. Do not load target settings.
export async function targetExternalCatalog(cwd, expected, dependencies = {}) {
  const local = await (dependencies.localConfigs || localConfigs)(cwd);
  const statuses = expected.map((server) => ({
    name: server.name,
    source: server.claudeAI ? "claudeai" : "user",
    status: "connected",
    config: server.claudeAI
      ? {
          type: "claudeai-proxy",
          id: server.proxyId,
          url: "https://mcp-proxy.anthropic.com/v1/mcp/" + server.proxyId,
        }
      : local[server.name],
  }));
  const result = await externalCatalog(
    statuses,
    cwd,
    expected.map((server) => server.alias),
    { ...dependencies, localConfigs: async () => local },
  );
  verifyExternalCatalog(expected, result.catalogs);
  return result;
}
