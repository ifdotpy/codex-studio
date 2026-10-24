import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { test, expect } from "../playwright.mjs";

for (const width of [1440, 390]) {
  test(`execution settings preserve nested Escape and focus at ${width}px`, async ({
    page,
  }) => {
    test.setTimeout(90000);
    test.fail(
      width === 390,
      "Account-modal Escape also closes the footer Drawer on phones.",
    );
    const root = join(import.meta.dirname, "../..");
    const require = createRequire(join(root, "package.json"));
    const { createServer } = await import(require.resolve("vite"));
    const cacheDir = await mkdtemp(join(tmpdir(), "native-execution-escape-"));
    const entry = join(root, "escape-fixture.tsx");
    const server = await createServer({
      root,
      configFile: false,
      cacheDir,
      server: { host: "127.0.0.1", port: 0, hmr: false },
      plugins: [
        {
          name: "execution-escape",
          configureServer(server) {
            server.middlewares.use("/check", (_req, res) => {
              res.setHeader("Content-Type", "text/html");
              res.end(
                '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/escape-fixture.tsx"></script>',
              );
            });
          },
          resolveId(id) {
            if (id === "/escape-fixture.tsx") return entry;
          },
          load(id) {
            if (id !== entry) return;
            return `import React,{useState} from 'react';import{createRoot}from'react-dom/client';import{MantineProvider,Modal,Button}from'@mantine/core';import'@mantine/core/styles.css';import{ExecutionSettings}from'/src/components/agents/ExecutionSettings.tsx';
const agent={id:'escape',accountKey:'first',provider:'codex',name:'Lead',source:'managed',model:'gpt-6-astra',effort:'high',isLead:true,status:'idle',created:1};const models=[{model:'gpt-6-astra',supportedReasoningEfforts:[{reasoningEffort:'low'},{reasoningEffort:'high'}],serviceTiers:[]}];function Fixture(){const[parent,setParent]=useState(true),[opened,setOpened]=useState(false),[account,setAccount]=useState(false);window.escapeState={parent,opened,account};return <MantineProvider><Modal opened={parent} title="Chat settings" onClose={()=>setParent(false)} closeOnEscape={!opened&&!account}><ExecutionSettings agent={agent} catalog={{models,loading:false,error:'',retry:()=>{}}} accounts={[{id:'first',label:'First',status:'ready'},{id:'second',label:'Second',status:'ready'}]} onAccountChange={()=>setAccount(true)} onOpenChange={setOpened} accountModalOpen={account} refresh={async()=>{}}/></Modal><Modal opened={account} onClose={()=>setAccount(false)} title="Change account" zIndex={350}><Button>Confirm account</Button></Modal></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
          },
        },
      ],
    });
    await server.listen();
    try {
      await page.setViewportSize({ width, height: 844 });
      await page.goto(server.resolvedUrls.local[0] + "check");
      const parent = page.getByRole("dialog", {
        name: "Chat settings",
        exact: true,
      });
      const trigger = parent.getByRole("button", {
        name: "Main agent settings",
        exact: true,
      });
      await trigger.click();
      const picker = page.getByRole("dialog", {
        name: "Main agent settings",
        exact: true,
      });
      await expect(picker).toBeVisible();
      await picker.getByRole("radio", { name: "Astra 6", exact: true }).focus();
      await page.keyboard.press("Escape");
      await expect(picker).not.toBeVisible();
      await expect(parent).toBeVisible();
      await expect(trigger).toBeFocused();
      await trigger.click();
      await picker.locator('[data-account-key="second"]').click();
      const account = page.getByRole("dialog", {
        name: "Change account",
        exact: true,
      });
      await expect(account).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(account).not.toBeVisible();

      await expect(picker).toBeVisible();
      await expect(parent).toBeVisible();
      await picker.getByRole("radio", { name: "Astra 6", exact: true }).focus();
      await page.keyboard.press("Escape");
      await expect(picker).not.toBeVisible();
      await expect(parent).toBeVisible();
    } finally {
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    }
  });
}
