import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../../tests/playwright.mjs";

test("Workspace panel retains one form, scroll, focus, and modal ownership across hosts", async ({
  browser,
}, testInfo) => {
  const entry = `
    import React, {useEffect,useState} from 'react';
    import {createRoot} from 'react-dom/client';
    import {MantineProvider,Modal} from '@mantine/core';
    import {WorkspacePanelHost} from '/src/components/shell/WorkspacePanelHost.tsx';
    import '@mantine/core/styles.css';
    function Form(){
      const [draft,setDraft]=useState('');
      const [formOpened,setFormOpened]=useState(false);
      window.setFormOpened=setFormOpened;
      useEffect(()=>{window.mounts=(window.mounts||0)+1},[]);
      return React.createElement(React.Fragment,null, React.createElement(Modal,{opened:formOpened,onClose:()=>setFormOpened(false),title:'Rule form',closeButtonProps:{'aria-label':'Close rule'}},React.createElement('input',{'aria-label':'Rule draft'})), React.createElement('div',{className:'test-scroll',style:{height:200,overflow:'auto'}},
        React.createElement('textarea',{'aria-label':'Draft',value:draft,onChange:e=>setDraft(e.target.value)}),
        React.createElement('div',{style:{height:1000}},'Content')));
    }
    function App(){
      const [mode,setMode]=useState('inline');
      const [opened,setOpened]=useState(true);
      const [target,setTarget]=useState(null);
      const [width,setWidth]=useState(540);
      window.changeMode=setMode;window.setOpened=setOpened;
      return React.createElement(React.Fragment,null,
        React.createElement('button',{id:'outside'},'Conversation'),
        React.createElement('div',{ref:setTarget,id:'inline',style:{height:650,display:'flex'}}),
        React.createElement(WorkspacePanelHost,{opened,onClose:()=>setOpened(false),inlineTarget:target,
          panelLayout:{mode,width,minWidth:360,maxWidth:600,rootFontSize:16},
          onPanelWidthChange:rem=>{window.savedWidth=rem*16;setWidth(rem*16)},
          title:'Workspace',drawerSize:'min(52.5rem, 100vw)'},React.createElement(Form)));
    }
    createRoot(document.getElementById('root')).render(React.createElement(MantineProvider,null,React.createElement(App)));
  `;
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "workspace-panel-fixture",
        resolveId(id) {
          if (id === "/panel-entry") return "\0panel-entry";
        },
        load(id) {
          if (id === "\0panel-entry") return entry;
        },
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            if (request.url !== "/panel-fixture") return next();
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/panel-entry"></script>',
            );
          });
        },
      },
    ],
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 900 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/panel-fixture`,
    );
    const draft = page.getByRole("textbox", { name: "Draft" });
    await draft.fill("Keep this draft");
    await draft.evaluate((el) => {
      el.setSelectionRange(2, 6);
      window.originalInput = el;
      el.parentElement.scrollTop = 120;
    });
    await page.evaluate(() => window.changeMode("drawer"));
    await expect(page.getByRole("dialog", { name: "Workspace" })).toBeVisible();
    await expect(draft).toHaveValue("Keep this draft");
    await expect(draft).toBeFocused();
    expect(await draft.evaluate((el) => el === window.originalInput)).toBe(
      true,
    );
    expect(
      await draft.evaluate((el) => [
        el.selectionStart,
        el.selectionEnd,
        el.parentElement.scrollTop,
      ]),
    ).toEqual([2, 6, 120]);
    await page.evaluate(() => window.changeMode("inline"));
    await expect(page.locator("#inline").getByLabel("Draft")).toBeVisible();
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await expect(draft).toBeFocused();
    expect(await page.evaluate(() => window.mounts)).toBe(1);
    await page.getByRole("button", { name: "Conversation" }).click();
    await expect(
      page.getByRole("button", { name: "Conversation" }),
    ).toBeFocused();
    const resize = page.getByRole("separator", {
      name: "Workspace panel width",
    });
    await resize.focus();
    await resize.press("ArrowLeft");
    expect(await page.evaluate(() => window.savedWidth)).toBeGreaterThan(540);
    await resize.press("End");
    expect(await page.evaluate(() => window.savedWidth)).toBe(600);
    await page.screenshot({
      path: testInfo.outputPath("workspace-inline.png"),
    });
    await page.evaluate(() => window.changeMode("drawer"));
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByRole("dialog", { name: "Workspace" })).toBeVisible();
    await expect(
      page.getByRole("dialog", { name: "Workspace" }),
    ).toBeInViewport({ ratio: 0.99 });
    await page.screenshot({
      path: testInfo.outputPath("workspace-drawer.png"),
    });
    await page.keyboard.press("Escape");
    await expect(draft).toBeHidden();
    await page.evaluate(() => window.setOpened(true));
    await expect(draft).toHaveValue("Keep this draft");
    expect(await page.evaluate(() => window.mounts)).toBe(1);
    await page.evaluate(() => window.changeMode("inline"));
    await expect(page.locator("#inline").getByLabel("Draft")).toBeVisible();
    await page.evaluate(() => window.setFormOpened(true));
    const formDraft = page.getByRole("textbox", { name: "Rule draft" });
    await formDraft.fill("Keep the rule draft");
    await page.evaluate(() => window.changeMode("drawer"));
    await expect(
      page.getByRole("dialog", { name: "Workspace" }),
    ).toBeInViewport({ ratio: 0.99 });
    await expect(formDraft).toBeFocused();
    await expect(formDraft).toHaveValue("Keep the rule draft");
    await formDraft.press("Tab");
    await expect(
      page
        .getByRole("dialog", { name: "Rule form" })
        .getByRole("button", { name: "Close rule" }),
    ).toBeFocused();
    await page.evaluate(() => window.changeMode("inline"));
    await expect(formDraft).toHaveValue("Keep the rule draft");
    expect(await page.evaluate(() => window.mounts)).toBe(1);
    expect(errors).toEqual([]);
  } finally {
    await page.close();
    await server.close();
  }
});
