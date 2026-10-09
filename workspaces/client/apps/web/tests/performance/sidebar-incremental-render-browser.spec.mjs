import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFile, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, readApiSchemaHash } from "../playwright.mjs";

test("Sidebar incremental rows and current callbacks @performance", async ({
  page,
}) => {
  test.setTimeout(60_000);
  const root = join(import.meta.dirname, "../../../web");
  const require = createRequire(join(root, "package.json"));
  const { build } = require("esbuild");
  const { createServer } = await import(require.resolve("vite"));
  const directory = await mkdtemp(
    join(tmpdir(), "studio-sidebar-incremental-"),
  );
  const workspaceId = "c".repeat(32);
  const bundle = join(directory, "fixture.js");
  const fixture = `
import React, {useMemo} from "react";
import {createRoot} from "react-dom/client";
import {MantineProvider} from "@mantine/core";
import Sidebar from "/src/components/Sidebar.tsx";
import SidebarRow from "/src/components/SidebarRow.tsx";
import {useSnapshot} from "/src/hooks.ts";
import {syncDatabase,persistProjection} from "/src/sync/client.ts";
import {chatIndicators} from "/src/components/chat-status/chatStatusModel.ts";
const noop=()=>{};
window.fixtureRows=0;window.fixtureJsonReads=0;window.fixtureRenders=0;
window.fixtureActions=[];
const row=(collection,id,value,seq)=>({id:"entity:"+collection+":"+id,seq,
 payload:JSON.stringify({collection,id,value})});
const agents=Array.from({length:20},(_,i)=>({id:"agent-"+i,name:"Chat "+i,
 kind:"agent",source:"managed",isLead:true,rootId:"agent-"+i,cwd:"/fixture",
 status:"completed",created:i,threadId:"thread-"+i,epoch:0,
 lastCompletedTurn:"turn-"+i,lastCompletedTurnStatus:"completed",readStateSupported:true}));
const {db}=await syncDatabase();
await db.projections.bulkInsert([
 ...agents.map((a,i)=>row("agent",a.id,a,i+1)),
 row("rule","r",{id:"r",checks:0},21),
 row("workspace","current",{stateDir:"/private-fixture",projectOrganizationVersion:1,
 sidebarOrder:{revision:1,groups:{}},peerTeamsVersion:1},22),
 {id:"state:entities:ready",seq:1,payload:"ready"},
]);
const oldDocument=await db.projections.findOne("entity:rule:r").exec();
const oldPayload=oldDocument.toJSON().payload;
window.sameDocumentVersion=oldDocument===await db.projections.findOne("entity:rule:r").exec();
window.documentVersionProof=async()=>{
 const current=await db.projections.findOne("entity:rule:r").exec();
 return {sameVersion:window.sameDocumentVersion,newVersion:current!==oldDocument,
  stableNewVersion:current===await db.projections.findOne("entity:rule:r").exec(),
  immutableOldVersion:oldDocument.toJSON().payload===oldPayload};
};
window.write=async(collection,id,value,seq,deleted=false)=>{
 await persistProjection(db.projections,{...row(collection,id,value,seq),_deleted:deleted});
};
window.readyAgain=()=>persistProjection(db.projections,{id:"state:entities:ready",seq:2,payload:"ready"});
function Harness(){
 const {data}=useSnapshot();window.fixtureRenders++;
 const indicators=useMemo(()=>data?chatIndicators(data):new Map(),[data]);
 window.fixtureData=data;
 const checks=data?.runtime?.rules?.[0]?.checks||0;
 return data?<><Sidebar data={data} opened={null} open={id=>window.fixtureActions.push([id,checks])}
 prepareChat={id=>window.fixtureActions.push(["prepare",id,checks])}
 newChat={noop} addProject={noop} changeProject={noop} projectAccount={noop} creating={false}
 rename={async()=>{}} remove={noop} mobile={false} onSearch={noop} close={noop}
 indicators={indicators} markUnread={a=>window.fixtureActions.push(["unread",a.id,checks])}
 markingRead={new Set()}/><SidebarRow row={agents[0]} selected={false}
 renaming={false} name="" organizing={false} compact={false} markingRead={false}
 bindings={{"data-sidebar-id":"key-row",onKeyDown:()=>window.fixtureActions.push(["key",checks]),
  onDragOver:()=>window.fixtureActions.push(["drag",checks])}}
 actions={{prepare:noop,open:noop,pin:noop,rename:noop,name:noop,cancelRename:noop,
  beginRename:noop,unread:noop,move:noop,changeProject:noop,archive:noop,remove:noop}}/></>:null;
}
createRoot(document.getElementById("root")).render(<MantineProvider><Harness/></MantineProvider>);
window.fixtureAgent=agents[19];window.fixtureReady=true;
`;
  let server;
  try {
    await build({
      stdin: { contents: fixture, loader: "tsx", resolveDir: root },
      absWorkingDir: root,
      bundle: true,
      jsx: "automatic",
      format: "esm",
      platform: "browser",
      outfile: bundle,
      define: { "process.env.NODE_ENV": '"production"' },
      plugins: [
        {
          name: "isolated-sidebar-probes",
          setup(build) {
            build.onResolve({ filter: /^\/src\// }, (args) => ({
              path: join(root, args.path),
            }));
            // The fixture measures the actual local projection and hook. The
            // resource transport has separate protocol and recovery contracts.
            build.onLoad({ filter: /resourceEvents\.ts$/ }, () => ({
              contents:
                "export function watchResourceChanges(){return()=>{}};export function watchResourceConnection(){return()=>{}};",
              loader: "ts",
            }));
            build.onLoad(
              {
                filter:
                  /\/src\/(components\/Sidebar(?:Row)?\.tsx|sync\/client\.ts)$/,
              },
              async (args) => {
                let source = await readFile(args.path, "utf8");
                // Both implementations use the same probe. The old Sidebar
                // builds every row; the memoized view builds changed rows only.
                source = source.replace(
                  "const renderRow = (row: Agent) => {",
                  "const renderRow = (row: Agent) => {window.fixtureRows++;",
                );
                source = source.replace(
                  "  const a = row;\n  return (",
                  "  window.fixtureRows++; const a = row;\n  return (",
                );
                source = source.replaceAll(
                  "document.toJSON()",
                  "(window.fixtureJsonReads++, document.toJSON())",
                );
                return {
                  contents: source,
                  loader: args.path.endsWith(".tsx") ? "tsx" : "ts",
                };
              },
            );
          },
        },
      ],
    });
    server = await createServer({
      configFile: false,
      root: directory,
      cacheDir: join(directory, "cache"),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "isolated-fixture-api",
          configureServer(vite) {
            vite.middlewares.use((request, response, next) => {
              const path = new URL(request.url, "http://fixture").pathname;
              response.setHeader("X-Studio-API-Schema", readApiSchemaHash());
              if (path === "/") {
                response.setHeader("Content-Type", "text/html");
                response.end(
                  '<div id="root"></div><link rel="stylesheet" href="/fixture.css"><script type="module" src="/fixture.js"></script>',
                );
                return;
              }
              let value;
              if (path === "/api/sync/identity") value = { workspaceId };
              if (path === "/api/session") value = { token: "fixture" };
              if (path === "/api/sync/pull")
                value = {
                  workspaceId,
                  documents: [],
                  checkpoint: { seq: 22 },
                  maxSeq: 22,
                  initialHigh: 22,
                };
              if (value) {
                response.setHeader("Content-Type", "application/json");
                response.end(JSON.stringify(value));
                return;
              }
              if (path.startsWith("/api/")) {
                response.statusCode = 500;
                response.end('{"error":"Isolated fixture"}');
                return;
              }
              next();
            });
          },
        },
      ],
    });
    await server.listen();
    const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/*", (route) => {
      if (
        !route.request().url().startsWith(`${origin}/`) ||
        !["GET", "HEAD"].includes(route.request().method())
      )
        return route.abort();
      return route.continue();
    });
    await page.goto(origin);
    await page.waitForFunction(() => window.fixtureReady && window.fixtureData);
    await page.locator('[data-chat="agent-19"]').waitFor();
    await page.waitForTimeout(100);
    const before = await page.evaluate(() => ({
      rows: window.fixtureRows,
      json: window.fixtureJsonReads,
      renders: window.fixtureRenders,
    }));
    for (let checks = 1; checks <= 3; checks++) {
      await page.evaluate(async (checks) => {
        await window.write("rule", "r", { id: "r", checks }, 100 + checks);
      }, checks);
      await page.waitForFunction(
        (checks) => window.fixtureData.runtime.rules[0].checks === checks,
        checks,
      );
      await page.waitForTimeout(30);
    }
    const after = await page.evaluate(() => ({
      rows: window.fixtureRows,
      json: window.fixtureJsonReads,
      renders: window.fixtureRenders,
    }));
    assert.equal(
      after.rows - before.rows,
      0,
      "rule changes retain all row views",
    );
    assert.equal(
      after.json - before.json,
      3,
      "each new RxDB document version converts once",
    );
    assert.equal(
      after.renders - before.renders,
      3,
      "empty creation reconciliation does not add a render",
    );
    assert.deepEqual(
      await page.evaluate(() => window.documentVersionProof()),
      {
        sameVersion: true,
        newVersion: true,
        stableNewVersion: true,
        immutableOldVersion: true,
      },
      "RxDB caches immutable document instances per revision",
    );
    await page.locator('[data-sidebar-id="key-row"]').press("ArrowDown");
    assert.deepEqual(await page.evaluate(() => window.fixtureActions.at(-1)), [
      "key",
      3,
    ]);
    await page.locator('[data-sidebar-id="key-row"]').dispatchEvent("dragover");
    assert.deepEqual(await page.evaluate(() => window.fixtureActions.at(-1)), [
      "drag",
      3,
    ]);
    await page.locator('[data-chat="agent-19"]').click();
    assert.deepEqual(
      await page.evaluate(() => window.fixtureActions.at(-1)),
      ["agent-19", 3],
      "an unchanged row calls the current committed callback",
    );
    const unchanged = await page.evaluate(() => window.fixtureRenders);
    await page.evaluate(() => window.readyAgain());
    await page.waitForTimeout(100);
    assert.equal(
      await page.evaluate(() => window.fixtureRenders),
      unchanged,
      "a ready marker alone does not publish the identical snapshot",
    );
    const rowsBeforeAgent = await page.evaluate(() => window.fixtureRows);
    await page.evaluate(async () => {
      const value = {
        ...window.fixtureAgent,
        name: "Current chat",
        status: "failed",
      };
      await window.write("agent", value.id, value, 200);
    });
    await page
      .locator('[data-chat="agent-19"]')
      .getByText("Current chat", { exact: true })
      .waitFor();
    assert.equal(
      await page.evaluate(() => window.fixtureRows),
      rowsBeforeAgent + 1,
      "the changed agent immediately replaces exactly its row view",
    );
    await page.evaluate(() => window.write("agent", "agent-19", {}, 201, true));
    await page.locator('[data-chat="agent-19"]').waitFor({ state: "detached" });
    await page.evaluate(() =>
      window.write("agent", "agent-19", window.fixtureAgent, 200),
    );
    await page.waitForTimeout(50);
    assert.equal(
      await page.locator('[data-chat="agent-19"]').count(),
      0,
      "a stale sequence cannot restore a deleted agent",
    );
    await page.evaluate(() =>
      window.write(
        "agent",
        "agent-19",
        { ...window.fixtureAgent, name: "Restored" },
        202,
      ),
    );
    await page
      .locator('[data-chat="agent-19"]')
      .getByText("Restored", { exact: true })
      .waitFor();
    assert.deepEqual(errors, []);
  } finally {
    await server?.close();
    await rm(directory, { recursive: true, force: true });
  }
});
