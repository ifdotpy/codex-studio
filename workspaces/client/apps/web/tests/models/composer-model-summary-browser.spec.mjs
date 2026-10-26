import { join } from "node:path";
import { mkdir, writeFile } from "node:fs/promises";
import { createFixtureViteServer, test, expect } from "../playwright.ts";

// Use the composer's real trigger, settings caller, toolbar controls and CSS.
// Only the catalog is a fixture; opening and switching roles use production UI.
test("composer model summary wraps on phones and opens the role settings", async ({
  page,
}) => {
  test.setTimeout(120000);
  const root = join(import.meta.dirname, "../..");
  const entry = join(root, "composer-model-summary-fixture.tsx");
  const server = await createFixtureViteServer({
    root,
    configFile: false,
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "composer-model-summary-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/composer-model-summary-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/composer-model-summary-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider,ActionIcon,Textarea} from '@mantine/core';
import {Paperclip,Mic,Square,ArrowUp,ListEnd} from 'lucide-react';
import '@mantine/core/styles.css';
import '/src/style.css';
import '/src/appearance.css';
import '/src/components/chat-controls.css';
import '/src/components/conversation/conversation-chrome.css';
import '/src/components/conversation/empty-chat-settings.css';
import {theme} from '/src/theme.ts';
import {ExecutionSummaryTrigger} from '/src/components/agents/ExecutionSummaryTrigger.tsx';
import {ExecutionSettings} from '/src/components/agents/ExecutionSettings.tsx';
import {ModelCatalogReader} from '/src/components/agents/WorkerModelPicker.tsx';
const query=new URLSearchParams(location.search);
const name=query.get('long')==='word'?'ExtremelyLongUnbrokenModelIdentifierForTheComposerRegression':query.has('long')?'Opus extended context model with a long descriptive label':'Opus 5.5';
const models=[{model:'opus',displayName:name,provider:'claude',supportedReasoningEfforts:[],serviceTiers:[]},{model:'sol',displayName:'Sol 6.1',provider:'codex',supportedReasoningEfforts:[],serviceTiers:[]}];
const reader={scope:'composer-model-summary',read:async()=>({data:models}),subscribeState:accept=>{accept({runtime:{modelsByAccount:{default:models.map((row,catalogPosition)=>({...row,accountKey:'default',catalogPosition}))}},entitiesComplete:true});return()=>{}}};
const lead={id:'lead',isLead:true,source:'managed',accountKey:'default',provider:'claude',model:'opus',effort:'high',status:query.has('active')?'running':'idle',workerDefaults:{model:'sol',effort:'high'},reviewDefaults:{model:null,effort:null}};
const agent=query.has('worker')?{...lead,id:'worker',isLead:false,rootId:'lead'}:lead;
function Fixture(){return <MantineProvider theme={theme} forceColorScheme={query.get('theme')||'dark'}><ModelCatalogReader.Provider value={reader}><main style={{padding:8,width:'100%',maxWidth:1200,margin:'auto',alignSelf:'end'}}><form id="composer" onSubmit={e=>e.preventDefault()}><Textarea id="message" aria-label="Message" placeholder="What should we work on?"/><div className="composer-bar"><div className="composer-tools"><ActionIcon aria-label="Attach files"><Paperclip/></ActionIcon><div className="empty-chat-settings composer-context-row" aria-label="Agent settings"><ExecutionSettings agent={agent} lead={lead} catalog={{models,loading:false,error:'',retry:()=>{}}} refresh={async()=>{}} renderTrigger={({agent,opened,ref,onClick})=><ExecutionSummaryTrigger agent={agent} lead={lead} team={[lead,agent]} ref={ref} aria-expanded={opened} onOpen={onClick}/>}/></div></div><div className="composer-submit-actions"><ActionIcon aria-label="Dictate"><Mic/></ActionIcon><ActionIcon className="composer-queue-primary" aria-label="Queue for next turn"><ListEnd/></ActionIcon>{query.has('active')&&<ActionIcon id="stop" aria-label="Stop"><Square/></ActionIcon>}<ActionIcon id="send" aria-label="Send message"><ArrowUp/></ActionIcon></div></div></form></main></ModelCatalogReader.Provider></MantineProvider>};
createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  await server.listen();
  try {
    const screenshots = process.env.COMPOSER_SUMMARY_SCREENSHOTS;
    if (screenshots) await mkdir(screenshots, { recursive: true });
    const measurements = [];
    const url = server.resolvedUrls.local[0] + "check";
    for (const width of [320, 360, 390, 1440]) {
      await page.setViewportSize({ width, height: 844 });
      for (const scenario of [
        "long=word&worker=1",
        "long=words&active=1",
        "",
      ]) {
        await page.goto(`${url}?${scenario}`);
        const trigger = page.getByRole("button", {
          name: /^Execution settings:/,
        });
        await expect(trigger).toBeVisible();
        await expect(trigger).toHaveAccessibleName(/Review same as caller/);
        const filename = `composer-${width}-${scenario.replaceAll(/[^a-z]/g, "") || "normal"}.png`;
        await page.screenshot({
          path: screenshots
            ? join(screenshots, filename)
            : test.info().outputPath(filename),
        });
        const geometry = await trigger.evaluate((node) => {
          const bounds = node.getBoundingClientRect();
          const texts = [
            ...node.querySelectorAll(
              ".execution-model,.execution-inherited-label",
            ),
          ].filter((el) => el.getBoundingClientRect().height);
          return {
            pageWidth: document.documentElement.scrollWidth,
            viewport: innerWidth,
            overflow: node.scrollWidth - node.clientWidth,
            escaped: texts.some((el) => {
              const r = el.getBoundingClientRect();
              return r.left < bounds.left || r.right > bounds.right + 1;
            }),
            // Detect overlaps with the actual neighboring toolbar controls.
            overlap: [...document.querySelectorAll("#composer button")]
              .filter((el) => el !== node)
              .some((el) => {
                const r = el.getBoundingClientRect();
                return (
                  r.width &&
                  r.left < bounds.right &&
                  r.right > bounds.left &&
                  r.top < bounds.bottom &&
                  r.bottom > bounds.top
                );
              }),
          };
        });
        expect(
          geometry,
          JSON.stringify({ width, scenario, ...geometry }),
        ).toMatchObject({ overflow: 0, escaped: false, overlap: false });
        expect(geometry.pageWidth).toBeLessThanOrEqual(geometry.viewport);
        measurements.push({ width, scenario, ...geometry });
        await expect(trigger.locator(".execution-provider")).toHaveCount(0);
        await expect(trigger.locator("svg")).toHaveCount(0);
        await expect(
          trigger.locator(".execution-inherited-label"),
        ).toBeVisible();
        await trigger.focus();
        await page.keyboard.press("Enter");
        await expect(trigger).toHaveAttribute("aria-expanded", "true");
        const roles = page.getByRole("radiogroup", {
          name: "Agent role",
          exact: true,
        });
        await expect(roles).toBeVisible();
        for (const [role, label, selected] of [
          ["Lead", /^(Main agent|Subagent) model$/, "opus"],
          [
            "Workers",
            /^(Default subagent|Subagent) model$/,
            scenario.includes("worker=1") ? "opus" : "sol",
          ],
          ["Review", "Default review model", "__model_default__"],
        ]) {
          await roles.getByText(role, { exact: true }).click();
          const models = page.getByRole("listbox", {
            name: label,
            exact: true,
          });
          await expect(models).toBeVisible();
          await expect(
            models.locator(`[role="option"][data-value="${selected}"]`),
          ).toHaveAttribute("aria-selected", "true");
        }
        // Reload closes either the mobile sheet or desktop popover for the next case.
      }
    }
    if (screenshots) {
      await writeFile(
        join(screenshots, "measurements.json"),
        JSON.stringify(measurements, null, 2),
      );
      for (const width of [320, 360, 390, 1440]) {
        await page.setViewportSize({ width, height: 844 });
        for (const theme of ["light", "dark"]) {
          await page.goto(`${url}?theme=${theme}`);
          await expect(
            page.locator(".execution-summary-trigger"),
          ).toBeVisible();
          await page.screenshot({
            path: join(screenshots, `composer-${width}-${theme}.png`),
          });
        }
      }
    }
  } finally {
    await server.close();
  }
});
