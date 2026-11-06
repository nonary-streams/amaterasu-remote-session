import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';

const context = vm.createContext({ Error, TextEncoder });
vm.runInContext(ts.transpile(['src/layout_model.ts','src/settings.ts'].map(file=>readFileSync(file,'utf8')).join('\n'), {
  target: ts.ScriptTarget.ES2022,
}), context);
const { SettingsEditor, parseReply, parseSettings } = context.RemoteSessionUi;
const ordinary = value => JSON.parse(JSON.stringify(value));
const settings = () => ({
  show_remote_input: true, show_remote_monitor: true,
  remote_monitor_mute_audio: false,
  remote_monitor_disconnect_on_stream_end: false,
  remote_monitor_disconnect_on_client_disconnect: false,
  remote_monitor_terminate_on_first_request: false,
  remote_monitor_confirm_app_replacement: true,
  max_clients: 16, remote_monitor_layout: { version: 1, placements: {} },
});
const reply = (revision = 4, values = settings()) => ({ ok: true, revision, settings: values });
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};

test('settings validate limits and switches, and never carry trusted bootstrap fields', () => {
  assert.deepEqual(ordinary(parseSettings({ ...settings(), enabled: false,
    allowed_principals: ['attacker'], future: true })), settings());
  for (const value of [0, 33, NaN, Infinity, 1.5, '16', null]) {
    assert.throws(() => parseSettings({ ...settings(), max_clients: value }), /whole number/);
  }
  for (const key of context.RemoteSessionUi.booleanSettings) {
    assert.throws(() => parseSettings({ ...settings(), [key]: 'false' }), /switch/);
  }
  for (const value of [undefined, [], null]) assert.throws(() => parseSettings(value));
  for (const revision of [-1, 1.5, Number.MAX_SAFE_INTEGER + 1, '4', null]) {
    assert.throws(() => parseReply({ ...reply(), revision }), /revision/);
  }
});

test('save confirms the exact update and its readback before clearing the draft', async () => {
  let stored = reply();
  const calls = [];
  const editor = new SettingsEditor({ async call(alias, payload) {
    calls.push([alias, ordinary(payload ?? {})]);
    if (alias === 'preferences_update') {
      assert.equal(payload.expected_revision, stored.revision);
      stored = reply(stored.revision + 1, ordinary(payload.settings));
    }
    return stored;
  } }, () => {});
  await editor.refresh();
  editor.edit('remote_monitor_mute_audio', true);
  editor.edit('max_clients', 8);
  assert.equal(editor.dirty(), true);
  await editor.save();
  assert.equal(editor.error, false);
  assert.equal(editor.dirty(), false);
  assert.equal(editor.saved.revision, 5);
  assert.deepEqual(calls, [
    ['preferences', {}],
    ['preferences_update', { settings: { ...settings(), remote_monitor_mute_audio: true, max_clients: 8 }, expected_revision: 4 }],
    ['preferences', {}],
  ]);
  assert.match(editor.message, /saved and confirmed/);
});

test('a conflict preserves changes and refresh rebases their expected revision', async () => {
  let stored = reply();
  let updates = 0;
  const editor = new SettingsEditor({ async call(alias, payload) {
    if (alias === 'preferences') return stored;
    updates += 1;
    if (payload.expected_revision !== stored.revision) {
      return { ok: false, code: 'revision_conflict', revision: stored.revision };
    }
    stored = reply(stored.revision + 1, ordinary(payload.settings));
    return stored;
  } }, () => {});
  await editor.refresh();
  editor.edit('show_remote_input', false);
  stored = reply(5, { ...settings(), max_clients: 20 });
  await editor.save();
  assert.equal(updates, 1);
  assert.equal(editor.dirty(), true);
  assert.equal(editor.confirmationRequired, false);
  assert.equal(editor.draft.show_remote_input, false);
  assert.match(editor.message, /another editor/);
  await editor.refresh();
  assert.equal(editor.saved.revision, 5);
  assert.equal(editor.draft.show_remote_input, false);
  assert.equal(editor.draft.max_clients, 20);
  assert.match(editor.message, /unsaved changes are kept/);
  await editor.save();
  assert.equal(editor.saved.revision, 6);
  assert.equal(editor.dirty(), false);
});

test('an uncertain write cannot be repeated before a read-only outcome check', async () => {
  let stored = reply();
  let updates = 0;
  const editor = new SettingsEditor({ async call(alias, payload) {
    if (alias === 'preferences') return stored;
    updates += 1;
    stored = reply(stored.revision + 1, ordinary(payload.settings));
    throw new Error('connection lost after admission');
  } }, () => {});
  await editor.refresh();
  editor.edit('show_remote_monitor', false);
  await editor.save();
  assert.equal(editor.confirmationRequired, true);
  assert.equal(editor.dirty(), true);
  await editor.save();
  assert.equal(updates, 1);
  await editor.refresh();
  assert.equal(editor.confirmationRequired, false);
  assert.equal(editor.saved.revision, 5);
  assert.equal(editor.dirty(), false);
  assert.equal(updates, 1);
});

test('stale or mismatched readback preserves the draft and never reports saved', async () => {
  for (const readback of [reply(), reply(6), reply(5, { ...settings(), max_clients: 3 })]) {
    let reads = 0;
    const editor = new SettingsEditor({ async call(alias, payload) {
      if (alias === 'preferences') return ++reads === 1 ? reply() : readback;
      return reply(5, ordinary(payload.settings));
    } }, () => {});
    await editor.refresh();
    editor.edit('max_clients', 8);
    await editor.save();
    assert.equal(editor.error, true);
    assert.equal(editor.confirmationRequired, true);
    assert.equal(editor.dirty(), true);
    assert.doesNotMatch(editor.message, /saved and confirmed/);
    assert.equal(editor.draft.max_clients, 8);
  }
});

test('invalid drafts are kept locally and cannot issue a write', async () => {
  const calls = [];
  const editor = new SettingsEditor({ async call(alias) { calls.push(alias); return reply(); } }, () => {});
  await editor.refresh();
  editor.edit('max_clients', NaN);
  await editor.save();
  assert.deepEqual(calls, ['preferences']);
  assert.equal(editor.valid(), false);
  assert.equal(editor.dirty(), true);
  assert.match(editor.message, /whole number/);
});

test('busy save freezes edits and duplicate submissions until confirmed', async () => {
  const pending = deferred();
  let stored = reply();
  let updates = 0;
  const editor = new SettingsEditor({ async call(alias, payload) {
    if (alias === 'preferences') return stored;
    updates += 1;
    stored = reply(5, ordinary(payload.settings));
    await pending.promise;
    return stored;
  } }, () => {});
  await editor.refresh();
  editor.edit('max_clients', 8);
  const saving = editor.save();
  assert.equal(editor.busy, true);
  editor.edit('max_clients', 10);
  await editor.save();
  await editor.refresh(true);
  assert.equal(editor.draft.max_clients, 8);
  assert.equal(updates, 1);
  pending.resolve();
  await saving;
  assert.equal(editor.saved.settings.max_clients, 8);
  assert.equal(editor.busy, false);
});

test('discard reads the current settings and disposal rejects all late updates', async () => {
  let stored = reply();
  const editor = new SettingsEditor({ async call() { return stored; } }, () => {});
  await editor.refresh();
  editor.edit('max_clients', 8);
  stored = reply(5, { ...settings(), max_clients: 24 });
  await editor.refresh(true);
  assert.equal(editor.draft.max_clients, 24);
  assert.equal(editor.dirty(), false);

  const pending = deferred();
  let changes = 0;
  const closed = new SettingsEditor({ call() { return pending.promise; } }, () => changes++);
  const loading = closed.refresh();
  closed.dispose();
  const before = changes;
  pending.resolve(reply());
  await loading;
  assert.equal(closed.saved, null);
  assert.equal(changes, before);
});

test('revision exhaustion cannot submit an imprecise expected revision', async () => {
  let writes = 0;
  const editor = new SettingsEditor({ async call(alias) {
    if (alias === 'preferences_update') writes++;
    return reply(Number.MAX_SAFE_INTEGER);
  } }, () => {});
  await editor.refresh();
  editor.edit('max_clients', 8);
  await editor.save();
  assert.equal(writes, 0);
  assert.match(editor.message, /next settings revision/);
});

test('preferences preserve the separately edited monitor layout and rebase its current revision', async () => {
  let current = reply(4, { ...settings(), remote_monitor_layout: { version: 1, placements: {
    'client-a': { anchor_kind: 'physical', anchor_id: 'display-main', edge: 'right', alignment: 'start', gap_px: 12, primary: false, preserve: false, automatic_fallback: false },
  } } });
  let sent;
  const editor = new SettingsEditor({ async call(alias, payload) {
    if (alias === 'preferences') return current;
    sent = ordinary(payload);
    current = reply(current.revision + 1, ordinary(payload.settings)); return current;
  } }, () => {});
  await editor.refresh(); editor.edit('show_remote_input', false);
  current = { ...current, revision: 5, settings: { ...current.settings, remote_monitor_layout: {
    version: 1, placements: { 'client-a': { ...current.settings.remote_monitor_layout.placements['client-a'], gap_px: 24 } },
  } } };
  await editor.refresh(); await editor.save();
  assert.equal(sent.expected_revision, 5);
  assert.equal(sent.settings.remote_monitor_layout.placements['client-a'].gap_px, 24);
  assert.equal(sent.settings.show_remote_input, false);
});
