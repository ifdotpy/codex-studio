import { createRequire } from 'node:module';
import { mkdirSync, writeFileSync } from 'node:fs';
const require = createRequire('/Users/igor/.cache/responsive-port-local-backup/duo-proportions/worktree/workspaces/client/apps/web/package.json');
const { chromium } = require('playwright');
const out = process.argv[2]; mkdirSync(out, { recursive: true });
const browser = await chromium.launch({ headless: true });
for (const profile of [{name:'open',width:951,height:669,insets:{top:0,right:82,bottom:34,left:0},hinge:'open'}, {name:'closed',width:653,height:951,insets:{top:82,right:0,bottom:34,left:0},hinge:'closed'}, {name:'portrait',width:669,height:951,insets:{top:82,right:0,bottom:34,left:0},hinge:'open'}]) {
 const context = await browser.newContext({viewport:{width:profile.width,height:profile.height},hasTouch:true,colorScheme:'dark',reducedMotion:'reduce',serviceWorkers:'block'});
 const page=await context.newPage();
 await page.addInitScript((p)=>{
  window.__studioNativeViewport={version:1,state:{insets:p.insets,keyboard:{overlap:0,duration:0,curve:7}}};
  const apply=()=>{ const root=document.documentElement; if(!root)return; for(const [key,value] of Object.entries(p.insets))root.style.setProperty('--native-safe-'+key,value+'px'); root.dataset.nativeHinge=p.hinge; root.style.setProperty('--native-status-column-bottom',(p.insets.top?82:124)+'px'); root.style.setProperty('--native-status-column-left',(p.width-(p.insets.top?124:84))+'px'); };
  apply(); document.addEventListener('DOMContentLoaded',apply,{once:true});
 },profile);
 await page.goto('http://127.0.0.1:8794/');
 await page.locator('#composer').waitFor({timeout:60000});
 await page.evaluate(()=>document.fonts.ready);
 await page.screenshot({path:out+'/'+profile.name+'.png'});
 const collect=()=>page.evaluate(()=>{
  const rect=(selector)=>Array.from(document.querySelectorAll(selector)).filter(n=>n.getClientRects().length).map(n=>{const r=n.getBoundingClientRect();const s=getComputedStyle(n);return {label:n.getAttribute('aria-label')||n.className,x:r.x,y:r.y,width:r.width,height:r.height,padding:s.padding,background:s.backgroundColor,blur:s.backdropFilter};});
  return {attrs:Array.from(document.documentElement.attributes).filter(a=>a.name.startsWith('data-')).map(a=>[a.name,a.value]),sidebar:rect('#sidebar'),header:rect('.sidebar-header,.sidebar-header > *'),title:rect('.duo-transcript-bar,.transcript-heading'),composer:rect('#composer'),message:rect('#messages .message-content,#messages .message.assistant,#messages .prose'),passive:rect('.usage-footer,.usage-passive-band'),rail:rect('.edge-rail,.edge-rail button')};
 });
 const data=await collect();
 if(!(await page.locator('#sidebar').isVisible())) {const toggle=page.locator('#sidebar-toggle');if(await toggle.isVisible()){await toggle.click();await page.locator('#sidebar').waitFor();await page.screenshot({path:out+'/'+profile.name+'-sidebar.png'});data.drawer=await collect();}}
 writeFileSync(out+'/'+profile.name+'.json',JSON.stringify(data,null,2));
 console.log(profile.name,JSON.stringify({sidebar:data.sidebar,title:data.title,composer:data.composer,rail:data.rail}));
 await context.close();
}
await browser.close();
