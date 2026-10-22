import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { test, expect } from "../playwright.mjs";

// No backend: exercise the real picker with a small, deterministic catalog.
test("native model group has one Tab stop and ArrowUp wraps from the selected model", async ({
  page,
}) => {
  test.setTimeout(120000);
  const root = join(import.meta.dirname, "../..");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const cacheDir = await mkdtemp(join(tmpdir(), "native-model-picker-"));
  const entry = join(root, "native-model-fixture.tsx");
  const server = await createServer({
    root,
    configFile: false,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "native-model-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/native-model-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/native-model-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React,{useState} from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider,Stack,Paper,Button} from '@mantine/core';
import '@mantine/core/styles.css';
import {AgentSetupPicker} from '/src/components/agents/AgentSetupPicker.tsx';
import {ModelField} from '/src/components/models/ModelField.tsx';
import '/src/components/agents/execution-settings.css';
function Fixture(){const[model,onModel]=useState('astra');return <MantineProvider forceColorScheme={new URLSearchParams(location.search).get('theme')||'light'}><Paper p="sm"><Stack gap="xs"><Button id="before">Before models</Button><AgentSetupPicker role="orchestrator" roles={[]} onRole={()=>{}} provider="codex" providers={[]} onProvider={()=>{}} accounts={[]} accountKey="" onAccount={()=>{}} models={[{value:'astra',label:'Astra 6',provider:'codex',badges:['default']},{value:'unavailable',label:'Unavailable model',provider:'codex',badges:[],disabled:true,disabledReason:'Unavailable on this account'},{value:'sol',label:'Sol 6.1',provider:'codex',badges:[]},{value:'opus',label:'Opus 4.6',provider:'claude',badges:[]}]} model={model} onModel={onModel} modelLabel="Model" efforts={[]} effort="" onEffort={()=>{}} effortLabel="Reasoning" disabled={false} fastAvailable={false} fast={false} onFast={()=>{}} daybreakAvailable={false} daybreak={false} onDaybreak={()=>{}}/><Button id="after">After models</Button><ModelField presentation="select" label="Compact model" value={model} onChange={onModel} options={[{value:'__model_default__',label:'Same as main agent',provider:'codex',badges:[]},{value:'astra',label:'Astra 6',provider:'codex',description:'General coding',badges:['default','current']},{value:'unavailable',label:'Unavailable model',provider:'codex',badges:[],disabledReason:'Unavailable on this account'},{value:'opus',label:'Opus 4.6',provider:'claude',description:'Careful reasoning',badges:[]}]}/></Stack></Paper></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  await server.listen();
  try {
    const url = server.resolvedUrls.local[0] + "check";
    await page.goto(url);
    await expect(page.locator("#model")).toBeVisible();
    if (process.env.NATIVE_MODEL_SCREENSHOTS) {
      await mkdir("/tmp/native-model-ui", { recursive: true });
      for (const width of [320, 390, 768, 1440]) {
        await page.setViewportSize({ width, height: 568 });
        for (const theme of ["light", "dark"]) {
          await page.goto(`${url}?theme=${theme}`);
          await expect(page.locator("#model")).toBeVisible();
          await page.screenshot({
            path: `/tmp/native-model-ui/${process.env.NATIVE_MODEL_SCREENSHOTS}-picker-${width}-${theme}.png`,
            fullPage: true,
          });
        }
      }
    }
    await page.locator("#before").focus();
    await page.keyboard.press("Tab");
    await page.keyboard.press("ArrowUp");
    await expect(page.locator('#model [data-value="opus"]')).toBeFocused();
    await expect(page.locator("#model")).toHaveAttribute("data-value", "opus");
    await page.keyboard.press("Tab");
    await expect(page.locator("#after")).toBeFocused();
    await page.keyboard.press("Shift+Tab");
    await expect(page.locator('#model [data-value="opus"]')).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(page.locator('#model [data-value="astra"]')).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(page.locator('#model [data-value="sol"]')).toBeFocused();
    await expect(page.locator("#model")).toHaveAttribute("data-value", "sol");
    const compact = page.getByRole("combobox", { name: "Compact model" });
    await expect(compact.locator("optgroup")).toHaveCount(2);
    await expect(compact.locator('optgroup[label="Claude"] option')).toHaveText(
      "Opus 4.6",
    );
    await expect(compact.locator('option[value="unavailable"]')).toBeDisabled();
    await expect(compact.locator('option[value="astra"]')).toHaveText(
      "Astra 6 (default, current)",
    );
    await compact.selectOption("__model_default__");
    await expect(compact).toHaveValue("__model_default__");
    await compact.selectOption("opus");
    await expect(
      page.getByText("Careful reasoning", { exact: true }),
    ).toBeVisible();
    await page.locator('#model [data-value="sol"]').click();
    await page.locator('#model [data-value="sol"]').focus();
    // Home/End follow the browser's native radio behavior; do not emulate it.
    await page.keyboard.press("Home");
    await page.keyboard.press("End");
    await expect(page.locator('#model [data-value="sol"]')).toBeFocused();
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
