// Isolated real React and IndexedDB. Audio capture and native speech are mocks.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { resolve } from "node:path";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { createServer } = await import(require.resolve("vite"));
const { chromium } = require("playwright-core");
const root = resolve(import.meta.dirname, "../web");
const server = await createServer({
  configFile: false,
  root,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "dictation-fixture",
      configureServer(server) {
        server.middlewares.use("/dictation-test", (_request, response) => {
          response.setHeader("Content-Type", "text/html");
          response.end(
            '<html><body><div id="root"></div><script type="module" src="/dictation-entry.tsx"></script></body></html>',
          );
        });
      },
      resolveId(id) {
        if (id === "/dictation-entry.tsx") return root + "/dictation-entry.tsx";
      },
      load(id) {
        if (id === root + "/dictation-entry.tsx")
          return `import React,{useState} from 'react';import{createRoot}from'react-dom/client';import{MantineProvider}from'@mantine/core';import'@mantine/core/styles.css';import'/src/studio-theme.css';import{theme}from'/src/theme.ts';import{Dictation}from'/src/components/Dictation.tsx';
 window.codexDesktop={requestMicrophone:async()=>true,prepareTranscription:async()=> 'permit',transcribeAudio:async(value)=>{window.audioBytes=value.audio.byteLength;if(window.deferTranscription)return new Promise(resolve=>window.finishTranscription=resolve);if(!window.retryOK)throw Error('Recognition unavailable; recording preserved.');return {text:'Recovered spoken instruction'};}};
 function App(){const[chat,setChat]=useState('first'),[draft,setDraft]=useState('Original draft');return <MantineProvider theme={theme} forceColorScheme="dark"><button onClick={()=>setChat(chat==='first'?'second':'first')}>Switch chat</button><textarea aria-label="Draft" value={draft} onChange={e=>setDraft(e.target.value)}/><Dictation key={chat} chatId={chat} onInsert={text=>setDraft(current=>current+'\\n'+text)}/></MantineProvider>}createRoot(document.getElementById('root')).render(<App/>);`;
        if (id.endsWith("/dictation/capture.ts"))
          return `export async function captureAudio(onChunk){window.emitAudio=()=>onChunk(new Int16Array(16000).fill(12));return {sampleRate:16000,stop:async()=>{window.captureStopped=(window.captureStopped||0)+1;if(window.failStop)throw Error("Final chunk not confirmed. Earlier audio saved.");}}}`;
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
  const page = await browser.newPage({ viewport: { width: 700, height: 850 } });
  const errors = [];
  page.on("pageerror", (e) => {
    errors.push(e.message);
    console.error(e.message);
  });
  page.on("console", (message) => {
    if (message.type() === "error") console.error(message.text());
  });
  const origin = server.resolvedUrls.local[0];
  await page.goto(origin + "dictation-test");
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  await page.getByRole("button", { name: "Record", exact: true }).click();
  await page.getByRole("button", { name: /Stop ·/ }).waitFor();
  await page.getByRole("button", { name: /Stop ·/ }).waitFor();
  await page.evaluate(() => window.emitAudio());
  await page.getByRole("button", { name: /Stop ·/ }).click();
  await page.getByRole("button", { name: "Transcribe", exact: true }).click();
  await page
    .getByText("Recognition unavailable; recording preserved.")
    .waitFor();
  assert.equal(
    await page.getByRole("textbox", { name: "Draft" }).inputValue(),
    "Original draft",
  );
  assert.equal(await page.locator("audio").count(), 1);
  assert.equal(await page.evaluate(() => window.audioBytes), 32044);
  await page.reload();
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  await page
    .getByText("Recognition unavailable; recording preserved.")
    .waitFor();
  assert.equal(await page.locator("audio").count(), 1);
  await page.evaluate(() => (window.retryOK = true));
  await page.getByRole("button", { name: "Retry transcription" }).click();
  await page
    .getByText("Recovered spoken instruction", { exact: true })
    .waitFor();
  assert.equal(
    await page.getByRole("textbox", { name: "Draft" }).inputValue(),
    "Original draft",
  );
  await page.setViewportSize({ width: 320, height: 760 });
  await page.screenshot({ path: "/tmp/studio-dictation-mobile.png" });
  await page.setViewportSize({ width: 700, height: 850 });
  await page.getByRole("button", { name: "Insert into message" }).click();
  assert.equal(
    await page.getByRole("textbox", { name: "Draft" }).inputValue(),
    "Original draft\nRecovered spoken instruction",
  );
  await page.getByRole("button", { name: "Switch chat" }).click();
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  assert.equal(await page.locator("audio").count(), 0);
  await page.getByRole("button", { name: "Record", exact: true }).click();
  await page.getByRole("button", { name: /Stop ·/ }).waitFor();
  await page.getByRole("button", { name: /Stop ·/ }).waitFor();
  await page.evaluate(() => window.emitAudio());
  await page.getByRole("button", { name: "Switch chat" }).click();
  await page.waitForFunction(() => window.captureStopped >= 1);
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  await page.getByRole("button", { name: "Delete recording" }).click();
  await page.waitForFunction(
    () => document.querySelectorAll("audio").length === 0,
  );
  // Reload during capture simulates a renderer loss. Periodic chunks survive.
  await page.getByRole("button", { name: "Record", exact: true }).click();
  await page.getByRole("button", { name: /Stop ·/ }).waitFor();
  await page.evaluate(() => window.emitAudio());
  await page.waitForFunction(async () => {
    const { listRecordings } = await import("/src/dictation/storage.ts");
    return (await listRecordings("first")).some((row) => row.samples === 16000);
  });
  await page.reload();
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  await page.getByText(/Recovered$/).waitFor();
  assert.equal(await page.locator("audio").count(), 1);
  // The original component can finish after this recording was deleted elsewhere.
  await page.evaluate(() => (window.deferTranscription = true));
  await page.getByRole("button", { name: "Transcribe", exact: true }).click();
  await page.waitForFunction(() => !!window.finishTranscription);
  await page.getByRole("button", { name: "Switch chat" }).click();
  await page.getByRole("button", { name: "Switch chat" }).click();
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  await page.getByRole("button", { name: "Delete recording" }).click();
  await page.waitForFunction(
    () => document.querySelectorAll("audio").length === 0,
  );
  await page.evaluate(() =>
    window.finishTranscription({ text: "Late response" }),
  );
  await page.waitForTimeout(100);
  assert.equal(
    await page.evaluate(async () => {
      const store = await import("/src/dictation/storage.ts");
      return (await store.listRecordings("first")).length;
    }),
    0,
  );
  // A newer attempt owns the result, even when the older attempt finishes last.
  assert.equal(
    await page.evaluate(async () => {
      const store = await import("/src/dictation/storage.ts");
      const row = {
        id: "attempt-race",
        chatId: "first",
        created: Date.now(),
        sampleRate: 16000,
        samples: 1,
        state: "ready",
      };
      await store.saveRecording(row);
      await store.beginTranscription(row, "old");
      await store.beginTranscription(row, "new");
      await store.updateRecording(
        { ...row, transcriptionAttempt: "new", transcript: "New result" },
        "new",
      );
      await store.updateRecording(
        { ...row, transcriptionAttempt: "old", transcript: "Old result" },
        "old",
      );
      const result = (await store.listRecordings("first"))[0].transcript;
      await store.deleteRecording(row.id);
      return result;
    }),
    "New result",
  );
  await page.getByRole("button", { name: "Record", exact: true }).click();
  await page.getByRole("button", { name: /Stop ·/ }).waitFor();
  await page.evaluate(() => {
    window.emitAudio();
    window.failStop = true;
  });
  await page.getByRole("button", { name: /Stop ·/ }).click();
  await page
    .getByText("Final chunk not confirmed. Earlier audio saved.", {
      exact: true,
    })
    .first()
    .waitFor();
  assert.equal(
    await page.getByRole("button", { name: "Record", exact: true }).isEnabled(),
    true,
  );
  assert.equal(await page.locator("audio").count(), 1);
  await page.reload();
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  await page
    .getByText("Final chunk not confirmed. Earlier audio saved.", {
      exact: true,
    })
    .waitFor();
  assert.deepEqual(errors, []);
  console.log(
    "PASS: recording chunks, WAV, failure, reload, retry, explicit draft insertion, chat isolation, unmount stop, deletion. No microphone or recognition calls.",
  );
} finally {
  await browser.close();
  await server.close();
}
