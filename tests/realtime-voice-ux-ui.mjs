// React fixture: no microphone, native account, or model request.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { resolve } from "node:path";
const root = resolve(import.meta.dirname, "../web");
const require = createRequire(root + "/package.json");
const { createServer } = await import(require.resolve("vite"));
const { chromium } = require("playwright-core");
const entry = `import React from 'react';import{createRoot}from'react-dom/client';import{MantineProvider}from'@mantine/core';import'@mantine/core/styles.css';import RealtimeVoice from '/src/components/RealtimeVoice.tsx';
window.calls=[];window.order=[];window.stops=0;window.records=[];window.pending=false;window.voiceError='';window.codexDesktop={requestMicrophone:async()=>{window.order.push('permission');if(window.holdPermission)await new Promise(r=>window.releasePermission=r);}};
window.RTCPeerConnection=class{constructor(){window.pc=this;}addTrack(){}createDataChannel(){return window.dc={close(){},send(){throw Error('Native Core owns messages');}};}async createOffer(){return {sdp:'v=0\\noffer'};}async setLocalDescription(){}async setRemoteDescription(sdp){window.answer=sdp;window.emit({type:"session.started"});}close(){}};
const track={enabled:true,stop(){window.stops++;}};
Object.defineProperty(navigator,'mediaDevices',{value:{getUserMedia:async()=>{window.order.push('capture');return {getTracks:()=>[track],getAudioTracks:()=>[track]};}}});
window.emit=e=>window.dc.onmessage({data:JSON.stringify(e)});
window.fetch=async(path,opts)=>{const a=path.split('/').at(-1),b=JSON.parse(opts?.body||'{}');window.calls.push([a,b]);let data={};if(a==='status'){window.order.push('status');data={configured:true,transport:'native'};}if(a==='records')data={records:window.records,cursor:window.records.length,session:window.sid?{session_id:window.sid,state:window.voiceError?'failed':window.pending?'connecting':'ready',sdp:window.pending?null:'answer',error:window.voiceError}:null};if(a==='start'){window.sid=b.session_id;if(window.holdStart)await new Promise(r=>window.releaseStart=r);data={session_id:window.sid,state:window.pending?'connecting':'ready',sdp:window.pending?null:'answer'};}if(a==='session')data={session_id:window.sid,state:window.voiceError?'failed':window.pending?'connecting':'ready',sdp:window.pending?null:'answer',error:window.voiceError};return new Response(JSON.stringify(data));};
const root=createRoot(document.getElementById('root'));window.render=id=>root.render(<MantineProvider defaultColorScheme='dark'><RealtimeVoice agentId={id} notify={()=>{}}/></MantineProvider>);window.render('chat-one');`;
const server = await createServer({ configFile: false, root, server: { host: "127.0.0.1", port: 0 }, plugins: [{ name: "voice-fixture", configureServer(s) { s.middlewares.use("/audit", (_, res) => { res.setHeader("Content-Type", "text/html"); res.end('<div id="root" style="padding:200px 24px"></div><script type="module" src="/audit-entry.tsx"></script>'); }); }, resolveId(id) { if (id === "/audit-entry.tsx") return root + id; }, load(id) { if (id === root + "/audit-entry.tsx") return entry; } }] });
await server.listen();
const browser = await chromium.launch({executablePath: process.env.CHROME_BIN || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
try {
 const page = await browser.newPage({viewport:{width:500,height:700}});
 const errors=[];page.on('pageerror',e=>errors.push(e.message));
 async function reset(){await page.goto(server.resolvedUrls.local[0]+'audit');await page.getByRole('button',{name:'Voice conversation',exact:true}).click();}
 async function start(){await page.getByRole('button',{name:'Start voice',exact:true}).click();await page.waitForFunction(()=>window.calls.some(c=>c[0]==='start'));}
 await reset();await start();await page.getByText('Listening',{exact:true}).waitFor();
 assert.deepEqual(await page.evaluate(()=>window.order),['status','permission','capture']);
 assert.equal(await page.getByRole('button',{name:'Send transcript',exact:true}).count(),0);
 await page.evaluate(()=>{window.emit({type:'delegation.created',item:{id:'task'}});window.records=[{id:'one',seq:1,kind:'user',text:'Native canonical transcript'}];});
 await page.getByText('Working',{exact:true}).waitFor();await page.getByText('Transcript',{exact:true}).click();await page.getByText('Native canonical transcript',{exact:true}).waitFor();
 assert.equal(await page.evaluate(()=>window.calls.filter(c=>['submit','record','speech','approve'].includes(c[0])).length),0);
 await page.getByRole('button',{name:'Mute',exact:true}).click();await page.getByRole('button',{name:'Unmute',exact:true}).waitFor();
 await page.screenshot({path:'/tmp/studio-native-voice-ui.png'});
 await page.getByRole('button',{name:'End voice',exact:true}).click();await page.waitForFunction(()=>window.stops===1&&window.calls.some(c=>c[0]==='end'));
 await reset();await page.evaluate(()=>window.pending=true);await start();assert.equal(await page.evaluate(()=>window.answer),undefined);
 await page.evaluate(()=>window.pending=false);await page.waitForFunction(()=>window.answer?.sdp==='answer');
 await page.evaluate(()=>window.voiceError='Voice connection failed');await page.getByRole('alert').getByText('Voice connection failed',{exact:true}).waitFor();assert.equal(await page.evaluate(()=>window.stops),1);
 await reset();await page.evaluate(()=>window.holdPermission=true);await page.getByRole('button',{name:'Start voice',exact:true}).click();await page.waitForFunction(()=>!!window.releasePermission);
 await page.getByRole('button',{name:'End voice',exact:true}).click();await page.evaluate(()=>window.releasePermission());assert.equal(await page.evaluate(()=>window.order.includes('capture')),false);
 await reset();await page.evaluate(()=>window.holdStart=true);await start();const sid=await page.evaluate(()=>window.sid);
 await page.getByRole('button',{name:'End voice',exact:true}).click();await page.evaluate(()=>window.releaseStart());await page.waitForFunction(()=>window.calls.some(c=>c[0]==='end'));
 assert.ok((await page.evaluate(()=>window.calls.filter(c=>c[0]==='end'))).every(c=>c[1].session_id===sid));
 await reset();await start();await page.evaluate(()=>window.render('chat-two'));await page.waitForFunction(()=>window.stops===1);assert.equal(await page.evaluate(()=>window.calls.filter(c=>c[0]==='end').at(-1)[1].agent),'chat-one');
 assert.deepEqual(errors,[]);
 console.log('PASS: native voice UI, canonical transcript, account switch, delayed SDP, connection error, stop during permission and start.');
} finally { await browser.close();await server.close(); }
