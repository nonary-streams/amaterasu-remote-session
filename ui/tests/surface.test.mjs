import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';

class Element {
  constructor(tagName) {
    this.tagName = tagName;
    this.children = [];
    this.attributes = new Map();
    this.dataset = {};
    this.classes = new Set();
    this.classList = { add: value => this.classes.add(value) };
    this.parent = null;
    this.content = '';
  }
  append(...children) {
    for (const child of children) {
      child.parent = this;
      this.children.push(child);
    }
  }
  remove() {
    if (this.parent) this.parent.children = this.parent.children.filter(child => child !== this);
    this.parent = null;
  }
  setAttribute(name, value) { this.attributes.set(name, value); }
  set textContent(value) { this.content = value; }
  get textContent() { return this.content + this.children.map(child => child.textContent).join(''); }
}
const settings = () => ({
  show_remote_input: true, show_remote_monitor: true,
  remote_monitor_mute_audio: false,
  remote_monitor_disconnect_on_stream_end: false,
  remote_monitor_disconnect_on_client_disconnect: false,
  remote_monitor_terminate_on_first_request: false,
  remote_monitor_confirm_app_replacement: true,
  max_clients: 16,
});
const reply = (revision = 4, values = settings()) => ({ ok: true, revision, settings: values });
const settle = () => new Promise(setImmediate);

function harness({ canWrite = true, call } = {}) {
  const registry = new Map();
  const context = vm.createContext({
    Error, AbortController,
    document: { createElement: tag => new Element(tag) },
    window: { AmaterasuExtensions: { v1: { register: (id, extension) => registry.set(id, extension) } } },
  });
  vm.runInContext(ts.transpile(['src/settings.ts', 'src/main.ts']
    .map(file => readFileSync(file, 'utf8')).join('\n'), {
    target: ts.ScriptTarget.ES2022,
  }), context);
  const fields = new Map(), actions = new Map(), all = [];
  const field = options => {
    let value = options.value;
    const control = {
      element: new Element('label'), state: { disabled: options.disabled ?? false },
      value: () => value, setValue: next => { value = next; },
      setState(patch) { Object.assign(this.state, patch); },
      change(next) { if (!this.state.disabled) { value = next; options.onChange?.(next); } },
      dispose() { this.disposed = true; this.element.remove(); },
    };
    control.element.textContent = options.label;
    fields.set(options.label, control);
    all.push(control);
    return control;
  };
  const guards = { dirty: [], busy: [] };
  const controller = new AbortController();
  const calls = [];
  let stored = reply();
  const sdk = {
    signal: controller.signal,
    canCall: alias => alias === 'preferences' || canWrite,
    assets: { async stylesheet(path) { assert.equal(path, 'remote-session.css'); } },
    setDirty: value => guards.dirty.push(value),
    setBusy: value => guards.busy.push(value),
    async call(alias, payload) {
      calls.push([alias, payload]);
      if (call) return call(alias, payload);
      if (alias === 'preferences_update') stored = reply(stored.revision + 1, payload.settings);
      return stored;
    },
    ui: {
      version: 1, toggle: field, numberField: field,
      card(options) {
        const element = new Element('section'), content = new Element('div');
        element.textContent = options.label;
        element.append(content);
        const card = { element, content, append: child => content.append(child),
          dispose() { this.disposed = true; element.remove(); } };
        all.push(card);
        return card;
      },
      button(options) {
        const control = { element: new Element('button'), state: { disabled: options.disabled ?? false },
          busy: () => Boolean(control.state.busy),
          setState(patch) { Object.assign(this.state, patch); },
          click() { return this.state.disabled ? undefined : options.onClick({}); },
          dispose() { this.disposed = true; this.element.remove(); } };
        control.element.textContent = options.label;
        actions.set(options.label, control);
        all.push(control);
        return control;
      },
    },
  };
  const container = new Element('div');
  const extension = registry.get('remote-session');
  assert.equal(extension.title, 'Remote sessions');
  assert.equal(extension.mount, undefined);
  const dispose = extension.mountSettings(container, sdk);
  return { container, fields, actions, guards, controller, calls, all, dispose };
}

test('signed settings placement declares only read and administrator update methods', () => {
  const manifest = JSON.parse(readFileSync('ui.json', 'utf8'));
  assert.deepEqual(manifest.placements, ['settings']);
  assert.deepEqual(manifest.methods.map(method => [method.alias, method.service, method.method, method.scope, method.min_role]), [
    ['preferences', 'nonary.remote-session/1', 'settings_get', 'configuration.read', 'viewer'],
    ['preferences_update', 'nonary.remote-session/1', 'settings_update', 'configuration.write', 'administrator'],
  ]);
});

test('mounted shared controls set draft and save guards, then confirm readback', async () => {
  let release;
  let stored = reply();
  const pending = new Promise(resolve => { release = resolve; });
  const view = harness({ async call(alias, payload) {
    if (alias === 'preferences_update') {
      stored = reply(5, payload.settings);
      await pending;
    }
    return stored;
  } });
  await settle();
  assert.equal(view.fields.size, 8);
  assert.equal(view.guards.dirty.at(-1), false);
  view.fields.get('Mute Remote Monitor audio').change(true);
  assert.equal(view.guards.dirty.at(-1), true);
  const saving = view.actions.get('Save remote session settings').click();
  assert.equal(view.guards.busy.at(-1), true);
  for (const field of view.fields.values()) assert.equal(field.state.disabled, true);
  assert.equal(view.actions.get('Refresh saved settings').state.disabled, true);
  assert.equal(view.actions.get('Discard changes').state.disabled, true);
  release();
  await saving;
  assert.equal(view.guards.busy.at(-1), false);
  assert.equal(view.guards.dirty.at(-1), false);
  assert.match(view.container.textContent, /Settings saved and confirmed/);
  assert.equal(view.calls.length, 3);
  view.dispose();
  assert.equal(view.container.children.length, 0);
  assert.ok(view.all.every(control => control.disposed));
});

test('viewers can read all fields without issuing an update', async () => {
  const view = harness({ canWrite: false });
  await settle();
  assert.match(view.container.textContent, /read-only for your account/);
  for (const field of view.fields.values()) assert.equal(field.state.disabled, true);
  const save = view.actions.get('Save remote session settings');
  assert.equal(save.state.disabled, true);
  await save.click();
  assert.deepEqual(view.calls.map(([alias]) => alias), ['preferences']);
  view.dispose();
});

test('failed initial reads can be retried using the visible refresh control', async () => {
  let reads = 0;
  const view = harness({ async call() {
    if (++reads === 1) throw new Error('settings unavailable');
    return reply();
  } });
  await settle();
  assert.match(view.container.textContent, /settings unavailable/);
  assert.equal(view.fields.size, 0);
  const refresh = view.actions.get('Refresh saved settings');
  assert.equal(refresh.state.disabled, false);
  await refresh.click();
  assert.equal(view.fields.size, 8);
  assert.match(view.container.textContent, /up to date/);
  view.dispose();
});

test('aborting a surface retires controls and ignores late settings responses', async () => {
  let release;
  const response = new Promise(resolve => { release = resolve; });
  const view = harness({ call: () => response });
  await settle();
  view.controller.abort();
  const states = view.guards.dirty.length;
  release(reply());
  await settle();
  assert.equal(view.container.children.length, 0);
  assert.equal(view.fields.size, 0);
  assert.equal(view.guards.dirty.length, states);
  assert.ok(view.all.every(control => control.disposed));
  view.dispose();
});
