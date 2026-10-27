import { fileURLToPath } from "node:url";
import type {
  McpServerConfig,
  McpServerStatus,
  McpClaudeAIProxyServerConfig,
  Options,
  Query,
} from "@anthropic-ai/claude-agent-sdk";
import type { ExternalConfig, ExternalServer } from "./move-external.ts";
import type { Tool } from "@modelcontextprotocol/sdk/types.js";
import { test, vi } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  sdkMcpServers,
  externalRefusal,
  externalAliases,
  externalCatalog,
  verifyExternalCatalog,
  targetExternalCatalog,
  listExternalTools,
  readExternalInstructions,
} from "./move-external.ts";

test("SDK boundary preserves local and proxy configurations for both callers", () => {
  const configs = {
    local: { type: "stdio", command: "node", args: ["catalog.mjs"] },
    proxy: {
      type: "claudeai-proxy",
      id: "mcpsrv_fixture",
      url: "https://proxy.example.test/mcp",
    },
  } satisfies Record<string, McpServerConfig | McpClaudeAIProxyServerConfig>;
  const options: Pick<Options, "mcpServers"> = {
    mcpServers: sdkMcpServers(configs),
  };
  const parameters: Parameters<Query["setMcpServers"]> = [
    sdkMcpServers(configs),
  ];
  assert.equal(options.mcpServers, configs);
  assert.equal(parameters[0], configs);
  assert.equal(configs.proxy.type, "claudeai-proxy");
});

const status = {
  name: "anarlog",
  status: "connected",
  source: "user",
  config: { type: "stdio", command: "node", args: ["catalog.mjs"] },
} satisfies Parameters<typeof externalCatalog>[0][number];
const statuses = [status];
const config = {
  ...status.config,
  env: { PRIVATE_TOKEN: "credential-secret" },
};
const tools = [
  {
    name: "first",
    description: "First",
    inputSchema: { type: "object", properties: { cwd: { type: "string" } } },
  },
  { name: "second", inputSchema: { type: "object" } },
] satisfies Tool[];
const dependencies = {
  localConfigs: async () => ({ anarlog: config }),
  listTools: async () => tools,
};

test("external proof lists only current server aliases and never transfers credentials", async () => {
  const aliases = externalAliases({
    inlineTools: false,
    activeDeferredNames: [
      "mcp__studio__orchestration_move",
      "mcp__anarlog__first",
      "mcp__anarlog__second",
      "mcp__claude_ai_Claude_Docs__batch",
    ],
    deferredTools: [{ name: "mcp__old__historical" }],
  });
  assert.deepEqual(aliases, ["anarlog", "claude_ai_Claude_Docs"]);
  let called = 0;
  const result = await externalCatalog(statuses, "/source", ["anarlog"], {
    ...dependencies,
    listTools: async (value) => {
      called++;
      assert.deepEqual(value, config);
      return tools;
    },
  });
  assert.equal(called, 1);
  assert.ok(result.catalogs[0]);
  assert.deepEqual(
    result.catalogs[0].tools.map((tool) => tool.name),
    ["mcp__anarlog__first", "mcp__anarlog__second"],
  );
  assert.ok(!JSON.stringify(result.catalogs).includes("credential-secret"));
  const localConfig = result.configs.anarlog as typeof config;
  assert.equal(localConfig.env.PRIVATE_TOKEN, "credential-secret");
  verifyExternalCatalog(result.catalogs, structuredClone(result.catalogs));
  for (const mutate of [
    (catalog: ExternalServer[]) => {
      assert.ok(catalog[0]);
      catalog[0].tools.reverse();
    },
    (catalog: ExternalServer[]) => {
      assert.ok(catalog[0]?.tools[0]);
      catalog[0].tools[0].description = "Changed";
    },
    (catalog: ExternalServer[]) => {
      // @ts-expect-error type intentionally violates MCP's object schema contract.
      (catalog[0].tools[0].input_schema as Tool["inputSchema"]).type = "array";
    },
    (catalog: ExternalServer[]) => {
      assert.ok(catalog[0]);
      catalog[0].tools = [];
    },
  ]) {
    const target = structuredClone(result.catalogs);
    mutate(target);
    assert.throws(
      () => verifyExternalCatalog(result.catalogs, target),
      /anarlog/,
    );
  }
});

test("external server failure and missing schemas return only safe names", async () => {
  for (const state of [
    [],
    [{ ...status, status: "needs-auth" }],
  ] satisfies Parameters<typeof externalCatalog>[0][])
    await assert.rejects(
      externalCatalog(state, "/target", ["anarlog"], dependencies),
      /anarlog/,
    );
  for (const listing of [
    async () => {
      throw new Error("https://host/?token=credential-secret");
    },
    async () => [{ name: "broken" }],
  ])
    await assert.rejects(
      externalCatalog(statuses, "/target", ["anarlog"], {
        ...dependencies,
        listTools: listing,
      }),
      (error) => {
        const refusal = error as ReturnType<typeof externalRefusal>;
        return (
          !JSON.stringify(refusal.data).includes("credential-secret") &&
          !refusal.message.includes("credential-secret")
        );
      },
    );
});

test("every disconnected SDK status refuses external proof before listing tools", async () => {
  const disconnected: Exclude<McpServerStatus["status"], "connected">[] = [
    "failed",
    "needs-auth",
    "pending",
    "disabled",
  ];
  for (const state of disconnected) {
    let listings = 0;
    await assert.rejects(
      externalCatalog([{ ...status, status: state }], "/target", ["anarlog"], {
        ...dependencies,
        listTools: async () => {
          listings++;
          return tools;
        },
      }),
      /anarlog/,
    );
    assert.equal(listings, 0);
  }
});

test.each([{ type: "stdio" }, {}] satisfies Pick<ExternalConfig, "type">[])(
  "stdio proof %# sends initialize and tools/list without tool calls or model input",
  async (transport) => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-mcp-list-"));
    const script = fileURLToPath(
      new URL("./move-external.fixture.ts", import.meta.url),
    );
    const log = path.join(root, "requests.jsonl");
    try {
      assert.deepEqual(
        await listExternalTools({
          ...transport,
          command: process.execPath,
          args: [script, log, "catalog", JSON.stringify(tools)],
        }),
        tools,
      );
      assert.deepEqual((await fs.readFile(log, "utf8")).trim().split("\n"), [
        "initialize",
        "notifications/initialized",
        "tools/list",
      ]);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  },
);

test("Codex instruction proof initializes without listing resources or calling tools", async () => {
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "codex-mcp-instructions-"),
  );
  const script = fileURLToPath(
    new URL("./move-external.fixture.ts", import.meta.url),
  );
  const log = path.join(root, "requests");
  try {
    assert.equal(
      await readExternalInstructions({
        type: "stdio",
        command: process.execPath,
        args: [script, log, "instructions"],
      }),
      "Private instructions",
    );
    assert.deepEqual((await fs.readFile(log, "utf8")).trim().split("\n"), [
      "initialize",
      "notifications/initialized",
    ]);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a catalog with sorted keys from the paired channel still verifies", async () => {
  const result = await externalCatalog(
    statuses,
    "/source",
    ["anarlog"],
    dependencies,
  );
  const sorted = JSON.parse(
    JSON.stringify(result.catalogs, (key, value) =>
      value && typeof value === "object" && !Array.isArray(value)
        ? Object.fromEntries(
            Object.keys(value)
              .sort()
              .map((k) => [k, value[k]]),
          )
        : value,
    ),
  );
  verifyExternalCatalog(sorted, result.catalogs);
});

test("without the connector token the account's connector catalog stands; local servers are read", async () => {
  const docs = {
    name: "claude.ai Claude Docs",
    alias: "claude_ai_Claude_Docs",
    configHash: "c",
    claudeAI: true,
    proxyId: "srv_1",
    tools: [
      {
        name: "mcp__claude_ai_Claude_Docs__guide",
        description: "Guide",
        input_schema: { type: "object" },
      },
    ],
  };
  const local = (
    await externalCatalog(statuses, "/source", ["anarlog"], dependencies)
  ).catalogs[0];
  assert.ok(local);
  let read = 0;
  const result = await targetExternalCatalog("/target", [docs, local], {
    ...dependencies,
    listTools: async (value) => {
      read++;
      assert.notEqual(value.type, "claudeai-proxy");
      return tools;
    },
    proxyToken: async () => {
      throw new Error("The local Claude proxy credential is unavailable");
    },
  });
  assert.equal(read, 1);
  assert.equal(result.accountConnectors, true);
  assert.deepEqual(result.catalogs, [docs, local]);
  assert.deepEqual(result.configs["claude.ai Claude Docs"], {
    type: "claudeai-proxy",
    id: "srv_1",
    url: "https://mcp-proxy.anthropic.com/v1/mcp/srv_1",
  });
  // A changed local server still refuses.
  await assert.rejects(
    targetExternalCatalog("/target", [docs, { ...local, tools: [] }], {
      ...dependencies,
      proxyToken: async () => {
        throw new Error("unavailable");
      },
    }),
    /anarlog/,
  );
});

test("a move carries a credential-free MCP transport and the target starts the same server", async () => {
  const transport = {
    type: "stdio",
    command: "node",
    args: ["catalog.mjs"],
  } satisfies ExternalConfig;
  const source = await externalCatalog(statuses, "/source", ["anarlog"], {
    localConfigs: async () => ({ anarlog: { ...transport, env: {} } }),
    listTools: async () => tools,
  });
  assert.deepEqual(source.transports, { anarlog: transport });
  let started: ExternalConfig | undefined;
  const target = await targetExternalCatalog("/target", source.catalogs, {
    localConfigs: async () => ({}),
    transports: structuredClone(source.transports),
    listTools: async (value) => {
      started = value;
      return tools;
    },
  });
  assert.deepEqual(started, transport);
  assert.deepEqual(target.catalogs, source.catalogs);
  assert.deepEqual(target.configs.anarlog, transport);
  // Without the carried transport the missing entry still refuses.
  await assert.rejects(
    targetExternalCatalog("/target", source.catalogs, {
      localConfigs: async () => ({}),
      listTools: async () => tools,
    }),
    /anarlog/,
  );
});

test("a server with credentials sends no transport, and only the proved transport starts", async () => {
  const withSecret = await externalCatalog(
    statuses,
    "/source",
    ["anarlog"],
    dependencies,
  );
  assert.deepEqual(withSecret.transports, {});
  assert.ok(
    !JSON.stringify(withSecret.transports).includes("credential-secret"),
  );
  // A carried transport that differs from the proved one is not started.
  const other = {
    type: "stdio",
    command: "node",
    args: ["other.mjs"],
  } satisfies ExternalConfig;
  let started = 0;
  await assert.rejects(
    targetExternalCatalog("/target", withSecret.catalogs, {
      localConfigs: async () => ({}),
      transports: { anarlog: other },
      listTools: async () => {
        started++;
        return tools;
      },
    }),
    /anarlog/,
  );
  assert.equal(started, 0);
  // The target's own entry with the same transport and its own credentials serves.
  const own = await targetExternalCatalog("/target", withSecret.catalogs, {
    localConfigs: async () => ({ anarlog: config }),
    transports: {},
    listTools: async (value) => {
      assert.deepEqual(value, config);
      return tools;
    },
  });
  assert.deepEqual(own.catalogs, withSecret.catalogs);
});

test.each([null, false, "", 0])(
  "a configured transport type %j cannot launch a stdio server",
  async (type) => {
    const root = await fs.mkdtemp(
      path.join(os.tmpdir(), "claude-mcp-boundary-"),
    );
    const log = path.join(root, "requests.jsonl");
    const script = fileURLToPath(
      new URL("./move-external.fixture.ts", import.meta.url),
    );
    vi.stubEnv("CLAUDE_CONFIG_DIR", root);
    try {
      await fs.writeFile(
        path.join(root, ".mcp.json"),
        JSON.stringify({
          mcpServers: {
            anarlog: {
              type,
              command: process.execPath,
              args: [script, log, "catalog", JSON.stringify(tools)],
            },
          },
        }),
      );
      await assert.rejects(
        externalCatalog(statuses, root, ["anarlog"]),
        /Unsupported MCP transport/,
      );
      await assert.rejects(fs.stat(log), { code: "ENOENT" });
    } finally {
      vi.unstubAllEnvs();
      await fs.rm(root, { recursive: true, force: true });
    }
  },
);

test.each([
  { type: "stdio", command: 42 },
  { command: "fixture", args: [42] },
  { command: "fixture", env: { TOKEN: 42 } },
  { type: "http", url: 42 },
  { type: "sse", url: "https://example.test", headers: { TOKEN: 42 } },
])(
  "a malformed MCP config refuses before listing tools: %j",
  async (invalidConfig) => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-mcp-fields-"));
    vi.stubEnv("CLAUDE_CONFIG_DIR", root);
    let reads = 0;
    try {
      await fs.writeFile(
        path.join(root, ".mcp.json"),
        JSON.stringify({ mcpServers: { anarlog: invalidConfig } }),
      );
      await assert.rejects(
        externalCatalog(statuses, root, ["anarlog"], {
          listTools: async () => {
            reads++;
            return tools;
          },
        }),
        /Unsupported MCP transport/,
      );
      assert.equal(reads, 0);
    } finally {
      vi.unstubAllEnvs();
      await fs.rm(root, { recursive: true, force: true });
    }
  },
);
