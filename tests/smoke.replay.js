import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { initial, reduce, progressText } from '../src/run-state.js';

// Written by `cargo test -p vk-saver --lib smoke` (pretest): real backend events in order.
const events = JSON.parse(readFileSync(new URL('../src-tauri/target/smoke-events.json', import.meta.url), 'utf8'));

test('backend smoke events drive the reducer', () => {
  let s = reduce({ ...initial, screen: 'found' }, { type: 'started' });
  const names = new Set();
  for (const { event, payload } of events) {
    names.add(event);
    if (event === 'progress') s = reduce(s, { type: 'progress', p: payload });
    if (event === 'paused') s = reduce(s, { type: 'paused', until_ms: payload.until_ms });
  }
  assert.ok(names.has('progress') && names.has('phase'));
  assert.equal(s.screen, 'download');
  assert.ok(s.progress.total > 0);
  const t = progressText(s.progress);
  assert.match(t.count, /из/);
  assert.match(t.speed, /МБ\/с$/);
  assert.ok(Object.values(t).every((x) => !x.includes('http') && !x.includes('\\')));
});
