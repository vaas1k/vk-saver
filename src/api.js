import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

// Demo mode is runtime: the fake backend replaces Tauri until the next real pick.
// In dev without Tauri the fake is on automatically (import.meta.env.DEV).
let DEMO = false;
export const setDemo = (v) => { DEMO = v; };

const fake = () => import('./dev/fake.js');
const autoFake = () => import.meta.env.DEV && !window.__TAURI_INTERNALS__;

// A rejected command (worker gone) becomes the same error shape the backend replies with.
function call(cmd, args) {
  const p = DEMO || autoFake() ? fake().then((f) => f.invoke(cmd, args)) : invoke(cmd, args);
  return p.catch(() => ({ error: 'internal' }));
}

// Both backends are subscribed: the fake emits nothing unless demo is on,
// the Tauri one is only registered where Tauri exists.
export function on(name, handler) {
  const offs = [fake().then((f) => f.listen(name, handler))];
  if (window.__TAURI_INTERNALS__) offs.push(listen(name, (e) => handler(e.payload)));
  return Promise.all(offs).then((r) => () => r.forEach((off) => off()));
}

export const pickExport = (kind) => call('pick_export', { kind });
export const lastExport = () => call('last_export');
export const scan = () => call('scan');
export const start = (excluded) => call('start', { excluded });
export const stop = () => call('stop');
export const result = () => call('result');
export const undo = () => call('undo');
export const saveDiagnostics = () => call('save_diagnostics');
export const search = (query) => call('search', { query });
export const openInBrowser = () => call('open_in_browser');
export const openFolder = () => call('open_folder');
export const openPage = (page) => call('open_page', { page });
export const openExportHelp = () => call('open_export_help');
