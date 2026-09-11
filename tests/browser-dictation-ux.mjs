// Real React and storage, simulated browser speech. No audio or service requests.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { resolve } from "node:path";
const root = resolve(import.meta.dirname, "../web");
const require = createRequire(root + "/package.json");
const { createServer } = await import(require.resolve("vite"));
const { chromium } = require("playwright-core");
const entry = `import React,{useState}from'react';import{createRoot}from'react-dom/client';import{MantineProvider}from'@mantine/core';import'@mantine/core/styles.css';import{Dictation}from'/src/components/Dictation.tsx';import'/src/components/realtime-voice.css';
window.calls=[];window.SpeechRecognition=class {constructor(){window.speech=this;}start(){window.calls.push('start');this.onstart?.();}stop(){window.calls.push('stop');this.onend?.();}abort(){window.calls.push('abort');this.onend?.();}};
if(location.search.includes('unsupported')){window.SpeechRecognition=undefined;window.webkitSpeechRecognition=undefined;}
function App(){const[draft,setDraft]=useState(''),[chat,setChat]=useState('a');return <MantineProvider><button onClick={()=>setChat(chat==='a'?'b':'a')}>Switch chat</button><textarea aria-label='Draft' value={draft} readOnly/><Dictation key={chat} chatId={chat} onInsert={setDraft}/></MantineProvider>}createRoot(document.getElementById('root')).render(<App/>);`;
const server = await createServer({
  configFile: false,
  root,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "browser-speech-fixture",
      configureServer(s) {
        s.middlewares.use("/audit", (_, r) => {
          r.setHeader("Content-Type", "text/html");
          r.end(
            '<div id="root" style="padding:140px 8px"></div><script type="module" src="/browser-entry.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/browser-entry.tsx") return root + id;
      },
      load(id) {
        if (id === root + "/browser-entry.tsx") return entry;
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
  const page = await browser.newPage({ viewport: { width: 390, height: 740 } });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  const url = server.resolvedUrls.local[0] + "audit";
  await page.goto(url);
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  await page.getByRole("button", { name: "Record", exact: true }).click();
  await page.evaluate(() =>
    window.speech.onresult({
      results: [
        { isFinal: true, 0: { transcript: "Recognized text" } },
        { isFinal: false, 0: { transcript: "unfinished" } },
      ],
    }),
  );
  assert.equal(
    await page
      .getByRole("textbox", { name: "Draft", exact: true })
      .inputValue(),
    "",
  );
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  await page
    .getByRole("button", { name: "Stop dictation", exact: true })
    .click();
  assert.ok((await page.evaluate(() => window.calls)).includes("stop"));
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  assert.equal(
    await page
      .getByRole("textbox", { name: "Review dictated text" })
      .inputValue(),
    "Recognized text",
  );
  await page
    .getByRole("textbox", { name: "Review dictated text" })
    .fill("Corrected text");
  await page.getByRole("button", { name: "Insert into message" }).click();
  assert.equal(
    await page
      .getByRole("textbox", { name: "Draft", exact: true })
      .inputValue(),
    "Corrected text",
  );
  await page.reload();
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  assert.equal(
    await page
      .getByRole("textbox", { name: "Review dictated text" })
      .inputValue(),
    "Corrected text",
  );
  await page.getByRole("button", { name: "Record", exact: true }).click();
  await page.getByRole("button", { name: "Switch chat" }).click();
  assert.ok((await page.evaluate(() => window.calls)).includes("abort"));
  await page.goto(url + "?unsupported");
  await page.getByRole("button", { name: "Dictation", exact: true }).click();
  await page
    .getByText("Dictation is unavailable in this browser.", { exact: false })
    .waitFor();
  assert.equal(
    await page
      .getByRole("button", { name: "Record", exact: true })
      .isDisabled(),
    true,
  );
  assert.deepEqual(await page.evaluate(() => window.calls), []);
  assert.deepEqual(errors, []);
  console.log(
    "PASS: browser/mobile speech capability, final text review, explicit insert, storage, stop outside panel, chat cleanup, unsupported browser.",
  );
} finally {
  await browser.close();
  await server.close();
}
