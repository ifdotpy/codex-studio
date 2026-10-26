import { canonicalJson, isRecord } from "./move.ts";
import type {
  McpServerStatus,
  McpServerStatusConfig,
  McpClaudeAIProxyServerConfig,
} from "@anthropic-ai/claude-agent-sdk";
import type { Tool } from "@modelcontextprotocol/sdk/types.js";
import type { ToolProof, ProofTool } from "./move.ts";
export type ExternalConfig = Exclude<McpServerStatusConfig, { type: "sdk" }>;
type LocalConfig = Exclude<ExternalConfig, McpClaudeAIProxyServerConfig>;
export type ExternalServer = {
  name: string;
  alias: string;
  configHash: string;
  tools: ProofTool[];
  claudeAI?: boolean;
  proxyId?: string;
};
type Status = Pick<McpServerStatus, "name"> & {
  status: string;
  source?: string;
  config?: McpServerStatus["config"];
};
export type CatalogDependencies = {
  localConfigs?: (cwd: string) => Promise<Record<string, unknown>>;
  listTools?: (config: ExternalConfig) => Promise<Partial<Tool>[]>;
  proxyToken?: () => Promise<string>;
  transports?: Record<string, LocalConfig>;
};
type CatalogResult = {
  catalogs: ExternalServer[];
  configs: Record<string, ExternalConfig>;
  transports: Record<string, LocalConfig>;
  accountConnectors?: boolean;
};
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
const hash = (value: unknown) =>
  createHash("sha256").update(JSON.stringify(value)).digest("hex");
export { canonicalJson } from "./move.ts";
export const serverAlias = (name: string) =>
  name.replace(/[^a-zA-Z0-9_-]/g, "_");
const safeName = (name: string) =>
  /^[A-Za-z0-9_.:+ -]{1,128}$/.test(name) ? name : "<unavailable>";
export function externalRefusal(
  source: string[],
  target: string[] = [],
  changed: string[] = [],
) {
  const names = (items: string[]) => items.map(safeName);
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
export function externalAliases(proof: ToolProof) {
  const names =
    proof.inlineTools === false
      ? proof.surfacedExternal
        ? (proof.deferredTools || []).map((tool) => tool.name)
        : proof.activeDeferredNames || []
      : (proof.tools ?? []).map((tool) => tool.name);
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
// User-authored MCP files have no generated wire contract. Narrow their transport
// fields once, retaining the complete config (including credentials) locally.
function parseExternalConfig(value: unknown): ExternalConfig {
  if (!isRecord(value)) throw new Error("Unsupported MCP transport");
  if (value.type === undefined || value.type === "stdio") {
    if (
      typeof value.command !== "string" ||
      (value.args !== undefined &&
        (!Array.isArray(value.args) ||
          !value.args.every((arg: unknown) => typeof arg === "string"))) ||
      (value.env !== undefined &&
        (!isRecord(value.env) ||
          !Object.values(value.env).every(
            (entry) => typeof entry === "string",
          )))
    )
      throw new Error("Unsupported MCP transport");
  } else if (value.type === "http" || value.type === "sse") {
    if (
      typeof value.url !== "string" ||
      (value.headers !== undefined &&
        (!isRecord(value.headers) ||
          !Object.values(value.headers).every(
            (entry) => typeof entry === "string",
          )))
    )
      throw new Error("Unsupported MCP transport");
  } else if (
    value.type !== "claudeai-proxy" ||
    typeof value.url !== "string" ||
    typeof value.id !== "string"
  )
    throw new Error("Unsupported MCP transport");
  return value as ExternalConfig;
}
async function localConfigs(cwd: string) {
  const configured = process.env.CLAUDE_CONFIG_DIR;
  const user = configured
    ? path.join(configured, ".claude.json")
    : path.join(os.homedir(), ".claude.json");
  const read = async (file: string) => {
    try {
      const settings: unknown = JSON.parse(await fs.readFile(file, "utf8"));
      return settings;
    } catch (error) {
      if (
        (error && typeof error === "object" && "code" in error
          ? error.code
          : undefined) === "ENOENT"
      )
        return {};
      throw error;
    }
  };
  const settings = await read(user);
  const project = await read(path.join(cwd, ".mcp.json"));
  const projects =
    isRecord(settings) && isRecord(settings.projects) ? settings.projects : {};
  const current = projects[cwd];
  const combined = {
    ...(isRecord(settings) && isRecord(settings.mcpServers)
      ? settings.mcpServers
      : {}),
    ...(isRecord(project) && isRecord(project.mcpServers)
      ? project.mcpServers
      : {}),
    ...(isRecord(current) && isRecord(current.mcpServers)
      ? current.mcpServers
      : {}),
  };
  return combined;
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
  const credentials: unknown = JSON.parse(bytes);
  const token = isRecord(credentials) ? credentials.claudeAiOauth : undefined;
  if (
    !isRecord(token) ||
    typeof token.accessToken !== "string" ||
    !token.accessToken ||
    typeof token.expiresAt !== "number" ||
    !Number.isFinite(token.expiresAt) ||
    token.expiresAt <= Date.now() + 30000
  )
    throw new Error("The local Claude proxy credential is unavailable");
  return token.accessToken;
}
async function readExternalServer(
  config: ExternalConfig,
  initializeOnly = false,
) {
  const client = new Client({ name: "studio-teleport-catalog", version: "1" });
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 15000);
  timer.unref();
  let transport;
  try {
    if (config.type === "stdio" || config.type === undefined) {
      transport = new StdioClientTransport({
        command: config.command,
        args: config.args || [],
        env: Object.fromEntries(
          Object.entries({ ...process.env, ...config.env }).filter(
            (entry): entry is [string, string] => typeof entry[1] === "string",
          ),
        ),
        ...("cwd" in config && typeof config.cwd === "string"
          ? { cwd: config.cwd }
          : {}),
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
          : "url" in config
            ? config.url
            : "",
      );
      const headers = proxy
        ? {
            Authorization: "Bearer " + (await proxyToken()),
            "Accept-Encoding": "identity",
            "X-Mcp-Client-Session-Id": randomUUID(),
          }
        : "headers" in config
          ? config.headers
          : undefined;
      const options = {
        requestInit: headers === undefined ? {} : { headers },
        fetch: (url: string | URL | Request, init?: RequestInit) =>
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
    const connection: Omit<typeof transport, "sessionId"> = transport;
    await client.connect(connection, {
      timeout: 10000,
      signal: controller.signal,
    });
    const instructions = client.getInstructions() ?? null;
    if (initializeOnly || !client.getServerCapabilities()?.tools)
      return { tools: [], instructions };
    const tools: Tool[] = [];
    let cursor: string | undefined;
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
export async function listExternalTools(config: ExternalConfig) {
  return (await readExternalServer(config)).tools;
}
export async function readExternalInstructions(config: ExternalConfig) {
  return (await readExternalServer(config, true)).instructions;
}
// The transport of an MCP server: what Claude Code starts or connects to. Auth headers and
// the process environment stay local, so they are not part of it.
export function transportIdentity(config: LocalConfig): LocalConfig;
export function transportIdentity(
  config: ExternalConfig,
): LocalConfig | Pick<McpClaudeAIProxyServerConfig, "type" | "id">;
export function transportIdentity(
  config: ExternalConfig,
): Record<string, unknown> {
  return Object.fromEntries(
    (config.type === "claudeai-proxy"
      ? ["type", "id"]
      : ["type", "command", "args", "url"]
    )
      .filter((key) => Reflect.get(config, key) !== undefined)
      .map((key) => [key, Reflect.get(config, key)]),
  );
}

const credentialFree = (config: ExternalConfig): config is LocalConfig =>
  config.type !== "claudeai-proxy" &&
  !Object.keys("env" in config ? config.env || {} : {}).length &&
  !Object.keys("headers" in config ? config.headers || {} : {}).length;
// A moved chat keeps the MCP servers of its source. The target's own entry serves when it has
// the same transport; otherwise the source's transport, which carries no credential, starts
// the same server on the target.
function movedConfig(
  server: ExternalServer,
  own: unknown,
  source: ExternalConfig | undefined,
) {
  if (own) {
    const config = parseExternalConfig(own);
    if (hash(transportIdentity(config)) === server.configHash) return config;
  }
  if (
    source &&
    credentialFree(source) &&
    hash(transportIdentity(source)) === server.configHash
  )
    return transportIdentity(source);
  return own ? parseExternalConfig(own) : undefined;
}
export async function externalCatalog(
  statuses: Status[],
  cwd: string,
  aliases: string[],
  dependencies: CatalogDependencies = {},
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
    const value =
      row.config.type === "claudeai-proxy" ? row.config : local[row.name];
    const config = value ? parseExternalConfig(value) : undefined;
    if (!config) throw externalRefusal(aliases, [row.name], [row.name]);
    // Keep auth headers and process environment local. Compare the transport
    // identity and the actual ordered tools, rather than credential values.
    return {
      name: row.name,
      alias,
      config,
      configHash: hash(transportIdentity(config)),
    };
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
  const catalogs = results.flatMap((result) =>
    result.status === "fulfilled" ? [result.value] : [],
  );
  return {
    catalogs,
    configs: Object.fromEntries(
      selected.map((server) => [server.name, server.config]),
    ),
    // What a move carries so that the target starts the same servers.
    transports: Object.fromEntries(
      selected.flatMap((server) =>
        credentialFree(server.config)
          ? [[server.name, transportIdentity(server.config)]]
          : [],
      ),
    ),
  };
}
export function verifyExternalCatalog(
  source: ExternalServer[],
  target: ExternalServer[],
) {
  if (canonicalJson(source) === canonicalJson(target)) return;
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
      (server, index) => canonicalJson(server) !== canonicalJson(target[index]),
    )
    .map((server) => server.name);
  throw externalRefusal(sourceNames, targetNames, changed);
}

// Target metadata comes only from the required local MCP entries and the same
// account's explicitly selected cloud connector ids. Do not load target settings.
export async function targetExternalCatalog(
  cwd: string,
  expected: ExternalServer[],
  dependencies: CatalogDependencies = {},
): Promise<CatalogResult> {
  const local = await (dependencies.localConfigs || localConfigs)(cwd);
  const transports = dependencies.transports || {};
  // claude.ai connectors belong to the account, and Studio moves a Claude chat only to the same
  // account and organization. A Mac without a person at it cannot open the account's Keychain
  // entry for the connector proxy, while its CLI loads the connectors itself; there the
  // source's connector catalog stands.
  if (
    expected.some((server) => server.claudeAI) &&
    !(await (dependencies.proxyToken || proxyToken)().then(
      () => true,
      () => false,
    ))
  ) {
    const own = expected.filter((server) => !server.claudeAI);
    const read = own.length
      ? await targetExternalCatalog(cwd, own, {
          ...dependencies,
          localConfigs: async () => local,
        })
      : { catalogs: [], configs: {}, transports: {} };
    const catalogs = expected.map((server) =>
      server.claudeAI
        ? structuredClone(server)
        : (read.catalogs.find((row) => row.name === server.name) ?? server),
    );
    const configs = { ...read.configs };
    for (const server of expected.filter((row) => row.claudeAI))
      configs[server.name] = {
        type: "claudeai-proxy",
        id: server.proxyId ?? "",
        url: "https://mcp-proxy.anthropic.com/v1/mcp/" + server.proxyId,
      };
    return {
      catalogs,
      configs,
      transports: read.transports,
      accountConnectors: true,
    };
  }
  const configured = { ...local };
  for (const server of expected.filter((row) => !row.claudeAI)) {
    const config = movedConfig(
      server,
      local[server.name],
      transports[server.name],
    );
    if (config) configured[server.name] = config;
  }
  const statuses: Status[] = expected.map((server) => ({
    name: server.name,
    source: server.claudeAI ? "claudeai" : "user",
    status: "connected",
    config: server.claudeAI
      ? {
          type: "claudeai-proxy",
          id: server.proxyId ?? "",
          url: "https://mcp-proxy.anthropic.com/v1/mcp/" + server.proxyId,
        }
      : configured[server.name]
        ? parseExternalConfig(configured[server.name])
        : undefined,
  }));
  const result = await externalCatalog(
    statuses,
    cwd,
    expected.map((server) => server.alias),
    { ...dependencies, localConfigs: async () => configured },
  );
  verifyExternalCatalog(expected, result.catalogs);
  return result;
}
