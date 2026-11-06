import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';
const context = vm.createContext({});
vm.runInContext(ts.transpile(readFileSync('src/surface_state.ts', 'utf8'), {
  target: ts.ScriptTarget.ES2022,
}), context);

test('one editor cannot clear another editor’s drafts or pending-write guard', () => {
  const states = [];
  const guards = new context.RemoteSessionUi.EditorGuards((dirty, busy) => states.push([dirty, busy]));
  const preferences = guards.create(), layout = guards.create(), profiles = guards.create();
  preferences.update(true, false);
  layout.update(true, true);
  preferences.update(false, false);
  assert.deepEqual(states.at(-1), [true, true]);
  layout.update(false, false);
  assert.deepEqual(states.at(-1), [false, false]);
  profiles.update(true, false);
  layout.dispose();
  assert.deepEqual(states.at(-1), [true, false]);
  profiles.dispose();
  assert.deepEqual(states.at(-1), [false, false]);
  const count = states.length;
  profiles.update(true, true);
  profiles.dispose();
  assert.equal(states.length, count);
  preferences.dispose();
});
