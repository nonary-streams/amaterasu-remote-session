import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';
const context = vm.createContext({ Error, TextEncoder });
vm.runInContext(ts.transpile(['src/layout_model.ts', 'src/settings.ts', 'src/profile_model.ts']
  .map(file => readFileSync(file, 'utf8')).join('\n'), { target: ts.ScriptTarget.ES2022 }), context);
const { readProfiles, ProfileEditor, parseProfilesReply } = context.RemoteSessionUi;
const ordinary = value => JSON.parse(JSON.stringify(value));
const id = number => `app-${number.toString(16).padStart(16, '0')}`;
const profile = (number, patch = {}) => ({ id: id(number), uuid: `${number.toString(16).padStart(8, '0')}-2222-4333-8444-555555555555`,
  name: `Screen ${number}`, width: 1920, height: 1080, refresh_millihz: 60_000,
  hdr_enabled: false, scale: 100, target: { kind: 'owned' }, ...patch });
const page = (revision, profiles, after = null) => ({ ok: true, revision, profiles,
  next_cursor: after === null ? null : { revision, after: id(after) } });

test('reader collects one complete revision across short and empty advancing pages', async () => {
  const replies = [page(7, [profile(1), profile(2)], 5), page(7, [], 9), page(7, [profile(10), profile(11)])];
  const calls = [];
  const complete = await readProfiles({ async call(alias, payload) {
    calls.push([alias, ordinary(payload)]); return replies.shift();
  } });
  assert.equal(complete.revision, 7);
  assert.deepEqual(ordinary(complete.profiles.map(profile => profile.id)), [id(1), id(2), id(10), id(11)]);
  assert.deepEqual(calls, [
    ['profiles', { limit: 32 }],
    ['profiles', { limit: 32, cursor: { revision: 7, after: id(5) } }],
    ['profiles', { limit: 32, cursor: { revision: 7, after: id(9) } }],
  ]);
});

test('reader rejects changed revisions, duplicate IDs/UUIDs, and nonadvancing cursors', async () => {
  const cases = [
    [page(2, [profile(2)]), /changed while loading/],
    [page(1, [profile(1)]), /duplicate|out of order/],
    [page(1, [profile(2, { uuid: profile(1).uuid })]), /duplicate/],
    [page(1, [], 1), /did not advance/],
    [page(1, [profile(3)], 2), /did not advance/],
    [{ ok: false, code: 'revision_conflict' }, /another editor/],
  ];
  for (const [second, expected] of cases) {
    const replies = [page(1, [profile(1)], 1), second];
    await assert.rejects(readProfiles({ async call() { return replies.shift(); } }), expected);
  }
});

test('page validation requires explicit continuation and rejects oversized or invalid cursors', () => {
  const missing = page(1, []); delete missing.next_cursor;
  for (const invalid of [
    missing,
    { ...page(1, []), next_cursor: { revision: 2, after: id(1) } },
    { ...page(1, []), next_cursor: { revision: 1, after: 'not-a-catalog-id' } },
    { ...page(1, []), next_cursor: { revision: 1, after: id(0) } },
    page(1, Array.from({ length: 33 }, (_, index) => profile(index + 1))),
    { ...page(1, []), padding: 'x'.repeat(12_000) },
  ]) assert.throws(() => parseProfilesReply(invalid));
});

test('refresh never publishes a partial menu or overwrites a draft after a later page fails', async () => {
  let replies = [page(1, [profile(1)])];
  const editor = new ProfileEditor({ async call() {
    const next = replies.shift();
    if (next instanceof Error) throw next;
    return next;
  } }, () => {});
  await editor.refresh(); editor.edit({ name: 'Unsaved profile name' });
  const saved = editor.saved;
  replies = [page(2, [profile(2)], 2), new Error('next page unavailable')];
  await editor.refresh();
  assert.equal(editor.saved, saved);
  assert.equal(editor.draft.name, 'Unsaved profile name');
  assert.equal(editor.dirty(), true);
  assert.equal(editor.error, true);
  assert.match(editor.message, /next page unavailable/);
});

test('closing during pagination stops further broker calls and ignores accumulated rows', async () => {
  let release;
  const response = new Promise(resolve => { release = resolve; });
  let calls = 0;
  const editor = new ProfileEditor({ async call() {
    calls++;
    return calls === 1 ? page(1, [profile(1)], 1) : response;
  } }, () => {});
  const loading = editor.refresh();
  await new Promise(setImmediate);
  assert.equal(calls, 2);
  assert.equal(editor.saved, null);
  editor.dispose(); release(page(1, [profile(2)], 2)); await loading;
  assert.equal(editor.saved, null);
  assert.equal(calls, 2);
});

test('save readback checks later pages before accepting a profile update', async () => {
  let reading = false;
  const editor = new ProfileEditor({ async call(alias, payload) {
    if (alias === 'profile_upsert') {
      reading = true;
      return { ok: true, revision: 2, profile: payload.profile };
    }
    if (!reading) return page(1, [profile(1), profile(2)]);
    return payload.cursor ? page(2, [profile(2)]) : page(2, [profile(1)], 1);
  } }, () => {});
  await editor.refresh(); editor.select(id(2)); editor.edit({ name: 'My edited profile' });
  await editor.save();
  assert.equal(editor.dirty(), true);
  assert.equal(editor.confirmationRequired, true);
  assert.match(editor.message, /changed before readback/);
});

test('delete readback cannot miss a retained profile on a later page', async () => {
  let deleting = false;
  const editor = new ProfileEditor({ async call(alias, payload) {
    if (alias === 'profile_delete') { deleting = true; return { ok: true, revision: 2 }; }
    if (!deleting) return page(1, [profile(1), profile(2)]);
    return payload.cursor ? page(2, [profile(2)]) : page(2, [profile(1)], 1);
  } }, () => {});
  await editor.refresh(); editor.select(id(2)); await editor.remove();
  assert.equal(editor.confirmationRequired, true);
  assert.match(editor.message, /still present in readback/);
  assert.equal(editor.draft.id, id(2));
});

test('lost create replies reconcile generated identity on a later page without duplication', async () => {
  let created = null, writes = 0;
  const editor = new ProfileEditor({ async call(alias, payload) {
    if (alias === 'profile_upsert') {
      writes++;
      created = profile(2, { ...payload.profile, id: id(2), uuid: profile(2).uuid });
      throw new Error('creation reply lost');
    }
    if (!created) return page(1, [profile(1)]);
    return payload.cursor ? page(2, [created]) : page(2, [profile(1)], 1);
  } }, () => {});
  await editor.refresh(); editor.select(''); editor.edit({ name: 'New saved profile' });
  await editor.save(); await editor.refresh();
  assert.equal(editor.dirty(), false);
  assert.equal(editor.draft.id, id(2));
  await editor.save(); assert.equal(writes, 1);
});

test('observed existing-display mode remains separate from requested stream mode', async () => {
  const editor = new ProfileEditor({ async call(alias) {
    if (alias === 'profile_targets') return { ok: true, targets: [{ selector: 'screen', label: 'Desktop', ready: true,
      mode: { width: 3840, height: 2160, refresh_millihz: 60_000, hdr_enabled: false } }] };
    return page(1, [profile(1, { width: 2560, height: 1600, refresh_millihz: 120_000, hdr_enabled: true,
      target: { kind: 'existing', selector: 'screen' } })]);
  } }, () => {});
  await editor.refresh(); await editor.refreshTargets();
  assert.equal(editor.targets[0].mode.hdr_enabled, false);
  assert.equal(editor.draft.hdr_enabled, true);
  assert.equal(editor.targets[0].mode.refresh_millihz, 60_000);
  assert.equal(editor.draft.refresh_millihz, 120_000);
});
