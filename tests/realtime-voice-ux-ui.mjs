// Isolated React fixture. No real microphone, native permission, backend, or model calls.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { resolve } from "node:path";
const root = resolve(import.meta.dirname, "../web");
const require = createRequire(root + "/package.json");
const { createServer } = await import(require.resolve("vite"));
const { chromium } = require("playwright-core");
const entry = `import React from 'react';import{createRoot}from'react-dom/client';import{MantineProvider}from'@mantine/core';import'@mantine/core/styles.css';import RealtimeVoice from '/src/components/RealtimeVoice.tsx';
window.voiceStartOrder=[];window.microphonePermits=0;window.codexDesktop={requestMicrophone:async()=>{window.voiceStartOrder.push('microphone');window.microphonePermits++;}};window.MediaRecorder=undefined; window.notify=[];window.submits=[];window.records=[]; window.emit=e=>window.dc.onmessage({data:JSON.stringify(e)});
window.RTCPeerConnection=class {constructor(){window.pc=this;}addTrack(){}createDataChannel(){return window.dc={close(){},send(){}};}async createOffer(){return {sdp:'offer'};}async setLocalDescription(){}async setRemoteDescription(){}close(){}};
Object.defineProperty(navigator,'mediaDevices',{value:{getUserMedia:async()=>{window.voiceStartOrder.push('capture');if(!window.microphonePermits)throw Error('Missing native microphone permit');return {getTracks:()=>[{stop(){}}],getAudioTracks:()=>[]};}}});
window.fetch=async(path,opts)=>{const action=path.split('/').at(-1),b=JSON.parse(opts?.body||'{}');let result={};let status=200;if(action==='status'){window.voiceStartOrder.push('status');window.microphonePermits=0;result={configured:true};}if(action==='records')result={records:window.records,cursor:0,delivered:[]};if(action==='start')result={records:window.records,cursor:0,delivered:[],sdp:'answer'};if(action==='approvals')result={requests:[]};if(action==='record'){if(window.failRecord){status=500;result={error:'Record write failed'};}else{result={...b,id:b.event_id,session:b.session_id,seq:window.records.length+1};window.records.push(result);}}if(action==='submit'){window.submits.push(b);if(window.loseSubmit){window.loseSubmit=false;throw new TypeError('Response lost');}if(window.submitError){status=400;result={error:window.submitError};}else if(!b.edited_text?.trim()&&b.edited_text!==null){status=400;result={error:'Edited transcript must contain 1 to 32000 characters'};}}return new Response(JSON.stringify(result),{status});};
createRoot(document.getElementById('root')).render(<MantineProvider><RealtimeVoice agentId='test' notify={t=>window.notify.push(t)}/></MantineProvider>);`;
const server = await createServer({
  configFile: false,
  root,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "audit",
      configureServer(s) {
        s.middlewares.use("/audit", (_, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/audit-entry.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/audit-entry.tsx") return root + id;
      },
      load(id) {
        if (id === root + "/audit-entry.tsx") return entry;
      },
    },
  ],
});
await server.listen();
const browser = await chromium.launch({
  executablePath:
    process.env.CHROME_BIN ||
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  headless: true,
});
try {
  const page = await browser.newPage();
  const origin = server.resolvedUrls.local[0];
  async function reset() {
    await page.goto(origin + "audit");
    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await page
      .getByRole("button", { name: "Voice conversation", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Start voice", exact: true })
      .click();
    await page
      .getByRole("button", { name: "End voice", exact: true })
      .waitFor();
  }
  await reset();
  await page.evaluate(() => {
    window.emit({
      type: "conversation.item.input_audio_transcription.completed",
      item_id: "one",
      transcript: "Please do work",
    });
  });
  await page.getByText("Edit transcript before send").click();
  await page.getByRole("textbox").fill("");
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  await page
    .getByText("Edited transcript must contain 1 to 32000 characters", {
      exact: true,
    })
    .waitFor();
  await page.getByRole("textbox").fill("Corrected text");
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  await page.waitForFunction(() => window.submits.length === 1);
  const submits = await page.evaluate(() => window.submits);
  assert.equal(submits[0].edited_text, "Corrected text");
  assert.equal(await page.evaluate(() => window.microphonePermits), 1);
  assert.deepEqual(await page.evaluate(() => window.voiceStartOrder), ["status", "microphone", "capture"]);
  console.log(
    "PASS: native microphone bridge and editable validation recovery.",
  );
  await reset();
  await page.evaluate(() => {
    window.failRecord = true;
    window.emit({ type: "input_audio_buffer.committed", item_id: "two" });
    window.emit({
      type: "conversation.item.input_audio_transcription.completed",
      item_id: "two",
      transcript: "A saved utterance",
    });
  });
  await page.getByRole("button", { name: "Retry transcript sync" }).waitFor();
  await page.evaluate(() => (window.failRecord = false));
  await page.getByRole("button", { name: "Retry transcript sync" }).click();
  await page.getByText("Transcript sync restored.", { exact: true }).waitFor();
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  await page.waitForFunction(() => window.submits.length === 1);
  console.log("PASS: transcript retry restores Send.");
  await reset();
  await page.evaluate(() =>
    window.emit({
      type: "conversation.item.input_audio_transcription.completed",
      item_id: "three",
      transcript: "Request to send",
    }),
  );
  await page.getByText("Edit transcript before send").waitFor();
  await page.evaluate(() => window.emit({ type: "response.created" }));
  await page.getByRole("button", { name: "End voice", exact: true }).click();
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  await page.waitForFunction(() => window.submits.length === 1);
  console.log(
    "PASS: End voice during courier response permits saved transcript delivery.",
  );

  await reset();
  await page.evaluate(() =>
    window.emit({
      type: "conversation.item.input_audio_transcription.completed",
      item_id: "four",
      transcript: "Original speech",
    }),
  );
  await page.getByText("Edit transcript before send").click();
  await page.getByRole("textbox").fill("First edit");
  await page.evaluate(() => (window.loseSubmit = true));
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  await page.getByText("Response lost", { exact: true }).waitFor();
  await page.getByRole("textbox").fill("Changed after response loss");
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  await page.waitForFunction(() => window.submits.length === 2);
  const attempts = await page.evaluate(() => window.submits);
  assert.deepEqual(attempts[1], attempts[0]);
  assert.equal(attempts[1].edited_text, "First edit");
  console.log("PASS: lost-response retry retains exact request and text.");
  await reset();
  await page.evaluate(() =>
    window.emit({
      type: "conversation.item.input_audio_transcription.completed",
      item_id: "five",
      transcript: "Saved before interruption",
    }),
  );
  await page.getByText("Edit transcript before send").waitFor();
  await page.evaluate(() =>
    window.emit({ type: "input_audio_buffer.speech_started" }),
  );
  await page.getByRole("button", { name: "End voice", exact: true }).click();
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  assert.equal(await page.evaluate(() => window.submits.length), 0);
  await page
    .getByRole("button", { name: "Use saved transcript only", exact: true })
    .waitFor();
  await page.reload();
  await page
    .getByRole("button", { name: "Voice conversation", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Use saved transcript only", exact: true })
    .waitFor();
  await page.getByRole("button", { name: "Start voice", exact: true }).click();
  await page.getByRole("button", { name: "End voice", exact: true }).waitFor();
  await page.evaluate(() =>
    window.emit({
      type: "conversation.item.input_audio_transcription.completed",
      item_id: "six",
      transcript: "Repeated after restart",
    }),
  );
  await page
    .getByRole("button", { name: "Use saved transcript only", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  await page.waitForFunction(() => window.submits.length === 1);
  console.log(
    "PASS: missing speech warning survives reload; explicit choice and restart allow sending.",
  );
  async function restoreAttempt(text, serverError) {
    await page.evaluate((editedText) => {
      localStorage.clear();
      localStorage.setItem(
        "voice-delivery:test",
        JSON.stringify({
          id: "legacy-attempt",
          ids: ["legacy-record"],
          editedText,
        }),
      );
    }, text);
    await page.reload();
    await page.evaluate((error) => {
      window.submitError = error;
      window.records.push({
        id: "legacy-record",
        kind: "user",
        text: "Original saved speech",
        seq: 1,
        session: "legacy-session",
        item_id: "legacy-item",
      });
    }, serverError);
    await page
      .getByRole("button", { name: "Voice conversation", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Start voice", exact: true })
      .click();
    await page
      .getByRole("button", { name: "End voice", exact: true })
      .waitFor();
  }
  await restoreAttempt("");
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  await page
    .getByText(
      "The server rejected the invalid transcript before sending. Edit the text, then send again.",
      { exact: true },
    )
    .waitFor();
  assert.equal(
    await page.evaluate(() =>
      JSON.parse(localStorage.getItem("voice-delivery:test")),
    ),
    null,
  );
  await page.getByText("Edit transcript before send").click();
  await page.getByRole("textbox").fill("Fixed legacy transcript");
  await page
    .getByRole("button", { name: "Send transcript", exact: true })
    .click();
  await page.waitForFunction(() => window.submits.length === 2);
  const legacyAttempts = await page.evaluate(() => window.submits);
  assert.equal(legacyAttempts[0].message_id, "legacy-attempt");
  assert.equal(legacyAttempts[0].edited_text, "");
  assert.notEqual(legacyAttempts[1].message_id, legacyAttempts[0].message_id);
  assert.equal(legacyAttempts[1].edited_text, "Fixed legacy transcript");
  console.log(
    "PASS: legacy blank attempt releases only after definitive validation rejection.",
  );
  for (const [body, rejection] of [
    ["", "Request rejected"],
    [
      "Valid original text",
      "Edited transcript must contain 1 to 32000 characters",
    ],
  ]) {
    await restoreAttempt(body, rejection);
    await page
      .getByRole("button", { name: "Send transcript", exact: true })
      .click();
    await page.getByText(rejection, { exact: true }).waitFor();
    assert.equal(
      await page.evaluate(
        () => JSON.parse(localStorage.getItem("voice-delivery:test")).id,
      ),
      "legacy-attempt",
    );
    await page.getByText("Edit transcript before send").click();
    await page.getByRole("textbox").fill("Different replacement");
    await page
      .getByRole("button", { name: "Send transcript", exact: true })
      .click();
    await page.waitForFunction(() => window.submits.length === 2);
    const rejectedAttempts = await page.evaluate(() => window.submits);
    assert.deepEqual(rejectedAttempts[1], rejectedAttempts[0]);
  }
  console.log(
    "PASS: generic 400 or a valid original body preserves the delivery identity.",
  );
} finally {
  await browser.close();
  await server.close();
}
