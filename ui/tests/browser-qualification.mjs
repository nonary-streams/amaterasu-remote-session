import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const sdkRoot = resolve(process.env.AMATERASU_SDK ?? join(root, '../../..'));
const output = resolve(process.env.AMATERASU_QUALIFICATION_DIR ?? join(root, 'build/browser-qualification'));
const configuration = JSON.parse(readFileSync(join(root, 'ui.json'), 'utf8'));
const plugin = readFileSync(join(root, 'dist/remote-session.js'));
const css = readFileSync(join(root, 'dist/remote-session.css'));
mkdirSync(output, { recursive: true });
const server = createServer((request, response) => {
  if (request.url?.endsWith('remote-session.js')) {
    response.setHeader('Content-Type', 'text/javascript'); response.end(plugin);
  } else if (request.url?.endsWith('remote-session.css')) {
    response.setHeader('Content-Type', 'text/css'); response.end(css);
  } else if (request.url === '/base.css') {
    response.setHeader('Content-Type', 'text/css');
    response.end(readFileSync(join(sdkRoot, 'frontend/dist/styles.css')));
  } else {
    response.end('<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/base.css"></head><body><main style="padding:24px;max-width:1100px;margin:auto"><div id="settings"></div></main></body></html>');
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const origin = `http://127.0.0.1:${server.address().port}`;
const profile = mkdtempSync(join(root, 'build/browser-profile-'));
const browser = spawn(process.env.AMATERASU_BROWSER ?? 'chromium', [
  '--headless=new', '--disable-gpu', '--no-first-run', '--disable-extensions',
  '--remote-debugging-port=0', `--user-data-dir=${profile}`, 'about:blank',
], { stdio: 'ignore' });
let socket;
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
try {
  let address;
  for (let attempt = 0; attempt < 200; attempt++) {
    try {
      const [port, path] = readFileSync(join(profile, 'DevToolsActivePort'), 'utf8').trim().split('\n');
      address = `ws://127.0.0.1:${port}${path}`;
      break;
    } catch { await delay(50); }
  }
  assert.ok(address, 'Chromium debugger did not start');
  socket = new WebSocket(address);
  await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
  let sequence = 0;
  const pending = new Map();
  socket.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    const callback = pending.get(message.id);
    if (!callback) return;
    pending.delete(message.id);
    clearTimeout(callback.timeout);
    if (message.error) callback.reject(new Error(message.error.message));
    else callback.resolve(message.result);
  };
  const command = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`Timed out: ${method}`)); }, 15_000);
    pending.set(id, { resolve, reject, timeout });
    socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
  });
  const { targetId } = await command('Target.createTarget', { url: origin });
  const { sessionId } = await command('Target.attachToTarget', { targetId, flatten: true });
  const evaluate = async expression => {
    const result = await command('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }, sessionId);
    if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? 'Browser evaluation failed');
    return result.result.value;
  };
  const until = async expression => {
    for (let attempt = 0; attempt < 200; attempt++) {
      if (await evaluate(expression)) return;
      await delay(25);
    }
    throw new Error(`Browser state did not settle: ${expression}`);
  };
  await command('Emulation.setDeviceMetricsOverride', {
    width: 1280, height: 1000, deviceScaleFactor: 1, mobile: false,
  }, sessionId);
  await until("!!document.querySelector('#settings')");
  const source = [
    'locale-catalogs.ts', 'locale.ts', 'ui.ts', 'extension_ui.ts', 'extension_ui_layout.ts',
    'extension_ui_tabs.ts', 'extension_ui_feedback.ts', 'extension_ui_dialog.ts',
    'extension_ui_charts.ts', 'extension_slots.ts', 'extensions.ts',
  ].map(file => readFileSync(join(sdkRoot, 'frontend/src', file), 'utf8')).join('\n');
  await evaluate(ts.transpile(source, { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None }));
  await evaluate(`(async()=>{
    window.preferencesRevision=1; window.readOnly=false; window.writes=[];window.layoutWrites=[];window.profileWrites=[];window.catalogRevision=1;window.profileSequence=1;
    window.preferences={
      show_remote_input:true,show_remote_monitor:true,remote_monitor_mute_audio:false,
      remote_monitor_disconnect_on_stream_end:false,remote_monitor_disconnect_on_client_disconnect:false,
      remote_monitor_terminate_on_first_request:false,remote_monitor_confirm_app_replacement:true,max_clients:16,remote_monitor_layout:{version:1,placements:{}}
    };
    window.profiles=[{id:'app-0000000000000001',uuid:'11111111-2222-4333-8444-555555555555',name:'Tablet monitor',width:1920,height:1080,refresh_millihz:60000,hdr_enabled:false,scale:100,target:{kind:'owned'}}];
    window.api={allows:()=>true,post:async(route,payload)=>{
      if(route==='extensions/list')return {extensions:[{
        id:'remote-session',name:'Remote sessions',sdk_version:1,digest:'a'.repeat(64),
        entrypoint:'remote-session.js',placements:['settings'],
        methods:${JSON.stringify(configuration.methods)}.filter(method=>!readOnly||!method.scope.endsWith('.write'))
      }]};
      if(route!=='extensions/call')throw Error(route);
      if(payload.method==='profile_targets')return {result:{ok:true,targets:[
        {selector:'display-main',label:'Desktop display',ready:true,mode:{width:1920,height:1080,refresh_millihz:60000,hdr_enabled:false}},
        {selector:'display-offline',label:'Disconnected display',ready:false,error:'Display is not active'}
      ]}};
      if(payload.method==='profiles'){
        const cursor=payload.payload?.cursor;
        if(cursor&&cursor.revision!==catalogRevision)return {result:{ok:false,code:'revision_conflict'}};
        const remaining=profiles.filter(profile=>!cursor||profile.id>cursor.after).sort((left,right)=>left.id.localeCompare(right.id));
        const rows=remaining.slice(0,1);
        return {result:{ok:true,revision:catalogRevision,profiles:rows.map(profile=>({...profile})),
          next_cursor:remaining.length>rows.length?{revision:catalogRevision,after:rows.at(-1).id}:null}};
      }
      if(payload.method==='profile_upsert'){
        const request=payload.payload;profileWrites.push(request);
        if(request.expected_revision!==catalogRevision)return {result:{ok:false,code:'revision_conflict'}};
        const profile={...request.profile};
        if(!profile.id){profileSequence++;profile.id='app-'+profileSequence.toString(16).padStart(16,'0');profile.uuid=profileSequence.toString().repeat(8)+'-2222-4333-8444-555555555555';}
        profiles=profiles.filter(existing=>existing.id!==profile.id);profiles.push(profile);catalogRevision++;
        return {result:{ok:true,revision:catalogRevision,profile}};
      }
      if(payload.method==='profile_delete'){
        if(payload.payload.expected_revision!==catalogRevision)return {result:{ok:false,code:'revision_conflict'}};
        profiles=profiles.filter(profile=>profile.id!==payload.payload.id);catalogRevision++;
        return {result:{ok:true,revision:catalogRevision}};
      }
      if(payload.method==='layout_update'){
        layoutWrites.push(payload.payload);
        if(payload.payload.expected_revision!==preferencesRevision)return {result:{ok:false,code:'revision_conflict'}};
        preferences.remote_monitor_layout=payload.payload.layout;preferencesRevision++;
      }
      if(payload.method==='layout'||payload.method==='layout_update')return {result:{ok:true,revision:preferencesRevision,
        layout:preferences.remote_monitor_layout,
        clients:[{id:'client-a',label:'Drawing tablet',ready:true,state:'ready'},{id:'client-b',label:'Laptop',ready:false,state:'retained'}],
        physical_anchors:[{id:'display-main',label:'Desktop display'}],application_status:{state:'pending'}
      }};
      if(payload.method==='preferences_update'){
        writes.push(payload.payload);
        if(payload.payload.expected_revision!==preferencesRevision)return {result:{ok:false,code:'revision_conflict'}};
        if(window.deferSave)await new Promise(resolve=>window.releaseSave=resolve);
        Object.assign(preferences,payload.payload.settings); preferencesRevision++;
      }
      return {result:{ok:true,settings:{...preferences},revision:preferencesRevision}};
    }};
    window.pages=new Amaterasu.ExtensionPages(api);await pages.discover();
    await pages.mountSlot('remote-session',document.querySelector('#settings'),'settings');
  })()`);
  await until("document.querySelectorAll('.remote-session-settings input').length===16&&document.querySelector('.remote-session-profile-name input')?.value==='Tablet monitor'");
  assert.equal(await evaluate("document.querySelector('input[type=number]').value"), '16');
  assert.equal(await evaluate('pages.isDirty()'), false);
  assert.equal(await evaluate('document.documentElement.scrollWidth<=document.documentElement.clientWidth+1'), true);
  let shot = await command('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true }, sessionId);
  writeFileSync(join(output, 'remote-session-desktop.png'), Buffer.from(shot.data, 'base64'));

  await evaluate("const field=document.querySelector('input[type=number]');field.value='8';field.dispatchEvent(new Event('input',{bubbles:true}));");
  assert.equal(await evaluate('pages.isDirty()'), true, 'settings drafts reach the shared navigation guard');
  await evaluate("window.deferSave=true;[...document.querySelectorAll('button')].find(button=>button.textContent==='Save remote session settings').click()");
  await until('!!window.releaseSave');
  assert.equal(await evaluate('pages.isBusy()'), true);
  assert.equal(await evaluate("[...document.querySelectorAll('.remote-session-settings > .plugin-ui-card:not(.remote-session-layout):not(.remote-session-profiles) input')].every(input=>input.disabled)"), true);
  await evaluate('releaseSave()');
  await until('!pages.isBusy()&&!pages.isDirty()');
  assert.equal(await evaluate('writes.length===1&&writes[0].expected_revision===1&&writes[0].settings.max_clients===8'), true);
  assert.equal(await evaluate("Object.keys(writes[0].settings).some(key=>['enabled','allowed_principals'].includes(key))"), false);
  assert.match(await evaluate("document.querySelector('.remote-session-notice').textContent"), /saved and confirmed/);

  await evaluate(`window.choose=(label,value)=>{
    const node=[...document.querySelectorAll('label')].find(node=>node.textContent===label);
    const field=document.getElementById(node.htmlFor);field.value=value;
    field.dispatchEvent(new Event(field.tagName==='SELECT'?'change':'input',{bubbles:true}));
  };window.press=label=>[...document.querySelectorAll('button')].find(button=>button.textContent===label).click();`);
  await evaluate("choose('Position','relative');choose('Place monitor','below');choose('Gap between monitors (pixels)','24');press('Save monitor positions')");
  await until("document.querySelector('.remote-session-layout .remote-session-notice').textContent.includes('another editor')");
  assert.equal(await evaluate('pages.isDirty()'), true, 'shared preferences revision conflicts keep position edits');
  await evaluate("press('Refresh saved positions')");
  await until("!pages.isBusy()&&document.querySelector('.remote-session-layout .remote-session-notice').textContent.includes('unsaved changes are kept')");
  await evaluate("press('Save monitor positions')");
  await until("!pages.isDirty()&&!pages.isBusy()&&document.querySelector('.remote-session-layout .remote-session-notice').textContent.includes('saved and confirmed')");
  assert.equal(await evaluate("layoutWrites.at(-1).expected_revision===2&&layoutWrites.at(-1).layout.placements['client-a'].gap_px===24&&layoutWrites.at(-1).layout.placements['client-a'].edge==='below'"), true);
  assert.match(await evaluate("document.querySelector('.remote-session-layout').textContent"), /Placement status: pending/);
  await evaluate("document.querySelector('.remote-session-layout').scrollIntoView({block:'start'})");
  shot=await command('Page.captureScreenshot',{format:'png',captureBeyondViewport:false},sessionId);
  writeFileSync(join(output,'remote-session-layout-desktop.png'),Buffer.from(shot.data,'base64'));

  await evaluate("choose('Profile name','Drawing tablet HDR');choose('Display target','existing:display-main');choose('Stream width (pixels)','2560');choose('Stream height (pixels)','1600');choose('Stream refresh rate (Hz)','120');press('Save monitor profile')");
  await until("!pages.isDirty()&&!pages.isBusy()&&document.querySelector('.remote-session-profiles .remote-session-notice').textContent.includes('saved and confirmed')");
  assert.equal(await evaluate("profileWrites[0].profile.target.selector==='display-main'&&profileWrites[0].profile.refresh_millihz===120000&&profileWrites[0].expected_revision===1"),true);
  assert.equal(await evaluate("[...document.querySelectorAll('.remote-session-profiles select option')].some(option=>option.value==='existing:display-offline')"),false,'unavailable displays cannot be chosen as new targets');
  assert.match(await evaluate("document.querySelector('.remote-session-profiles').textContent"), /Display currently 1920 × 1080 at 60 Hz, SDR/);
  assert.match(await evaluate("document.querySelector('.remote-session-profiles').textContent"), /HDR setting unchanged/);
  await evaluate("press('New monitor profile');choose('Profile name','Presentation display');press('Save monitor profile')");
  await until("profiles.length===2&&!pages.isBusy()&&!pages.isDirty()");
  await evaluate("press('Delete monitor profile')");
  assert.equal(await evaluate("document.querySelector('dialog')?.open"),true);
  await evaluate("press('Delete this profile')");
  await until("profiles.length===1&&!pages.isBusy()&&!pages.isDirty()");
  assert.match(await evaluate("document.querySelector('.remote-session-profiles .remote-session-notice').textContent"), /Presentation display/);
  assert.equal(await evaluate("document.querySelector('.remote-session-profile-name input').value"), 'Drawing tablet HDR');

  await command('Emulation.setDeviceMetricsOverride', {
    width: 390, height: 844, deviceScaleFactor: 1, mobile: true,
  }, sessionId);
  assert.equal(await evaluate('document.documentElement.scrollWidth<=document.documentElement.clientWidth+1'), true, 'mobile settings do not overflow');
  await evaluate("window.scrollTo(0,0)");
  shot = await command('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true }, sessionId);
  writeFileSync(join(output, 'remote-session-mobile.png'), Buffer.from(shot.data, 'base64'));
  await evaluate("document.querySelector('.remote-session-profiles').scrollIntoView({block:'start'})");
  shot = await command('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false }, sessionId);
  writeFileSync(join(output, 'remote-session-profiles-mobile.png'), Buffer.from(shot.data, 'base64'));

  await evaluate(`for(let index=3;index<=76;index++)profiles.push({
    ...profiles[0],id:'app-'+index.toString(16).padStart(16,'0'),
    uuid:index.toString(16).padStart(8,'0')+'-2222-4333-8444-555555555555',name:'Screen '+index.toString().padStart(4,'0')
  });press('Refresh saved profiles');`);
  await until("!pages.isBusy()&&document.querySelector('.remote-session-profiles').textContent.includes('75 saved in total')");
  assert.equal(await evaluate("[...document.querySelectorAll('label')].find(label=>label.textContent==='Saved monitor profile')&&[...document.querySelectorAll('.remote-session-profiles select')][0].options.length<=33"),true,'local picker respects the shared SDK option bound');
  await evaluate("press('Next profiles');choose('Saved monitor profile','profile:app-0000000000000040')");
  assert.equal(await evaluate("document.querySelector('.remote-session-profile-name input').value"),'Screen 0064');
  await evaluate("choose('Find a monitor profile','Screen 0070');choose('Saved monitor profile','profile:app-0000000000000046')");
  assert.equal(await evaluate("document.querySelector('.remote-session-profile-name input').value"),'Screen 0070');
  assert.equal(await evaluate('pages.isDirty()'),false);

  await evaluate(`(async()=>{
    pages.dispose();readOnly=true;
    pages=new Amaterasu.ExtensionPages(api);await pages.discover();
    await pages.mountSlot('remote-session',document.querySelector('#settings'),'settings');
  })()`);
  await until("document.querySelectorAll('.remote-session-settings input').length===16");
  assert.equal(await evaluate("[...document.querySelectorAll('.remote-session-settings .plugin-control:not(.remote-session-profile-search) input')].every(input=>input.disabled)"), true);
  assert.equal(await evaluate("[...document.querySelectorAll('button')].find(button=>button.textContent==='Save remote session settings').disabled"), true);
  await evaluate('pages.dispose()');
  assert.equal(await evaluate("!document.querySelector('.remote-session-settings')&&!pages.isDirty()&&!pages.isBusy()"), true);
  console.log('PASS: signed SDK settings, persisted layout with shared-revision conflict, profile create/update/delete and exact target selection, readback, guards, read-only aliases, mobile layout, and retirement');
} finally {
  socket?.close();
  const exited = new Promise(resolve => browser.once('exit', resolve));
  browser.kill('SIGTERM');
  if (browser.exitCode === null) await exited;
  rmSync(profile, { recursive: true, force: true });
  await new Promise(resolve => server.close(resolve));
}
