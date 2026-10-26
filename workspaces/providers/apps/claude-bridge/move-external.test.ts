import { test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  externalAliases,
  externalCatalog,
  verifyExternalCatalog,
  targetExternalCatalog,
  listExternalTools,
  readExternalInstructions,
} from "./move-external.mjs";

const statuses = [
  {
    name: "anarlog",
    status: "connected",
    source: "user",
    config: { type: "stdio", command: "node", args: ["catalog.mjs"] },
  },
];
const config = {
  ...statuses[0].config,
  env: { PRIVATE_TOKEN: "credential-secret" },
};
const tools = [
  {
    name: "first",
    description: "First",
    inputSchema: { type: "object", properties: { cwd: { type: "string" } } },
  },
  { name: "second", inputSchema: { type: "object" } },
];
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
  assert.deepEqual(
    result.catalogs[0].tools.map((tool) => tool.name),
    ["mcp__anarlog__first", "mcp__anarlog__second"],
  );
  assert.ok(!JSON.stringify(result.catalogs).includes("credential-secret"));
  assert.equal(result.configs.anarlog.env.PRIVATE_TOKEN, "credential-secret");
  verifyExternalCatalog(result.catalogs, structuredClone(result.catalogs));
  for (const mutate of [
    (catalog) => catalog[0].tools.reverse(),
    (catalog) => (catalog[0].tools[0].description = "Changed"),
    (catalog) => (catalog[0].tools[0].input_schema.type = "array"),
    (catalog) => (catalog[0].tools = []),
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
  for (const state of [[], [{ ...statuses[0], status: "needs-auth" }]])
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
      (error) =>
        !JSON.stringify(error.data).includes("credential-secret") &&
        !error.message.includes("credential-secret"),
    );
});

test("stdio proof sends initialize and tools/list without tool calls or model input", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-mcp-list-"));
  const script = path.join(root, "server.mjs");
  const log = path.join(root, "requests.jsonl");
  await fs.writeFile(
    script,
    `import fs from 'node:fs';import {createInterface} from 'node:readline';for await(const line of createInterface({input:process.stdin})){const r=JSON.parse(line);fs.appendFileSync(${JSON.stringify(log)},r.method+'\\n');if(!('id'in r))continue;const result=r.method==='initialize'?{protocolVersion:r.params.protocolVersion,capabilities:{tools:{}},serverInfo:{name:'fixture',version:'1'}}:r.method==='tools/list'?{tools:${JSON.stringify(tools)}}:{};process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:r.id,result})+'\\n');}`,
  );
  try {
    assert.deepEqual(
      await listExternalTools({
        type: "stdio",
        command: process.execPath,
        args: [script],
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
});

test("Codex instruction proof initializes without listing resources or calling tools", async () => {
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "codex-mcp-instructions-"),
  );
  const script = path.join(root, "server.mjs");
  const log = path.join(root, "requests");
  await fs.writeFile(
    script,
    `import fs from 'node:fs';import {createInterface} from 'node:readline';for await(const line of createInterface({input:process.stdin})){const r=JSON.parse(line);fs.appendFileSync(${JSON.stringify(log)},r.method+'\\n');if(!('id'in r))continue;process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:r.id,result:{protocolVersion:r.params.protocolVersion,capabilities:{tools:{}},serverInfo:{name:'fixture',version:'1'},instructions:'Private instructions'}})+'\\n');}`,
  );
  try {
    assert.equal(
      await readExternalInstructions({
        type: "stdio",
        command: process.execPath,
        args: [script],
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
  const transport = { type: "stdio", command: "node", args: ["catalog.mjs"] };
  const source = await externalCatalog(statuses, "/source", ["anarlog"], {
    localConfigs: async () => ({ anarlog: { ...transport, env: {} } }),
    listTools: async () => tools,
  });
  assert.deepEqual(source.transports, { anarlog: transport });
  let started;
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
  const other = { type: "stdio", command: "node", args: ["other.mjs"] };
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
