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
    window.preferencesRevision=1; window.readOnly=false; window.writes=[];
    window.preferences={
      show_remote_input:true,show_remote_monitor:true,remote_monitor_mute_audio:false,
      remote_monitor_disconnect_on_stream_end:false,remote_monitor_disconnect_on_client_disconnect:false,
      remote_monitor_terminate_on_first_request:false,remote_monitor_confirm_app_replacement:true,max_clients:16
    };
    window.api={allows:()=>true,post:async(route,payload)=>{
      if(route==='extensions/list')return {extensions:[{
        id:'remote-session',name:'Remote sessions',sdk_version:1,digest:'a'.repeat(64),
        entrypoint:'remote-session.js',placements:['settings'],
        methods:${JSON.stringify(configuration.methods)}.filter(method=>!readOnly||!method.scope.endsWith('.write'))
      }]};
      if(route!=='extensions/call')throw Error(route);
      if(payload.method==='preferences_update'){
        writes.push(payload.payload);
        if(window.deferSave)await new Promise(resolve=>window.releaseSave=resolve);
        Object.assign(preferences,payload.payload.settings); preferencesRevision++;
      }
      return {result:{ok:true,settings:{...preferences},revision:preferencesRevision}};
    }};
    window.pages=new Amaterasu.ExtensionPages(api);await pages.discover();
    await pages.mountSlot('remote-session',document.querySelector('#settings'),'settings');
  })()`);
  await until("document.querySelectorAll('.remote-session-settings input').length===8");
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
  assert.equal(await evaluate("[...document.querySelectorAll('.remote-session-settings input')].every(input=>input.disabled)"), true);
  await evaluate('releaseSave()');
  await until('!pages.isBusy()&&!pages.isDirty()');
  assert.equal(await evaluate('writes.length===1&&writes[0].expected_revision===1&&writes[0].settings.max_clients===8'), true);
  assert.equal(await evaluate("Object.keys(writes[0].settings).some(key=>['enabled','allowed_principals'].includes(key))"), false);
  assert.match(await evaluate("document.querySelector('.remote-session-notice').textContent"), /saved and confirmed/);

  await command('Emulation.setDeviceMetricsOverride', {
    width: 390, height: 844, deviceScaleFactor: 1, mobile: true,
  }, sessionId);
  assert.equal(await evaluate('document.documentElement.scrollWidth<=document.documentElement.clientWidth+1'), true, 'mobile settings do not overflow');
  shot = await command('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true }, sessionId);
  writeFileSync(join(output, 'remote-session-mobile.png'), Buffer.from(shot.data, 'base64'));

  await evaluate(`(async()=>{
    pages.dispose();readOnly=true;
    pages=new Amaterasu.ExtensionPages(api);await pages.discover();
    await pages.mountSlot('remote-session',document.querySelector('#settings'),'settings');
  })()`);
  await until("document.querySelectorAll('.remote-session-settings input').length===8");
  assert.equal(await evaluate("[...document.querySelectorAll('.remote-session-settings input')].every(input=>input.disabled)"), true);
  assert.equal(await evaluate("[...document.querySelectorAll('button')].find(button=>button.textContent==='Save remote session settings').disabled"), true);
  await evaluate('pages.dispose()');
  assert.equal(await evaluate("!document.querySelector('.remote-session-settings')&&!pages.isDirty()&&!pages.isBusy()"), true);
  console.log('PASS: signed settings discovery, shared controls, exact revision/readback, draft/busy guards, read-only aliases, mobile layout, and surface retirement');
} finally {
  socket?.close();
  const exited = new Promise(resolve => browser.once('exit', resolve));
  browser.kill('SIGTERM');
  if (browser.exitCode === null) await exited;
  rmSync(profile, { recursive: true, force: true });
  await new Promise(resolve => server.close(resolve));
}
