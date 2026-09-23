import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
const root = resolve(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cacheDir = await mkdtemp(join(tmpdir(), "studio-radio-"));
const entry = join(root, "radio-fixture.tsx");
const server = await createServer({
  configFile: false,
  root,
  cacheDir,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "radio-fixture",
      configureServer(s) {
        s.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<html><body><div id="root"></div><script type="module" src="/radio-fixture.tsx"></script></body></html>',
          );
        });
      },
      resolveId(id) {
        if (id === "/radio-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `
import React,{useState} from 'react'; import {createRoot} from 'react-dom/client'; import {MantineProvider} from '@mantine/core'; import '@mantine/core/styles.css';
import RadioChat from '/src/components/RadioChat.tsx';import {PeerTeamGroup} from '/src/components/PeerTeams.tsx';import '/src/style.css';import '/src/appearance.css';import {theme} from '/src/theme.ts';
const team={id:'t',name:'Research',projectPath:'/p',members:['a','b']};
const room={id:'radio:t',name:'Research',kind:'private',projectPath:'/p',peerTeamId:'t',members:['a','b'],radio:{teamId:'t',revision:1,status:'idle',speaker:null,next:[],active:null,error:null}};
const initial={stateDir:'radio-test',threads:[{id:'a',name:'Astra',model:'gpt-6-astra',provider:'codex'},{id:'b',name:'Claude',model:'claude-opus-5-5',provider:'claude'}],runtime:{requests:[],peerTeams:[team]}};
function Fixture(){const[data,setData]=useState(initial);const[value,setRoom]=useState(room);const[draft,setDraft]=useState('');const[roomId,setRoomId]=useState(undefined);window.setRoom=setRoom;window.currentRoom=value;window.opened=null;
const refresh=async()=>{if(window.failRefresh)throw Error('Refresh failed');const response=await fetch('/snapshot');setRoom(await response.json());setRoomId('radio:t');};
return <MantineProvider theme={theme} defaultColorScheme="dark"><div style={{height:'100%',display:'flex',flexDirection:'column',width:'100%'}}><PeerTeamGroup scope="radio-test" team={team} closed={false} toggle={()=>{}} edit={()=>{}} dissolve={()=>{}} roomId={roomId} openRoom={id=>window.opened=id} refresh={refresh}><></></PeerTeamGroup><RadioChat room={value} data={data} draft={draft} setDraft={setDraft} refresh={refresh} notify={console.error}/></div></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1100, height: 850 },
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  let mode = "lost";
  const requests = [];
  await page.route("**/api/agent-chat?*", (r) =>
    r.fulfill({
      json: {
        messages: [
          {
            id: "1",
            seq: 1,
            sender: "user",
            text: "Compare these approaches",
            created: 1750000000,
          },
          {
            id: "2",
            seq: 2,
            sender: "a",
            senderName: "Astra",
            text: "First reply. **Keep this visible.**",
            created: 1750000010,
          },
          {
            id: "3",
            seq: 3,
            sender: "b",
            senderName: "Claude",
            text: "Second reply with a different view.",
            created: 1750000020,
          },
        ],
        nextBefore: null,
      },
    }),
  );
  await page.route("**/snapshot", async (r) =>
    r.fulfill({ json: await page.evaluate(() => window.currentRoom) }),
  );
  await page.route("**/api/peer-teams", async (r) => {
    requests.push(r.request().postDataJSON());
    if (mode === "lost") {
      mode = "ok";
      return r.abort("failed");
    }
    if (mode === "reject")
      return r.fulfill({ status: 409, json: { error: "Revision changed" } });
    return r.fulfill({ json: { room: { id: "radio:t" } } });
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.getByText("Second reply with a different view.").waitFor();
  assert.deepEqual(
    await page
      .locator(".radio-message")
      .evaluateAll((es) => es.map((e) => e.getAttribute("data-sender"))),
    ["user", "a", "b"],
  );
  assert.equal(await page.locator(".radio-message time[datetime]").count(), 3);
  await page.getByRole("button", { name: "Options for team Research" }).click();
  await page.getByRole("menuitem", { name: "Open shared chat" }).click();
  await page.getByRole("button", { name: "Retry", exact: true }).waitFor();
  assert.equal(await page.evaluate(() => window.opened), null);
  await page.getByRole("button", { name: "Retry", exact: true }).click();
  await page.waitForFunction(() => window.opened === "radio:t");
  assert.equal(requests.length, 2);
  assert.deepEqual(requests[0], requests[1]);
  requests.length = 0;
  mode = "lost";
  const input = page.getByRole("textbox", { name: "Message both agents" });
  await input.fill("Hello both");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await page.getByRole("button", { name: "Retry", exact: true }).waitFor();
  assert.equal(await input.inputValue(), "Hello both");
  assert.equal(await input.isDisabled(), true);
  await page.reload();
  await page.getByRole("button", { name: "Retry", exact: true }).click();
  await page.waitForFunction(() => !document.querySelector(".radio-retry"));
  assert.equal(requests.length, 2);
  assert.deepEqual(requests[0], requests[1]);
  assert.equal(requests[0].rounds, 1);
  assert.equal(requests[0].target, "both");
  await input.fill("Discuss tradeoffs");
  await page.evaluate(() => (window.failRefresh = true));
  await page
    .getByRole("button", { name: "Discuss (4 replies)", exact: true })
    .click();
  await page.getByRole("button", { name: "Refresh", exact: true }).waitFor();
  assert.equal(await input.inputValue(), "");
  const posted = requests.length;
  await page.evaluate(() => (window.failRefresh = false));
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await page.waitForFunction(() => !document.querySelector(".radio-retry"));
  assert.equal(requests.length, posted);
  assert.equal(requests.at(-1).rounds, 2);
  mode = "reject";
  await input.fill("Stale proposal");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await page.getByRole("button", { name: "Refresh", exact: true }).waitFor();
  const rejected = requests.length;
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await page.waitForFunction(() => !document.querySelector(".radio-retry"));
  assert.equal(requests.length, rejected);
  assert.equal(await input.inputValue(), "Stale proposal");
  mode = "ok";
  await page.evaluate(() =>
    window.setRoom({
      ...window.currentRoom,
      radio: { ...window.currentRoom.radio, status: "speaking", speaker: "a" },
    }),
  );
  await page.getByRole("button", { name: "Reply next" }).click();
  await page.waitForFunction(() => !document.querySelector(".radio-retry"));
  assert.equal(requests.at(-1).radio_action, "pass");
  assert.equal(requests.at(-1).target, "b");
  await page.getByRole("button", { name: "Stop", exact: true }).click();
  await page.waitForFunction(() => !document.querySelector(".radio-retry"));
  assert.equal(requests.at(-1).radio_action, "stop");
  assert.equal(
    await page.getByRole("button", { name: "Send", exact: true }).isEnabled(),
    true,
  );
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await page.waitForFunction(() => !document.querySelector(".radio-retry"));
  assert.equal(requests.at(-1).radio_action, "send");
  await input.fill("New instruction");
  await page.evaluate(() =>
    window.setRoom({
      ...window.currentRoom,
      radio: {
        ...window.currentRoom.radio,
        status: "blocked",
        error: "Reply interrupted. Send a new message.",
        active: null,
        speaker: null,
      },
    }),
  );
  assert.equal(
    await page.getByRole("button", { name: "Send", exact: true }).isEnabled(),
    true,
  );
  await page.evaluate(() =>
    window.setRoom({
      ...window.currentRoom,
      radio: {
        ...window.currentRoom.radio,
        status: "blocked",
        active: { eventId: "e", agentId: "a" },
        speaker: "a",
      },
    }),
  );
  assert.equal(
    await page.getByRole("button", { name: "Send", exact: true }).isDisabled(),
    true,
  );
  assert.equal(
    await page.getByRole("button", { name: "Stop", exact: true }).isEnabled(),
    true,
  );
  await page.evaluate(() =>
    window.setRoom({
      ...window.currentRoom,
      radio: {
        ...window.currentRoom.radio,
        status: "speaking",
        error: null,
        active: { eventId: "e", agentId: "a" },
        speaker: "a",
      },
    }),
  );
  await page.getByRole("button", { name: "Options for team Research" }).click();
  await page.getByRole("menuitem", { name: "Open shared chat" }).click();
  await page.waitForFunction(() => window.opened === "radio:t");
  await page.getByRole("menu").waitFor({ state: "hidden" });
  for (const width of [1100, 375]) {
    await page.setViewportSize({ width, height: 850 });
    assert.equal(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
      true,
    );
    assert.equal(
      await page
        .getByRole("button", { name: "Send", exact: true })
        .evaluate(
          (e) =>
            e.getBoundingClientRect().bottom <=
            document.getElementById("root").getBoundingClientRect().bottom,
        ),
      true,
    );
    await page.screenshot({
      path: join(tmpdir(), `studio-radio-${width}.png`),
      fullPage: true,
    });
  }
  assert.deepEqual(errors, []);
  console.log(
    `PASS radio UI: ordered transcript, timestamps, durable exact retry, acknowledged refresh, rejected revision, pass, stop, open and responsive layout. Screenshots ${tmpdir()}/studio-radio-{1100,375}.png`,
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
