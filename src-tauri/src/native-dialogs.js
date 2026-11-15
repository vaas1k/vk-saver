// Runs before tauri-plugin-dialog's init script, which replaces alert/confirm in every webview.
// Outside the main window that IPC is refused, and its async confirm() answers a truthy Promise.
(() => {
  const label = window.__TAURI_INTERNALS__?.metadata?.currentWebview?.label;
  if (label === 'main') return;
  for (const name of ['alert', 'confirm', 'prompt']) {
    const native = window[name];
    Object.defineProperty(window, name, { get: () => native, set: () => {}, configurable: false });
  }
})();
