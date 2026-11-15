import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

// import.meta.env.DEV is false in production builds, so the fake module is dropped from the bundle.
const FAKE = import.meta.env.DEV && !window.__TAURI_INTERNALS__;

// A rejected command (worker gone) becomes the same error shape the backend replies with.
function call(cmd, args) {
  const p = FAKE ? import('./dev/fake.js').then((f) => f.invoke(cmd, args)) : invoke(cmd, args);
  return p.catch(() => ({ error: 'internal' }));
}

export function on(name, handler) {
  if (FAKE) return import('./dev/fake.js').then((f) => f.listen(name, handler));
  return listen(name, (e) => handler(e.payload));
}

export const pickExport = (kind) => call('pick_export', { kind });
export const lastExport = () => call('last_export');
export const scan = () => call('scan');
export const start = (excluded) => call('start', { excluded });
export const stop = () => call('stop');
export const result = () => call('result');
export const undo = () => call('undo');
export const saveDiagnostics = () => call('save_diagnostics');
export const openInBrowser = () => call('open_in_browser');
export const openFolder = () => call('open_folder');
export const openExportHelp = () => call('open_export_help');
