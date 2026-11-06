import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';
const context = vm.createContext({ Error, TextEncoder });
vm.runInContext(ts.transpile(['src/layout_model.ts', 'src/settings.ts', 'src/profile_model.ts']
  .map(file => readFileSync(file, 'utf8')).join('\n'), { target: ts.ScriptTarget.ES2022 }), context);
const { LayoutEditor, ProfileEditor, parseLayout, parseProfile, parseProfilesReply, automaticPlacement } = context.RemoteSessionUi;
const ordinary = value => JSON.parse(JSON.stringify(value));
const placement = (patch = {}) => ({ anchor_kind: 'physical', anchor_id: 'display-main', edge: 'right',
  alignment: 'start', gap_px: 0, primary: false, preserve: false, automatic_fallback: false, ...patch });
const layoutReply = (revision = 1, placements = {}) => ({ ok: true, revision, layout: { version: 1, placements },
  clients: [{ id: 'client-a', label: 'Tablet', ready: true, state: 'ready' }, { id: 'client-b', label: 'Laptop', ready: false, state: 'retained' }],
  physical_anchors: [{ id: 'display-main', label: 'Desktop' }], application_status: { state: 'pending' } });
const profile = (patch = {}) => ({ id: 'app-0000000000000001', uuid: '11111111-2222-4333-8444-555555555555', name: 'Tablet',
  width: 1920, height: 1080, refresh_millihz: 60_000, hdr_enabled: false, scale: 100, target: { kind: 'owned' }, ...patch });
const profilesReply = (revision = 1, profiles = [profile()]) => ({ ok: true, revision, profiles, next_cursor: null });

test('layout validation rejects loops, duplicate primary, unsafe inventory choices and excessive payloads', () => {
  assert.throws(() => parseLayout({ version: 1, placements: { a: placement({ anchor_kind: 'client', anchor_id: 'b' }), b: placement({ anchor_kind: 'client', anchor_id: 'a' }) } }), /loop/);
  assert.throws(() => parseLayout({ version: 1, placements: { a: placement({ primary: true }), b: placement({ primary: true }) } }), /one primary/);
  assert.throws(() => parseLayout({ version: 1, placements: { a: placement({ gap_px: -1 }) } }), /gap/);
  assert.throws(() => parseLayout({ version: 1, placements: Object.fromEntries(Array.from({ length: 33 }, (_, i) => [`client-${i}`, placement()])) }), /32/);
  assert.throws(() => parseLayout({ version: 1, placements: Object.fromEntries(Array.from({ length: 4 }, (_, i) => [`client-${i}`, placement({ anchor_id: 'x'.repeat(4096) })])) }), /too large/);
  const parsed = parseLayout(JSON.parse('{"version":1,"placements":{"__proto__":{"anchor_kind":"physical","anchor_id":"display-main","edge":"right","alignment":"start","gap_px":0}}}'));
  assert.equal(Object.hasOwn(parsed.placements, '__proto__'), true);
  assert.equal({}.anchor_id, undefined);
});

test('layout writes persist exact public schema, read back, and keep native application status separate', async () => {
  let stored = layoutReply();
  const calls = [];
  const editor = new LayoutEditor({ async call(alias, payload) {
    calls.push([alias, ordinary(payload ?? {})]);
    if (alias === 'layout_update') {
      assert.equal(payload.expected_revision, stored.revision);
      stored = { ...stored, revision: stored.revision + 1, layout: ordinary(payload.layout) };
    }
    return stored;
  } }, () => {});
  await editor.refresh();
  editor.edit('client-a', placement({ gap_px: 24, edge: 'below', primary: true }));
  assert.equal(editor.validationError(), null);
  await editor.save();
  assert.equal(editor.dirty(), false);
  assert.equal(editor.saved.application_status.state, 'pending');
  assert.match(editor.message, /saved and confirmed/);
  assert.doesNotMatch(editor.message, /applied/);
  assert.deepEqual(calls.map(([alias]) => alias), ['layout', 'layout_update', 'layout']);
  assert.equal(calls[1][1].layout.placements['client-a'].gap_px, 24);
});

test('a layout conflict rebases only changed client rules and preserves unrelated saved rules', async () => {
  let stored = layoutReply();
  const editor = new LayoutEditor({ async call(alias, payload) {
    if (alias === 'layout') return stored;
    if (payload.expected_revision !== stored.revision) return { ok: false, code: 'revision_conflict' };
    stored = { ...stored, revision: stored.revision + 1, layout: ordinary(payload.layout) }; return stored;
  } }, () => {});
  await editor.refresh();
  editor.edit('client-a', placement({ gap_px: 12 }));
  stored = layoutReply(2, { 'client-b': placement({ edge: 'below' }) });
  await editor.save();
  assert.equal(editor.dirty(), true);
  assert.equal(editor.confirmationRequired, false);
  await editor.refresh();
  assert.equal(editor.draft.placements['client-a'].gap_px, 12);
  assert.equal(editor.draft.placements['client-b'].edge, 'below');
  await editor.save();
  assert.equal(editor.saved.revision, 3);
  assert.equal(editor.dirty(), false);
});

test('layout inventory polls update readiness without overwriting unsaved positions or revision', async () => {
  let current = layoutReply();
  const editor = new LayoutEditor({ async call() { return current; } }, () => {});
  await editor.refresh();
  editor.edit('client-a', placement({ gap_px: 24 }));
  current = layoutReply(8, { 'client-a': placement({ gap_px: 999 }) });
  current.clients[0].ready = false; current.clients[0].state = 'cleanup_failed'; current.clients[0].error = 'Display removal is still pending';
  await editor.pollInventory();
  assert.equal(editor.saved.revision, 1);
  assert.equal(editor.saved.clients[0].state, 'cleanup_failed');
  assert.equal(editor.draft.placements['client-a'].gap_px, 24);
  assert.equal(editor.dirty(), true);
});

test('invalid layout edits stay visible, uncertain writes require a read-only check, primary is exclusive', async () => {
  let current = layoutReply(1, { 'client-b': placement({ primary: true }) });
  let writes = 0;
  const editor = new LayoutEditor({ async call(alias, payload) {
    if (alias === 'layout') return current;
    writes++; current = { ...current, revision: 2, layout: ordinary(payload.layout) };
    throw new Error('reply lost');
  } }, () => {});
  await editor.refresh();
  editor.edit('client-a', placement({ gap_px: NaN }));
  assert.equal(editor.dirty(), true);
  assert.match(editor.validationError(), /whole number/);
  await editor.save(); assert.equal(writes, 0);
  editor.edit('client-a', placement({ primary: true }));
  assert.equal(editor.draft.placements['client-b'].primary, false);
  await editor.save(); assert.equal(editor.confirmationRequired, true);
  await editor.save(); assert.equal(writes, 1);
  await editor.refresh(); assert.equal(editor.dirty(), false);
});

test('profile validation enforces catalog identity and bounded mode/scale values', () => {
  for (const patch of [{ width: 319 }, { height: 4321 }, { refresh_millihz: 1000.5 }, { scale: 110 }, { name: ' ' }, { uuid: 'invalid' }]) {
    assert.throws(() => parseProfile(profile(patch)));
  }
  assert.throws(() => parseProfilesReply(profilesReply(1, [profile(), profile()])), /duplicate/);
  assert.throws(() => parseProfile(profile({ target: { kind: 'existing', selector: 'x'.repeat(257) } })), /identity/);
});

test('profile create/update/delete use catalog revisions and confirm exact readback', async () => {
  let stored = profilesReply();
  const calls = [];
  const editor = new ProfileEditor({ async call(alias, payload) {
    calls.push([alias, ordinary(payload ?? {})]);
    if (alias === 'profile_upsert') {
      const next = { ...payload.profile,
        id: payload.profile.id || 'app-0000000000000002',
        uuid: payload.profile.uuid || '22222222-2222-4333-8444-555555555555' };
      stored = profilesReply(stored.revision + 1, [...stored.profiles.filter(profile => profile.id !== next.id), next]);
      return { ok: true, revision: stored.revision, profile: next };
    }
    if (alias === 'profile_delete') {
      stored = profilesReply(stored.revision + 1, stored.profiles.filter(profile => profile.id !== payload.id));
      return { ok: true, revision: stored.revision };
    }
    return stored;
  } }, () => {});
  await editor.refresh();
  editor.select(''); editor.edit({ name: 'Drawing tablet', width: 2560, height: 1600, refresh_millihz: 120_000, hdr_enabled: true });
  await editor.save();
  assert.equal(editor.error, false);
  assert.equal(editor.draft.id, 'app-0000000000000002');
  assert.equal(editor.dirty(), false);
  const createdUuid = editor.draft.uuid;
  editor.edit({ name: 'Drawing tablet HDR' }); await editor.save();
  assert.equal(editor.draft.uuid, createdUuid);
  assert.equal(stored.profiles.length, 2);
  await editor.remove();
  assert.equal(stored.profiles.length, 1);
  assert.equal(editor.dirty(), false);
  assert.match(editor.message, /deleted and confirmed/);
  assert.deepEqual(calls.filter(([alias]) => alias === 'profile_upsert').map(([, body]) => body.expected_revision), [1, 2]);
  assert.equal(calls.find(([alias]) => alias === 'profile_delete')[1].expected_revision, 3);
});

test('profile conflict keeps edited fields and rebases untouched mode fields', async () => {
  let current = profilesReply();
  const editor = new ProfileEditor({ async call(alias) {
    return alias === 'profiles' ? current : { ok: false, code: 'revision_conflict' };
  } }, () => {});
  await editor.refresh(); editor.edit({ name: 'My edited name' });
  current = profilesReply(2, [profile({ height: 1600 })]);
  await editor.save(); assert.equal(editor.dirty(), true);
  await editor.refresh();
  assert.equal(editor.draft.name, 'My edited name');
  assert.equal(editor.draft.height, 1600);
});

test('lost create replies reconcile generated identity from saved profiles without duplicating', async () => {
  let current = profilesReply();
  let writes = 0;
  const editor = new ProfileEditor({ async call(alias, payload) {
    if (alias === 'profiles') return current;
    writes++;
    current = profilesReply(2, [...current.profiles, profile({ ...payload.profile,
      id: 'app-0000000000000002', uuid: '22222222-2222-4333-8444-555555555555' })]);
    throw new Error('update reply lost');
  } }, () => {});
  await editor.refresh(); editor.select(''); editor.edit({ name: 'New profile' });
  await editor.save(); assert.equal(editor.confirmationRequired, true);
  await editor.save(); assert.equal(writes, 1);
  await editor.refresh();
  assert.equal(editor.dirty(), false);
  assert.equal(editor.draft.id, 'app-0000000000000002');
  await editor.save(); assert.equal(writes, 1);
});

test('unavailable saved targets remain explicit and new arbitrary selectors are rejected', async () => {
  const savedTarget = { kind: 'existing', selector: 'display-old' };
  const editor = new ProfileEditor({ async call(alias) {
    if (alias === 'profile_targets') return { ok: true, targets: [{ selector: 'display-main', label: 'Desktop', ready: true }] };
    return profilesReply(1, [profile({ target: savedTarget })]);
  } }, () => {});
  await editor.refresh(); await editor.refreshTargets();
  editor.edit({ name: 'Same display' });
  assert.equal(editor.validationError(), null);
  editor.edit({ target: { kind: 'existing', selector: 'arbitrary-path' } });
  assert.match(editor.validationError(), /unavailable/);
  editor.edit({ target: { kind: 'existing', selector: 'display-main' } });
  assert.equal(editor.validationError(), null);
});

test('disposal ignores late profile replies and never releases another editor’s guards', async () => {
  let resolve;
  const response = new Promise(yes => { resolve = yes; });
  let changes = 0;
  const editor = new ProfileEditor({ call: () => response }, () => changes++);
  const loading = editor.refresh(); editor.dispose();
  const before = changes; resolve(profilesReply()); await loading;
  assert.equal(editor.saved, null); assert.equal(changes, before);
});
