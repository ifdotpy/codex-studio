import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { join } from "node:path";
import { test, apiSchemaHandshakeSse } from "../playwright.mjs";

test("Realtime voice ux", async ({ context }, testInfo) => {
  test.setTimeout(180_000);
  // React fixture: no microphone, native account, or model request.
  const root = fileURLToPath(new URL("../../../web/", import.meta.url));
  const require = createRequire(root + "/package.json");
  const { createServer } = await import(require.resolve("vite"));
  const entryPath = join(root, "audit-entry.tsx");
  const workspaceId = "0123456789abcdef0123456789abcdef";
  const entry = `import React from 'react';import{createRoot}from'react-dom/client';import{MantineProvider}from'@mantine/core';import'@mantine/core/styles.css';import RealtimeVoice from '/src/components/RealtimeVoice.tsx';
  window.calls=[];window.order=[];window.stops=0;window.records=[];window.pending=false;window.voiceError='';window.codexDesktop={requestMicrophone:async()=>{window.order.push('permission');if(window.holdPermission)await new Promise(r=>window.releasePermission=r);}};
  window.RTCPeerConnection=class{constructor(){window.pc=this;}addTrack(){}createDataChannel(){return window.dc={close(){},send(){throw Error('Native Core owns messages');}};}async createOffer(){return {sdp:'v=0\\noffer'};}async setLocalDescription(){}async setRemoteDescription(sdp){window.answers=(window.answers||0)+1;window.answer=sdp;window.emit({type:"session.started"});}close(){}};
  const track=window.track={enabled:true,stop(){window.stops++;}};
  Object.defineProperty(navigator,'mediaDevices',{value:{getUserMedia:async()=>{window.order.push('capture');return {getTracks:()=>[track],getAudioTracks:()=>[track]};}}});
  window.emit=e=>window.dc.onmessage({data:JSON.stringify(e)});
  const nativeFetch=window.fetch.bind(window);window.resourceChange=()=>nativeFetch('/audit/resource-change',{method:'POST'});
  window.fetch=async(input,opts)=>{const request=input instanceof Request?input:null,path=request?.url||input,body=request?await request.clone().text():opts?.body;const a=new URL(path,location.origin).pathname.split('/').at(-1),b=JSON.parse(body||'{}');window.calls.push([a,b]);let data={};if(a==='identity')data={workspaceId:'${workspaceId}'};if(a==='status'){window.order.push('status');data={configured:true,transport:'native'};}if(a==='records'&&window.failRecords)throw Error('Connection unavailable');if(a==='records')data={records:window.records,cursor:window.records.length,session:window.sid?{session_id:window.sid,state:window.voiceError?'failed':window.pending?'connecting':'ready',sdp:window.pending?null:'answer',error:window.voiceError}:null};if(a==='start'){window.sid=b.session_id;const stale=window.holdStart?{session_id:window.sid,state:'connecting',sdp:null}:null;if(window.holdStart)await new Promise(r=>window.releaseStart=r);data=stale||{session_id:window.sid,state:window.pending?'connecting':'ready',sdp:window.pending?null:'answer'};}if(a==='session')data={session_id:window.sid,state:window.voiceError?'failed':window.pending?'connecting':'ready',sdp:window.pending?null:'answer',error:window.voiceError};return new Response(JSON.stringify(data));};
  const root=createRoot(document.getElementById('root'));window.render=id=>root.render(<MantineProvider defaultColorScheme='dark'><RealtimeVoice agentId={id} notify={()=>{}}/></MantineProvider>);window.render('chat-one');`;
  const server = await createServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "voice-fixture",
        configureServer(s) {
          const streams = new Set();
          let revision = 0;
          const epoch = "voice-fixture-epoch";
          const writeResourceEvent = (stream, reason) => {
            revision++;
            stream.res.write(
              `event: resources\ndata: ${JSON.stringify({ protocol: 3, workspaceId, epoch, revision, reason, resources: stream.resources })}\n\n`,
            );
          };
          s.middlewares.use("/audit/resource-change", (req, res) => {
            if (req.method !== "POST") {
              res.statusCode = 405;
              res.end();
              return;
            }
            for (const stream of streams) writeResourceEvent(stream, "change");
            res.end("ok");
          });
          s.middlewares.use("/audit", (_, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root" style="padding:200px 24px"></div><script type="module" src="/audit-entry.tsx"></script>',
            );
          });
          s.middlewares.use("/api/sync/stream", (req, res) => {
            const query = new URL(req.url || "/", "http://localhost")
              .searchParams;
            const stream = {
              res,
              resources: JSON.parse(query.get("resources") || "[]"),
            };
            res.writeHead(200, {
              "Content-Type": "text/event-stream",
              "Cache-Control": "no-cache",
              Connection: "keep-alive",
            });
            res.write(apiSchemaHandshakeSse());
            streams.add(stream);
            writeResourceEvent(stream, "initial");
            const heartbeat = setInterval(() => {
              res.write(
                `event: heartbeat\ndata: ${JSON.stringify({ protocol: 3, workspaceId, epoch, revision })}\n\n`,
              );
            }, 1000);
            res.on("close", () => {
              clearInterval(heartbeat);
              streams.delete(stream);
            });
          });
        },
        resolveId(id) {
          if (id === "/audit-entry.tsx") return entryPath;
        },
        load(id) {
          if (id === entryPath) return entry;
        },
      },
    ],
  });
  await server.listen();
  try {
    const page = await context.newPage();
    await page.setViewportSize({ width: 500, height: 700 });
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    async function reset() {
      await page.goto(server.resolvedUrls.local[0] + "audit");
      await page
        .getByRole("button", { name: "Voice conversation", exact: true })
        .click();
    }
    async function start() {
      await page
        .getByRole("button", { name: "Start voice", exact: true })
        .click();
      await page.waitForFunction(() =>
        window.calls.some((c) => c[0] === "start"),
      );
    }
    await reset();
    await start();
    await page.waitForFunction(() => window.answer?.sdp === "answer");
    assert.deepEqual(await page.evaluate(() => window.order), [
      "status",
      "permission",
      "capture",
    ]);
    assert.equal(
      await page
        .getByRole("button", { name: "Send transcript", exact: true })
        .count(),
      0,
    );
    const readsBeforeTranscriptChange = await page.evaluate(
      () => window.calls.filter((call) => call[0] === "records").length,
    );
    await page.evaluate(async () => {
      window.emit({ type: "delegation.created", item: { id: "task" } });
      window.records = [
        {
          id: "one",
          seq: 1,
          kind: "user",
          text: "Native canonical transcript",
        },
      ];
      await window.resourceChange();
    });
    await page.waitForFunction(
      (count) =>
        window.calls.filter((call) => call[0] === "records").length ===
        count + 1,
      readsBeforeTranscriptChange,
    );
    await page.waitForFunction(() =>
      [...document.querySelectorAll('[role="status"]')].some(
        (status) => status.textContent === "Working",
      ),
    );
    await page
      .locator(".realtime-voice details summary")
      .evaluate((summary) => summary.click());
    await page.waitForFunction(() =>
      document.body.textContent?.includes("Native canonical transcript"),
    );
    assert.equal(
      await page.evaluate(
        () =>
          window.calls.filter((c) =>
            ["submit", "record", "speech", "approve"].includes(c[0]),
          ).length,
      ),
      0,
    );
    await page.getByRole("button", { name: "Mute", exact: true }).click();
    await page.getByRole("button", { name: "Unmute", exact: true }).waitFor();
    await page.screenshot({
      path: testInfo.outputPath("studio-native-voice-ui.png"),
    });
    await page.getByRole("button", { name: "End voice", exact: true }).click();
    await page.waitForFunction(
      () => window.stops === 1 && window.calls.some((c) => c[0] === "end"),
    );
    await reset();
    await start();
    await page.waitForFunction(() =>
      window.calls.some((call) => call[0] === "records"),
    );
    await page.waitForTimeout(100);
    const readsBeforeOffline = await page.evaluate(
      () => window.calls.filter((call) => call[0] === "records").length,
    );
    const statusBeforeOffline = await page.evaluate(
      () => window.calls.filter((call) => call[0] === "status").length,
    );
    await context.setOffline(true);
    await page.waitForFunction(() =>
      [...document.querySelectorAll('[role="alert"]')].some((alert) =>
        alert.textContent?.includes("The microphone is off."),
      ),
    );
    assert.equal(await page.evaluate(() => window.stops), 1);
    assert.equal(
      await page.evaluate(
        () => window.calls.filter((call) => call[0] === "status").length,
      ),
      statusBeforeOffline,
    );
    assert.equal(
      await page.evaluate(
        () => window.calls.filter((call) => call[0] === "records").length,
      ),
      readsBeforeOffline,
    );
    await context.setOffline(false);
    await page.waitForFunction(
      (count) =>
        window.calls.filter((call) => call[0] === "records").length ===
        count + 1,
      readsBeforeOffline,
    );
    await page.waitForTimeout(100);
    assert.equal(
      await page.evaluate(
        () => window.calls.filter((call) => call[0] === "records").length,
      ),
      readsBeforeOffline + 1,
      JSON.stringify(await page.evaluate(() => window.calls)),
    );
    await reset();
    await page.evaluate(() => (window.pending = true));
    await start();
    assert.equal(await page.evaluate(() => window.answer), undefined);
    await page.evaluate(async () => {
      window.pending = false;
      await window.resourceChange();
    });
    await page.waitForFunction(() => window.answer?.sdp === "answer");
    await reset();
    await page.evaluate(() => {
      window.pending = true;
      window.holdStart = true;
    });
    const recordsBeforeEarlyAnswer = await page.evaluate(
      () => window.calls.filter((call) => call[0] === "records").length,
    );
    await start();
    await page.evaluate(async () => {
      window.pending = false;
      await window.resourceChange();
    });
    await page.waitForFunction(
      (count) =>
        window.calls.filter((call) => call[0] === "records").length ===
        count + 1,
      recordsBeforeEarlyAnswer,
    );
    assert.equal(await page.evaluate(() => window.answer), undefined);
    await page.evaluate(() => {
      window.holdStart = false;
      window.releaseStart();
    });
    await page.waitForFunction(() => window.answer?.sdp === "answer");
    assert.equal(
      await page.evaluate(() => window.answers),
      1,
      "the buffered records answer is applied once after the stale start response",
    );
    assert.equal(await page.evaluate(() => window.stops), 0);
    await page.evaluate(async () => {
      window.voiceError = "Voice connection failed";
      await window.resourceChange();
    });
    await page.waitForFunction(() =>
      [...document.querySelectorAll('[role="alert"]')].some((alert) =>
        alert.textContent?.includes("Voice connection failed"),
      ),
    );
    assert.equal(await page.evaluate(() => window.stops), 1);
    await reset();
    await page.evaluate(() => (window.holdPermission = true));
    await page
      .getByRole("button", { name: "Start voice", exact: true })
      .click();
    await page.waitForFunction(() => !!window.releasePermission);
    await page.getByRole("button", { name: "End voice", exact: true }).click();
    await page.evaluate(() => window.releasePermission());
    assert.equal(
      await page.evaluate(() => window.order.includes("capture")),
      false,
    );
    await reset();
    await page.evaluate(() => (window.holdStart = true));
    await start();
    const sid = await page.evaluate(() => window.sid);
    await page.getByRole("button", { name: "End voice", exact: true }).click();
    await page.evaluate(() => window.releaseStart());
    await page.waitForFunction(() => window.calls.some((c) => c[0] === "end"));
    assert.ok(
      (
        await page.evaluate(() => window.calls.filter((c) => c[0] === "end"))
      ).every((c) => c[1].session_id === sid),
    );
    await reset();
    await start();
    await page.evaluate(() => window.render("chat-two"));
    await page.waitForFunction(() => window.stops === 1);
    assert.equal(
      await page.evaluate(
        () => window.calls.filter((c) => c[0] === "end").at(-1)[1].agent,
      ),
      "chat-one",
    );
    await reset();
    await start();
    await page.getByRole("button", { name: "Close voice panel" }).click();
    await page
      .getByRole("group", { name: "Active voice conversation" })
      .waitFor();
    await page.getByRole("button", { name: "Mute", exact: true }).click();
    assert.equal(await page.evaluate(() => window.track.enabled), false);
    await page.getByRole("button", { name: "Unmute", exact: true }).click();
    assert.equal(await page.evaluate(() => window.track.enabled), true);
    await page.evaluate(async () => {
      window.failRecords = true;
      await window.resourceChange();
    });
    await page.waitForFunction(() =>
      [...document.querySelectorAll('[role="alert"]')].some((alert) =>
        alert.textContent?.includes("The microphone is off."),
      ),
    );
    assert.equal(await page.evaluate(() => window.stops), 1);
    await reset();
    await start();
    await page.evaluate(() => {
      window.pc.connectionState = "disconnected";
      window.pc.onconnectionstatechange();
    });
    assert.equal(await page.evaluate(() => window.track.enabled), false);
    await page.waitForFunction(
      () =>
        [...document.querySelectorAll('[role="alert"]')].some((alert) =>
          alert.textContent?.includes("Voice could not reconnect"),
        ),
      { timeout: 18000 },
    );
    assert.equal(await page.evaluate(() => window.stops), 1);
    await reset();
    await page.evaluate(() =>
      localStorage.setItem(
        "voice-delivery:chat-one",
        JSON.stringify({ editedText: "Saved old voice text" }),
      ),
    );
    await page.reload();
    await page
      .getByRole("button", { name: "Voice conversation", exact: true })
      .click();
    assert.equal(
      await page.getByText("Recovered voice draft", { exact: true }).count(),
      0,
    );
    assert.deepEqual(
      await page.evaluate(() =>
        JSON.parse(localStorage.getItem("voice-delivery:chat-one")),
      ),
      { editedText: "Saved old voice text" },
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS: native voice UI, canonical transcript, account switch, delayed SDP, connection error, stop during permission and start.",
    );
  } finally {
    await server.close();
  }
});
