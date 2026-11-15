// Dev-only stand-in for the Tauri backend, never part of the production bundle (marker VkSaverFake).
// Same JSON shapes as src-tauri, synthetic numbers only.
const handlers = {};
let cancel = false;

export function listen(name, handler) {
  (handlers[name] ??= new Set()).add(handler);
  return Promise.resolve(() => handlers[name].delete(handler));
}

const emit = (name, payload) => handlers[name]?.forEach((h) => h(payload));
const group = (count, bytes, saved = 0) => ({ count, bytes, saved });
const chat = (peer, label, count) => ({ peer, label, photos: group(count, count * 470000), voice: group(2, 40000), videos: 1, docs: 0, photo_links: 0 });
const TOTAL = 40;
const TICK_MS = 150;

const SUMMARY = {
  root: 'C:\\VkSaverFake\\Archive',
  free_bytes: 41 * 1024 ** 3,
  albums: group(1200, 2147483648, 12),
  messages: group(4400, 5000000000),
  voice: group(93, 20971520),
  other: group(1, 90000),
  chats: [chat(2000000101, 'Тестовый чат', 1204), chat(2000000102, '', 310), chat(100002, 'Тестовый собеседник', 57), chat(-200003, 'Тестовое сообщество', 9)],
  videos: 310,
  docs: 57,
  photo_links: 12,
  eta_s: 7200,
  too_large: 0,
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function start() {
  cancel = false;
  emit('phase', { phase: 'fetch' });
  let done = 0;
  for (; done < TOTAL && !cancel; done++) {
    if (done === TOTAL / 2) {
      emit('paused', { until_ms: Date.now() + 60000 });
      await sleep(TICK_MS * 5);
    }
    emit('progress', { done, total: TOTAL, bytes: done * 470000, mb_s: 3.1, eta_s: (TOTAL - done) * 30, section: done < TOTAL / 2 ? 'albums' : 'messages', saved: Math.max(0, done - 4), gone: Math.min(done, 2), failed: done > 10 ? 1 : 0 });
    await sleep(TICK_MS);
  }
  emit('phase', { phase: 'done' });
  const rewrite = { pages: 3, changed: 3, busy: 0, skipped: 0 };
  return { stop: cancel ? 'cancelled' : null, saved: done - 4, reused: 5, bytes: done * 470000, gone: 2, host_dead: 1, failed: 1, rewrite, report_failed: false };
}

const COMMANDS = {
  pick_export: async ({ kind }) => {
    if (kind === 'zip') {
      cancel = false;
      for (let done = 0; done <= TOTAL; done += 4) {
        if (cancel) return { error: 'cancelled' };
        emit('unzip', { done: done * 470000, total: TOTAL * 470000 });
        await sleep(TICK_MS);
      }
    }
    return kind === 'folder' || kind === 'zip' ? { root: SUMMARY.root } : null;
  },
  last_export: () => null,
  scan: () => SUMMARY,
  start,
  stop: () => {
    cancel = true;
    return null;
  },
  result: () => ({ saved: 5000, bytes: 7000000000, gone: 210, host_dead: 60, failed: 14, videos: 310, docs: 57, photo_links: 12 }),
  undo: () => ({ restored: 2, busy: 0, kept: 0, skipped: 0 }),
  save_diagnostics: () => ({ saved: true }),
  open_in_browser: () => ({}),
  open_folder: () => ({}),
  open_export_help: () => ({}),
};

export async function invoke(cmd, args = {}) {
  const f = COMMANDS[cmd];
  if (!f) throw new Error('unknown command ' + cmd);
  return f(args);
}
