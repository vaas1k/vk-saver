import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { initial, reduce, etaText, stopText, reasonRows, foundRows, linksText, progressText, countsText, nothingToDownload, formatBytes, formatCount, unzipText, undoText, tooLargeText, unrewrittenText, reusedText, reportFailedText, isGroupChat, shownChats, allSelected, chatRows } from '../src/run-state.js';
import { invoke, listen } from '../src/dev/fake.js';

const group = (count, bytes = 0, saved = 0) => ({ count, bytes, saved });
const chat = (peer, label, count) => ({ peer, label, photos: group(count, count * 1000), voice: group(0), videos: 0, docs: 0, photo_links: 0 });
const SUMMARY = {
  root: 'C:\\Users\\Тест\\Archive',
  free_bytes: 41 * 1024 ** 3,
  albums: group(1200, 2147483648, 12),
  messages: group(4400, 5e9),
  voice: group(0),
  other: group(0),
  chats: [chat(2000000101, 'Тестовый чат', 5), chat(2000000102, '', 900), chat(-200003, 'Тестовое сообщество', 40)],
  videos: 0,
  docs: 0,
  photo_links: 0,
  eta_s: 7200,
};

const found = () => reduce(reduce(initial, { type: 'opened', reply: { root: SUMMARY.root } }), { type: 'scanned', reply: SUMMARY });

test('pick -> found on open result', () => {
  assert.equal(initial.screen, 'pick');
  assert.equal(reduce(initial, { type: 'opened', reply: null }), initial);
  const bad = reduce(initial, { type: 'opened', reply: { error: 'not_export' } });
  assert.equal(bad.screen, 'pick');
  assert.equal(bad.error, 'Это не похоже на архив ВКонтакте. Запросите архив по ссылке на странице помощи ВКонтакте выше');
  const opened = reduce(bad, { type: 'opened', reply: { root: 'x' } });
  assert.equal(opened.screen, 'found');
  assert.equal(opened.error, null);
  assert.equal(opened.summary, null);
  const s = found();
  assert.equal(s.screen, 'found');
  assert.equal(s.summary, SUMMARY);
  const lost = reduce(opened, { type: 'scanned', reply: { error: 'not_export' } });
  assert.equal(lost.screen, 'pick');
});

test('excluded peers sent as strings', () => {
  let s = found();
  assert.deepEqual(s.excluded, []);
  s = reduce(s, { type: 'toggle', peer: 2000000102 });
  s = reduce(s, { type: 'toggle', peer: -200003 });
  s = reduce(s, { type: 'toggle', peer: 2000000101 });
  s = reduce(s, { type: 'toggle', peer: 2000000101 });
  assert.deepEqual(s.excluded, ['2000000102', '-200003']);
});

test('progress events update counts and eta', () => {
  let s = reduce(found(), { type: 'started' });
  assert.equal(s.screen, 'download');
  s = reduce(s, { type: 'paused', until_ms: new Date(2026, 0, 1, 9, 5).getTime() });
  assert.equal(s.pausedText, 'ВКонтакте просит подождать, продолжим в 09:05');
  s = reduce(s, { type: 'progress', p: { done: 10, total: 40, bytes: 5e6, mb_s: 1.25, eta_s: 95, section: 'albums' } });
  assert.equal(s.pausedText, null);
  assert.equal(s.progress.done, 10);
  assert.deepEqual(progressText(s.progress), {
    count: '10 из 40',
    speed: '1,3 МБ/с',
    eta: 'около 2 мин',
    section: 'Фотографии из альбомов',
  });
  assert.equal(etaText(null), '');
  assert.equal(etaText(30), 'меньше минуты');
  assert.equal(etaText(7200), 'около 2 ч');
  s = reduce(s, { type: 'finished', reply: { stop: null, saved: 30, bytes: 1, gone: 0, host_dead: 0, failed: 0, rewrite: {} } });
  s = reduce(s, { type: 'result', reply: { saved: 30, bytes: 1, gone: 0, host_dead: 0, failed: 0, videos: 0, docs: 0, photo_links: 0 } });
  assert.equal(s.screen, 'result');
  assert.equal(s.stop, null);
});

test('stop kinds map to Russian texts', () => {
  assert.equal(stopText('throttled'), 'ВКонтакте ограничил скачивание, попробуйте позже');
  assert.equal(stopText('network'), 'Нет соединения с интернетом – нажмите "Докачать", когда оно появится');
  assert.equal(stopText('no_space'), 'На диске закончилось место');
  assert.equal(stopText('cancelled'), 'Остановлено. То, что уже скачано, сохранено.');
  assert.equal(stopText(null), '');
  assert.ok(stopText('storage').length > 0);
  const s = reduce(reduce(found(), { type: 'started' }), { type: 'finished', reply: { stop: 'network', saved: 1 } });
  assert.equal(s.stop, 'network');
  assert.equal(reduce(reduce(found(), { type: 'started' }), { type: 'finished', reply: { error: 'storage' } }).stop, 'storage');
});

test('result reasons grouped', () => {
  assert.deepEqual(reasonRows({ saved: 5, bytes: 9, gone: 3, host_dead: 0, failed: 2, videos: 4, docs: 1, photo_links: 2 }), [
    { label: 'удалены во ВКонтакте (404)', count: 3 },
    { label: 'не удалось скачать – попробуйте ещё раз', count: 2 },
    { label: 'видео и документы – только ссылки', count: 7 },
  ]);
  assert.deepEqual(reasonRows({ saved: 1, bytes: 1, gone: 0, host_dead: 6, failed: 0, videos: 0, docs: 0, photo_links: 0 }), [
    { label: 'старый сервер ВКонтакте не отвечает', count: 6 },
  ]);
});

test('empty scan parts show nothing', () => {
  const rows = foundRows(SUMMARY);
  assert.deepEqual(rows.map((r) => r.label), ['Фотографии из альбомов', 'Фото в переписках']);
  assert.equal(rows[0].saved, 'уже скачано 12');
  assert.equal(rows[1].saved, '');
  assert.equal(linksText(SUMMARY), '');
  assert.equal(linksText({ ...SUMMARY, videos: 310, docs: 57 }).replace(/\s/g, ' '), 'Только ссылки на ВК (не скачиваются): видео 310, документы 57');
});

test('section survives byte-level progress events', () => {
  let s = reduce(found(), { type: 'started' });
  s = reduce(s, { type: 'progress', p: { done: 1, total: 4, bytes: 1, mb_s: 1, eta_s: 1, section: 'albums' } });
  s = reduce(s, { type: 'progress', p: { done: 1, total: 4, bytes: 2, mb_s: 1, eta_s: 1, section: null } });
  assert.equal(s.progress.section, 'albums');
});

test('busy reply to start does not finish the run', () => {
  const s = reduce(found(), { type: 'started' });
  assert.equal(reduce(s, { type: 'finished', reply: { error: 'busy' } }), s);
});

test('nothingToDownload', () => {
  const g = (count) => ({ count, bytes: 0, saved: 0 });
  assert.equal(nothingToDownload({ albums: g(0), messages: g(0), voice: g(0), chats: [] }), true);
  assert.equal(nothingToDownload({ albums: g(1), messages: g(0), voice: g(0), chats: [] }), false);
});

test('countsText hides zero parts', () => {
  assert.equal(countsText({ saved: 1204, gone: 3, failed: 1 }).replace(/\s/g, ' '), 'Сохранено 1 204 · удалено во ВКонтакте 3 · не скачано 1');
  assert.equal(countsText({ saved: 5, gone: 0, failed: 0 }), 'Сохранено 5');
  assert.equal(countsText({ saved: 0, gone: 0, failed: 0 }), '');
  assert.equal(countsText(null), '');
});

test('no url or path rendered', async () => {
  let s = initial;
  const off = await listen('progress', (p) => {
    s = reduce(s, { type: 'progress', p });
    invoke('stop');
  });
  s = reduce(s, { type: 'opened', reply: await invoke('pick_export', { kind: 'folder' }) });
  const sum = await invoke('scan');
  s = reduce(s, { type: 'scanned', reply: sum });
  s = reduce(s, { type: 'started' });
  const rep = await invoke('start', { excluded: [] });
  s = reduce(s, { type: 'finished', reply: rep });
  const view = await invoke('result');
  s = reduce(s, { type: 'result', reply: view });
  off();
  const texts = [
    ...foundRows(sum).flatMap((r) => [r.label, r.text, r.saved]),
    linksText(sum),
    etaText(sum.eta_s),
    ...sum.chats.map((c) => c.label),
    ...Object.values(progressText(s.progress)),
    countsText(s.progress),
    stopText(rep.stop),
    ...reasonRows(view).map((r) => r.label),
    formatBytes(view.bytes),
    formatCount(view.saved),
  ];
  assert.equal(s.screen, 'result');
  assert.equal(s.stop, 'cancelled');
  for (const t of texts) {
    assert.equal(typeof t, 'string');
    assert.ok(!t.includes('http') && !t.includes('\\'), t);
  }
});

test('formatBytes', () => {
  assert.equal(formatBytes(512), '512 Б');
  assert.equal(formatBytes(2147483648), '2,0 ГБ');
  assert.equal(formatBytes(5 * 1024 * 1024), '5,0 МБ');
});

test('formatCount groups digits', () => {
  assert.equal(formatCount(1234567).replace(/\s/g, ' '), '1 234 567');
});

test('zip progress then found', async () => {
  let s = reduce({ ...initial, error: 'old' }, { type: 'unzip', p: { done: 512, total: 2048 } });
  assert.equal(s.screen, 'pick');
  assert.equal(s.error, null);
  assert.equal(unzipText(s.unzip), 'Распаковка: 512 Б из 2,0 КБ');
  s = reduce(s, { type: 'opened', reply: { root: 'x' }, zip: true });
  assert.equal(s.screen, 'found');
  assert.equal(s.unzip, null);
  const ro = reduce(reduce(initial, { type: 'unzip', p: { done: 0, total: 1 } }), { type: 'opened', reply: { error: 'storage' }, zip: true });
  assert.equal(ro.error, 'Не получилось распаковать рядом с архивом – распакуйте его сами и выберите папку');
  assert.equal(ro.unzip, null);
  assert.equal(reduce(initial, { type: 'opened', reply: { error: 'bad_zip' }, zip: true }).error, stopText('bad_zip'));
  const stopped = reduce(reduce(initial, { type: 'unzip', p: { done: 1, total: 2 } }), { type: 'opened', reply: { error: 'cancelled' }, zip: true });
  assert.equal(stopped.error, 'Распаковка остановлена');
  assert.equal(stopped.unzip, null);
  assert.notEqual(stopText('bad_zip'), stopText('unknown'));
  const seen = [];
  const off = await listen('unzip', (p) => seen.push(p));
  assert.deepEqual(await invoke('pick_export', { kind: 'zip' }), { root: 'C:\\VkSaverFake\\Archive' });
  off();
  assert.ok(seen.length > 1);
  assert.equal(seen.at(-1).done, seen.at(-1).total);
});

test('undo confirm then notice', async () => {
  let s = reduce(initial, { type: 'result', reply: { saved: 1, bytes: 1, gone: 0, host_dead: 0, failed: 0, videos: 0, docs: 0, photo_links: 0 } });
  assert.equal(s.undoAsk, false);
  s = reduce(s, { type: 'undo_ask' });
  assert.equal(s.undoAsk, true);
  assert.equal(reduce(s, { type: 'undo_cancel' }).undoAsk, false);
  const done = reduce(s, { type: 'undone', reply: { restored: 3, busy: 0, kept: 0 } });
  assert.equal(done.undoAsk, false);
  assert.equal(done.undoText, 'Страницы возвращены. Файлы остались в папках media – "Докачать" снова подключит их');
  const partial = reduce(s, { type: 'undone', reply: { restored: 1, busy: 2, kept: 1 } });
  assert.ok(partial.undoText.startsWith('Страницы возвращены.'));
  assert.ok(partial.undoText.includes('2 страницы открыты в другой программе'), partial.undoText);
  assert.ok(partial.undoText.includes('1 страница изменена после vk-saver и оставлена как есть'), partial.undoText);
  assert.equal(done.undoError, false);
  const failed = reduce(s, { type: 'undone', reply: { error: 'storage' } });
  assert.equal(failed.undoText, 'Не получилось вернуть страницы');
  assert.equal(failed.undoError, true);
  assert.equal(failed.undoAsk, false);
  assert.equal(reduce(done, { type: 'started' }).undoText, null);
  assert.deepEqual(await invoke('undo'), { restored: 2, busy: 0, kept: 0, skipped: 0 });
});

test('undo with nothing restored names only what was left', () => {
  const t = undoText({ restored: 0, busy: 2, kept: 1 });
  assert.ok(!t.includes('возвращены'), t);
  assert.ok(t.startsWith('2 страницы открыты в другой программе'), t);
  assert.ok(t.includes('1 страница изменена после vk-saver'), t);
  assert.ok(undoText({ restored: 0, busy: 0, kept: 0 }).startsWith('Страницы возвращены.'));
});

test('pages not rewritten are counted', () => {
  assert.equal(unrewrittenText({ rewrite: { pages: 9, changed: 3, busy: 2, skipped: 1 } }), 'Не удалось обновить страниц: 3');
  assert.equal(unrewrittenText({ rewrite: { pages: 9, changed: 3, busy: 0, skipped: 0 } }), null);
  assert.equal(unrewrittenText(null), null);
});

test('reused files shown on result', async () => {
  assert.equal(reusedText({ reused: 12 }), 'Взято из прошлого архива: 12');
  assert.equal(reusedText({ reused: 0 }), null);
  assert.equal(reusedText(null), null);
  const s = reduce(reduce(found(), { type: 'started' }), { type: 'finished', reply: { stop: null, saved: 3, reused: 2, bytes: 1, rewrite: {} } });
  assert.equal(reusedText(s.report), 'Взято из прошлого архива: 2');
  const result = readFileSync(new URL('../src/screens/Result.jsx', import.meta.url), 'utf8');
  assert.match(result, /reusedText\(s\.report\)/);
  const reply = await invoke('start', { excluded: [] });
  assert.equal(typeof reply.reused, 'number');
});

test('developer buttons removed from the result screen', async () => {
  const result = readFileSync(new URL('../src/screens/Result.jsx', import.meta.url), 'utf8');
  const app = readFileSync(new URL('../src/App.jsx', import.meta.url), 'utf8');
  const api = readFileSync(new URL('../src/api.js', import.meta.url), 'utf8');
  assert.doesNotMatch(result, /Сохранить отчёт для разработчика/);
  assert.doesNotMatch(result, /Вернуть исходные страницы/);
  assert.doesNotMatch(app, /saveReport|undo_ask/);
  // The backend command stays: only the UI entry points are gone.
  assert.match(api, /saveDiagnostics = \(\) => call\('save_diagnostics'\)/);
  assert.deepEqual(await invoke('save_diagnostics'), { saved: true });
});

test('storage errors have their own texts', () => {
  assert.equal(reduce(initial, { type: 'report_saved', error: 'storage' }).notice, 'Не получилось сохранить отчёт');
  assert.equal(reduce(initial, { type: 'report_saved', error: null }).reportText, 'Отчёт сохранён');
  const ro = reduce(initial, { type: 'opened', reply: { error: 'storage' }, zip: true });
  assert.match(ro.error, /распаковать/);
  const plain = reduce(initial, { type: 'opened', reply: { error: 'storage' } });
  assert.doesNotMatch(plain.error, /распаковать/);
});

test('report.html write failure is shown', () => {
  assert.equal(reportFailedText({ report_failed: true }), 'Не получилось записать report.html');
  assert.equal(reportFailedText({ report_failed: false }), null);
  assert.equal(reportFailedText(null), null);
  const result = readFileSync(new URL('../src/screens/Result.jsx', import.meta.url), 'utf8');
  assert.match(result, /reportFailedText\(s\.report\)/);
});

test('saved-report text and rewrite notice clear on new run, undo and pick', () => {
  const dirty = {
    ...reduce(initial, { type: 'report_saved', error: null }),
    report: { reused: 2, rewrite: { busy: 1, skipped: 1 } },
    notice: 'x',
  };
  assert.equal(unrewrittenText(dirty.report), 'Не удалось обновить страниц: 2');
  for (const msg of [{ type: 'started' }, { type: 'undone', reply: { restored: 1, busy: 0, kept: 0 } }, { type: 'pick' }]) {
    const n = reduce(dirty, msg);
    assert.equal(n.reportText, '', msg.type);
    assert.equal(n.notice, '', msg.type);
    assert.equal(unrewrittenText(n.report), null, msg.type);
  }
  assert.equal(reusedText(reduce(dirty, { type: 'undone', reply: { restored: 1 } }).report), 'Взято из прошлого архива: 2');
});

test('too large pages note on Found', () => {
  assert.equal(tooLargeText({ too_large: 0 }), null);
  assert.equal(tooLargeText({ too_large: 2 }), 'Слишком большие страницы пропущены: 2');
});

test('undo counts skipped pages with the pages left alone', () => {
  const t = undoText({ restored: 1, busy: 0, kept: 1, skipped: 2 });
  assert.ok(t.includes('3 страницы изменены после vk-saver и оставлены как есть'), t);
  assert.ok(undoText({ restored: 0, busy: 0, kept: 0, skipped: 1 }).includes('1 страница'));
});

test('toggle all: all -> clear -> all, partial -> all', () => {
  const sum = { ...SUMMARY, chats: [chat(2000000101, 'A', 3), chat(-200003, 'B', 5), chat(7, 'Empty', 0)] };
  let s = reduce(initial, { type: 'scanned', reply: sum });
  s = reduce(s, { type: 'toggle_all' });
  assert.deepEqual([...s.excluded].sort(), ['-200003', '2000000101']);
  s = reduce(s, { type: 'toggle_all' });
  assert.deepEqual(s.excluded, []);
  s = reduce(s, { type: 'toggle', peer: 2000000101 });
  s = reduce(s, { type: 'toggle_all' });
  assert.deepEqual(s.excluded, []);
});

test('chat kind boundaries', () => {
  assert.equal(isGroupChat(1999999999), false);
  assert.equal(isGroupChat(2000000000), true);
  assert.equal(isGroupChat(-200003), false);
});

test('chat sorting keys', () => {
  const c = (peer, label, count, bytes, last) => ({ peer, label, photos: { count, bytes }, voice: { count: 1, bytes: 10 }, last });
  const sum = { chats: [c(1, 'Бэ', 10, 100, 3), c(2, 'А', 5, 500, 9), c(3, 'Вэ', 20, 50, 1)] };
  const peers = (sort) => chatRows(sum, sort).map((x) => x.peer);
  assert.deepEqual(peers('size'), [2, 1, 3]);
  assert.deepEqual(peers('messages'), [3, 1, 2]);
  assert.deepEqual(peers('date'), [2, 1, 3]);
  assert.deepEqual(peers('name'), [2, 1, 3]);
  assert.deepEqual(peers('bogus'), [2, 1, 3]); // неизвестный ключ = сортировка по умолчанию
});

test('toggle all with kind filter touches only shown chats', () => {
  const sum = { ...SUMMARY, chats: [chat(2000000101, 'G1', 3), chat(2000000102, 'G2', 4), chat(100002, 'U', 5), chat(-200003, 'C', 6)] };
  let s = reduce(initial, { type: 'scanned', reply: sum });
  s = reduce(s, { type: 'chat_kind', kind: 'group' });
  assert.deepEqual(shownChats(chatRows(sum), 'group').map((c) => c.peer).sort(), [2000000101, 2000000102]);
  assert.deepEqual(shownChats(chatRows(sum), 'user').map((c) => c.peer).sort((a, b) => a - b), [-200003, 100002]);
  s = reduce(s, { type: 'toggle_all' });
  assert.deepEqual([...s.excluded].sort(), ['2000000101', '2000000102']);
  assert.equal(allSelected([2000000101, 2000000102], s.excluded), false);
  s = reduce(s, { type: 'chat_kind', kind: 'user' });
  assert.equal(allSelected([100002, -200003], s.excluded), true);
  s = reduce(s, { type: 'toggle_all' });
  assert.deepEqual([...s.excluded].sort(), ['-200003', '100002', '2000000101', '2000000102']);
  s = reduce(s, { type: 'chat_kind', kind: 'group' });
  s = reduce(s, { type: 'toggle_all' });
  assert.deepEqual([...s.excluded].sort(), ['-200003', '100002']);
  s = reduce(s, { type: 'chat_kind', kind: 'all' });
  s = reduce(s, { type: 'toggle_all' });
  assert.deepEqual(s.excluded, []);
});
