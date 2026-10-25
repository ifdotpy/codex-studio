import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../tests/playwright.mjs";

for (const width of [320, 1200]) {
  test(`project deletion menu and confirmation at ${width}px`, async ({
    browser,
  }) => {
    const entry = `
      import React, {useState} from 'react';
      import {createRoot} from 'react-dom/client';
      import {MantineProvider} from '@mantine/core';
      import Sidebar from '/src/components/Sidebar.tsx';
      import ProjectGroupsSidebar from '/src/servers/ProjectGroupsSidebar.tsx';
      import {ProjectDeleteDialog} from '/src/components/ProjectDeleteDialog.tsx';
      import {useProjectDelete} from '/src/useProjectDelete.ts';
      import '@mantine/core/styles.css';
      import '/src/components/sidebar-projects.css';
      const project = {id:'project',name:'Example',path:'/project',homeServerId:'home',projectSeq:12};
      const agent = {id:'lead',cwd:'/project',source:'managed',isLead:true,name:'Chat',status:'paused',projectId:'project',projectServerId:'home'};
      const initial = {stateDir:'fixture',entitiesComplete:true,threads:[agent],chats:[],nodes:[agent],edges:[],runtime:{agents:[agent],projects:[project],rooms:[],requests:[],tasks:[],monitors:[],complaints:[],rules:[],peerTeams:[],events:[],work:[],projectOrganizationVersion:1,peerTeamsVersion:1}};
      window.posts=[]; window.finished=[]; window.suspended=[]; window.fail=false;
      const store = new Map();
      const backend = {ownerId:'owner',storage:{getItem:key=>store.get(key)||null,setItem:(key,value)=>store.set(key,value),removeItem:key=>store.delete(key)},saved:(key,value)=>JSON.parse(store.get(key)||'null')??value,save:()=>{},editPreference:(_key,_before,after)=>after,subscribePreference:()=>()=>{},post:async(path,body)=>{
        window.posts.push({path,body});
        if(window.fail) throw new TypeError('response lost');
        return {id:body.id,request_id:body.request_id,outcome:'deleted',deletedAgentIds:['lead'],deletedRoomIds:[]};
      }};
      const services={cache:backend,owner:()=>backend,byOwner:()=>backend,serverForChat:()=> 'owner'};
      function Fixture(){
        const [data,setData]=useState(initial);
        const [,setScope]=useState(new Set());
        window.retire=()=>setData({...initial,threads:[],nodes:[],runtime:{...initial.runtime,agents:[],projects:[]}});
        const deletion=useProjectDelete({data,workspaceId:'workspace',services,onSuspend:scope=>{window.suspended=[...scope];setScope(scope)},onResume:()=>setScope(new Set()),onFinish:ids=>{window.finished.push(ids);setScope(new Set())},notify:message=>{window.notice=message}});
        const logical=location.search.includes('logical');
        return React.createElement(React.Fragment,null,
          logical? React.createElement(ProjectGroupsSidebar,{navigation:{owner:{projects:data.runtime.projects,chats:data.threads.map(chat=>({...chat,path:chat.cwd,archived:false})),ready:true,opened:'lead',alerts:[],error:''}},servers:[],statuses:{},current:'owner',query:'',hideOldChatsThreshold:20,send:(owner,command)=>{window.owner=owner;if(command.action==='delete-project')deletion.open(command.projectId)}}):
          React.createElement(Sidebar,{services,data,opened:'lead',indicators:new Map(),markingRead:new Set(),creating:false,mobile:true,onSearch:()=>{},close:()=>{},open:()=>{},newChat:()=>{},addProject:()=>{},changeProject:()=>{},projectAccount:()=>{},rename:async()=>{},remove:()=>{},markUnread:()=>{},deleteProject:path=>deletion.open(path),refresh:async()=>{throw new Error('Unexpected refresh')}}),
          React.createElement(ProjectDeleteDialog,deletion.dialog));
      }
      createRoot(document.getElementById('root')).render(React.createElement(React.StrictMode,null,React.createElement(MantineProvider,null,React.createElement(Fixture))));
    `;
    const server = await createFixtureViteServer({
      configFile: false,
      root: fileURLToPath(new URL("../..", import.meta.url)),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "project-delete-fixture",
          resolveId(id) {
            if (id === "/project-delete-entry") return "\0project-delete-entry";
          },
          load(id) {
            if (id === "\0project-delete-entry") return entry;
          },
          configureServer(server) {
            server.middlewares.use((request, response, next) => {
              if (!request.url?.startsWith("/project-delete-fixture"))
                return next();
              response.setHeader("Content-Type", "text/html");
              response.end(
                '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/project-delete-entry"></script>',
              );
            });
          },
        },
      ],
    });
    const page = await browser.newPage({
      viewport: { width, height: 900 },
      hasTouch: width === 320,
    });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    try {
      await server.listen();
      const url = `http://127.0.0.1:${server.httpServer.address().port}/project-delete-fixture`;
      for (const variant of ["", "?logical"]) {
        await page.goto(url + variant);
        const options = page.getByRole("button", {
          name: "Options for project Example",
        });
        await options.focus();
        await page.keyboard.press("Enter");
        const item = page.getByRole("menuitem", { name: "Delete project…" });
        await expect(item).toBeVisible();
        await item.focus();
        await page.keyboard.press("Enter");
        const dialog = page.getByRole("dialog", {
          name: "Delete project",
          exact: true,
        });
        await expect(dialog).toContainText("archived chats");
        await expect(dialog).toContainText("Running agents will be stopped");
        await expect(dialog).toContainText("Git worktrees are kept");
        await dialog
          .getByRole("button", { name: "Cancel", exact: true })
          .click();
        expect(await page.evaluate(() => window.posts)).toEqual([]);
        await options.click();
        await page.getByRole("menuitem", { name: "Delete project…" }).click();
        await page.evaluate(() => {
          window.fail = true;
        });
        // Two clicks in one task exercise the synchronous intention lock before React commits.
        await dialog
          .getByRole("button", { name: "Delete project", exact: true })
          .evaluate((button) => {
            button.click();
            button.click();
          });
        const unknown = page.getByRole("dialog", {
          name: "Delete result unconfirmed",
        });
        await expect(unknown).toBeVisible();
        expect(await page.evaluate(() => window.posts.length)).toBe(1);
        expect(await page.evaluate(() => window.suspended)).toContain("lead");
        expect(await page.evaluate(() => window.finished)).toEqual([]);
        await page.evaluate(() => window.retire());
        await expect(options).toHaveCount(0);
        await expect(unknown).toBeVisible();
        await page.evaluate(() => {
          window.fail = false;
        });
        await unknown
          .getByRole("button", { name: "Recover saved request" })
          .click();
        await expect(unknown).toHaveCount(0);
        const posts = await page.evaluate(() => window.posts);
        expect(posts).toHaveLength(2);
        expect(posts[1]).toEqual(posts[0]);
        if (variant)
          expect(await page.evaluate(() => window.owner)).toBe("owner");
        expect(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
          ),
        ).toBe(true);
      }
      expect(errors).toEqual([]);
    } finally {
      await page.close();
      await server.close();
    }
  });
}
