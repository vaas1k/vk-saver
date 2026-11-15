import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const check = (dir) => spawnSync(process.execPath, ['scripts/check-dist.js', dir]).status;

test('dist guard fails on the fake backend marker', () => {
  const dir = mkdtempSync(join(tmpdir(), 'dist-'));
  mkdirSync(join(dir, 'assets'));
  writeFileSync(join(dir, 'assets', 'a.js'), 'const ok = 1;');
  assert.equal(check(dir), 0);
  writeFileSync(join(dir, 'assets', 'b.js'), 'const root = "C:\\\\VkSaverFake";');
  assert.equal(check(dir), 1);
});
