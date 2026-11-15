import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const script = readFileSync(new URL('../src-tauri/src/native-dialogs.js', import.meta.url), 'utf8');

// The assignments of tauri-plugin-dialog 2.8.4 src/init-iife.js, which runs after our script.
const shim = `"use strict";
window.alert = function (e) {};
window.confirm = async function (e) { return true; };`;

function load(label) {
  const native = { alert() {}, confirm() { return false; }, prompt() { return null; } };
  const window = { ...native, __TAURI_INTERNALS__: { metadata: { currentWebview: { label } } } };
  new Function('window', script)(window);
  new Function('window', shim)(window);
  return { window, native };
}

test('login window keeps native dialogs', () => {
  const { window, native } = load('vk-login');
  for (const name of ['alert', 'confirm', 'prompt']) assert.equal(window[name], native[name]);
  assert.equal(window.confirm('x'), false);
});

test('subframe without metadata keeps native dialogs', () => {
  const native = { alert() {}, confirm() { return false; }, prompt() { return null; } };
  const window = { ...native };
  new Function('window', script)(window);
  new Function('window', shim)(window);
  assert.equal(window.confirm, native.confirm);
});

test('main window left to the plugin', () => {
  const { window, native } = load('main');
  assert.notEqual(window.confirm, native.confirm);
});
